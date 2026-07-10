//! `zccache exec` — generic tool caching (issue #272).
//!
//! Builds a `Request::GenericToolExec`, sends it to the daemon, writes the
//! tool's stdout/stderr to this process's streams, and exits with the cached
//! (or fresh) exit code. On cache hit the tool is not spawned at all.

use std::process::ExitCode;
use std::sync::Arc;

use crate::core::NormalizedPath;
use crate::protocol::{ExecCachePolicy, ExecOutputStreams, Request, Response};

use super::daemon::ensure_daemon;
use super::util::{absolute_path, connect, exit_code_from_i32, resolve_endpoint};

/// Parsed view of `--input-file`, `--input-env`, etc. — pre-validates argv
/// before sending the IPC request so misuse errors render before reaching the
/// daemon.
pub(crate) struct ExecParams {
    pub(crate) input_files: Vec<String>,
    /// Issue #837: newline-delimited file of additional input-file paths.
    pub(crate) input_file_list: Option<String>,
    /// Issue #837: read additional input-file paths from stdin.
    pub(crate) input_file_stdin: bool,
    pub(crate) input_env: Vec<String>,
    pub(crate) input_extra: Option<String>,
    pub(crate) output_stdout: bool,
    pub(crate) output_stderr: bool,
    pub(crate) output_files: Vec<String>,
    pub(crate) tool_hash: Option<String>,
    pub(crate) no_cache: bool,
    pub(crate) no_cwd_in_key: bool,
    pub(crate) endpoint: Option<String>,
    pub(crate) tool_command: Vec<String>,
    /// Path A — file(s) to scan for `#include` directives.
    pub(crate) include_scan: Vec<String>,
    /// `-I` user include directories used by the scan.
    pub(crate) include_dir: Vec<String>,
    /// `-isystem` system include directories used by the scan.
    pub(crate) system_include: Vec<String>,
    /// `-iquote` quoted-only include directories used by the scan.
    pub(crate) iquote_dir: Vec<String>,
    /// Path B — depfile the tool emits.
    pub(crate) depfile: Option<String>,
    /// Mark the run as non-deterministic (no caching).
    pub(crate) non_deterministic: bool,
    /// Regex patterns whose matches are removed from the cache-key arg list.
    pub(crate) key_args_filter: Vec<String>,
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn cmd_exec(params: ExecParams) -> ExitCode {
    let ExecParams {
        input_files,
        input_file_list,
        input_file_stdin,
        input_env,
        input_extra,
        output_stdout,
        output_stderr,
        output_files,
        tool_hash,
        no_cache,
        no_cwd_in_key,
        endpoint,
        tool_command,
        include_scan,
        include_dir,
        system_include,
        iquote_dir,
        depfile,
        non_deterministic,
        key_args_filter,
    } = params;

    if tool_command.is_empty() {
        eprintln!(
            "zccache exec: expected `--` followed by the tool command\n\
             example: zccache exec --input-file src/foo.cpp -- fastled-lint src/foo.cpp"
        );
        return ExitCode::from(2);
    }

    let tool_str = &tool_command[0];
    let tool_args: Vec<String> = tool_command[1..].to_vec();

    let tool_resolved: NormalizedPath = match resolve_tool_path(tool_str) {
        Some(p) => p,
        None => {
            eprintln!(
                "zccache exec: tool not found: {tool_str} (PATH lookup failed and the value is not an absolute path)"
            );
            return ExitCode::from(127);
        }
    };

    // Snapshot only the declared env vars into the request — the daemon
    // refuses to import the rest of the process env so the cache key
    // depends only on what the caller declared.
    let mut env_pairs: Vec<(String, String)> = Vec::with_capacity(input_env.len());
    for name in &input_env {
        let value = std::env::var(name).unwrap_or_default();
        env_pairs.push((name.clone(), value));
    }

    let cwd_norm: NormalizedPath = std::env::current_dir().unwrap_or_default().into();

    // Issue #837: expand `--input-file-list` / `--input-file-stdin` into the
    // same path set as repeated `--input-file`. This is purely a delivery
    // mechanism — the paths join `input_files` before absolutization, so the
    // cache key is byte-identical to spelling every path on the command line.
    let mut input_files = input_files;
    if let Some(list_path) = input_file_list.as_deref() {
        match std::fs::read_to_string(list_path) {
            Ok(contents) => input_files.extend(parse_input_path_lines(&contents)),
            Err(e) => {
                eprintln!("zccache exec: failed to read --input-file-list {list_path}: {e}");
                return ExitCode::from(2);
            }
        }
    }
    if input_file_stdin {
        use std::io::Read as _;
        let mut buf = String::new();
        if let Err(e) = std::io::stdin().read_to_string(&mut buf) {
            eprintln!("zccache exec: failed to read --input-file-stdin: {e}");
            return ExitCode::from(2);
        }
        input_files.extend(parse_input_path_lines(&buf));
    }

    let input_file_paths: Vec<NormalizedPath> =
        input_files.iter().map(|p| absolute_path(p)).collect();

    // Output paths can be relative — daemon absolutizes against cwd. We keep
    // them as the user typed them so the cache `outputs[].name` matches what
    // the caller asked for (the daemon does its own absolutization for the
    // disk read/write).
    let output_file_paths: Vec<NormalizedPath> =
        output_files.iter().map(NormalizedPath::from).collect();

    let parsed_tool_hash: Option<[u8; 32]> = match tool_hash.as_deref() {
        Some(hex) => match parse_hex_32(hex) {
            Some(bytes) => Some(bytes),
            None => {
                eprintln!(
                    "zccache exec: --tool-hash must be 64 hex characters (32 bytes); got {} chars",
                    hex.len()
                );
                return ExitCode::from(2);
            }
        },
        None => None,
    };

    let extra_bytes = Arc::new(input_extra.map(String::into_bytes).unwrap_or_default());

    let include_scan_files: Vec<NormalizedPath> =
        include_scan.iter().map(|p| absolute_path(p)).collect();
    let include_dirs: Vec<NormalizedPath> = include_dir.iter().map(|p| absolute_path(p)).collect();
    let system_include_dirs: Vec<NormalizedPath> =
        system_include.iter().map(|p| absolute_path(p)).collect();
    let iquote_dirs: Vec<NormalizedPath> = iquote_dir.iter().map(|p| absolute_path(p)).collect();
    let depfile_path: Option<NormalizedPath> = depfile.as_deref().map(absolute_path);

    let request = Request::GenericToolExec {
        tool: tool_resolved,
        args: tool_args,
        cwd: cwd_norm,
        env: env_pairs,
        input_files: input_file_paths,
        input_extra: extra_bytes,
        output_streams: ExecOutputStreams {
            stdout: output_stdout,
            stderr: output_stderr,
        },
        output_files: output_file_paths,
        tool_hash: parsed_tool_hash,
        cache_policy: if no_cache {
            ExecCachePolicy::Bypass
        } else {
            ExecCachePolicy::Normal
        },
        cwd_in_key: !no_cwd_in_key,
        include_scan_files,
        include_dirs,
        system_include_dirs,
        iquote_dirs,
        depfile: depfile_path,
        non_deterministic,
        key_args_filter,
    };

    let endpoint = resolve_endpoint(endpoint.as_deref());

    super::util::run_async(async move {
        if let Err(e) = ensure_daemon(&endpoint).await {
            eprintln!("zccache exec: failed to start daemon: {e}");
            return ExitCode::from(2);
        }
        let mut conn = match connect(&endpoint).await {
            Ok(c) => c,
            Err(e) => {
                eprintln!("zccache exec: cannot connect to daemon: {e}");
                return ExitCode::from(2);
            }
        };

        let wire = crate::protocol::wire_prost::full_family_wire_format_from_env();
        if let Err(e) = conn.send_request(&request, wire).await {
            eprintln!("zccache exec: send error: {e}");
            return ExitCode::from(2);
        }

        match conn.recv_response().await {
            Ok(Some(Response::GenericToolExecResult {
                exit_code,
                stdout,
                stderr,
                cached,
                cache_key_hex,
                ..
            })) => {
                use std::io::Write;
                let _ = std::io::stdout().write_all(&stdout);
                let _ = std::io::stderr().write_all(&stderr);
                tracing::debug!(%cache_key_hex, %cached, "zccache exec result");
                exit_code_from_i32(exit_code)
            }
            Ok(Some(Response::Error { message })) => {
                eprintln!("zccache exec: daemon error: {message}");
                ExitCode::from(2)
            }
            Ok(other) => {
                eprintln!("zccache exec: unexpected response: {other:?}");
                ExitCode::from(2)
            }
            Err(e) => {
                eprintln!("zccache exec: recv error: {e}");
                ExitCode::from(2)
            }
        }
    })
}

/// Resolve `tool` to an absolute path. If `tool` already contains a
/// directory separator or is absolute, use as-is; otherwise walk PATH.
fn resolve_tool_path(tool: &str) -> Option<NormalizedPath> {
    let p = std::path::Path::new(tool);
    if p.is_absolute() || p.components().count() > 1 {
        if p.is_file() {
            return Some(p.into());
        }
        // Even when it has separators we still want to absolutize relative
        // paths against cwd so the daemon doesn't re-interpret them.
        return Some(absolute_path(tool));
    }
    let path_env = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_env) {
        let candidate = dir.join(tool);
        if candidate.is_file() {
            return Some(candidate.into());
        }
        // Windows: try common executable extensions when the user didn't
        // type one. Mirrors what cmd.exe / PowerShell expand.
        #[cfg(windows)]
        {
            for ext in [".exe", ".bat", ".cmd"] {
                let with_ext = dir.join(format!("{tool}{ext}"));
                if with_ext.is_file() {
                    return Some(with_ext.into());
                }
            }
        }
    }
    None
}

fn parse_hex_32(s: &str) -> Option<[u8; 32]> {
    if s.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, byte) in out.iter_mut().enumerate() {
        let start = i * 2;
        *byte = u8::from_str_radix(&s[start..start + 2], 16).ok()?;
    }
    Some(out)
}

/// Issue #837: split a newline-delimited path list (from `--input-file-list`
/// or `--input-file-stdin`) into individual paths. Trailing whitespace and
/// `\r` (Windows CRLF) are trimmed and blank lines dropped so a trailing
/// newline or a hand-edited list doesn't inject empty paths. Every surviving
/// line is treated exactly as one `--input-file` value — no comment syntax,
/// so a literal `#foo` path is preserved.
fn parse_input_path_lines(contents: &str) -> Vec<String> {
    contents
        .lines()
        .map(|line| line.trim_end_matches(['\r', '\n']).trim_end())
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_hex_32_round_trip() {
        let bytes: [u8; 32] = std::array::from_fn(|i| i as u8);
        let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(parse_hex_32(&hex), Some(bytes));
    }

    #[test]
    fn parse_hex_32_rejects_wrong_length() {
        assert!(parse_hex_32("deadbeef").is_none());
        assert!(parse_hex_32(&"a".repeat(65)).is_none());
    }

    #[test]
    fn parse_hex_32_rejects_non_hex() {
        let mut bad = "0".repeat(64);
        bad.replace_range(0..1, "z");
        assert!(parse_hex_32(&bad).is_none());
    }

    #[test]
    fn parse_input_path_lines_splits_and_trims() {
        let listing = "src/a.h\nsrc/b.h\r\n  src/c.h  \n\n\tsrc/d.h\n";
        assert_eq!(
            parse_input_path_lines(listing),
            vec!["src/a.h", "src/b.h", "  src/c.h", "\tsrc/d.h"],
        );
    }

    #[test]
    fn parse_input_path_lines_drops_blanks_and_keeps_hash_paths() {
        // Trailing newline / blank lines must not inject empty entries, and a
        // leading '#' is a real path segment, not a comment.
        assert!(parse_input_path_lines("\n\n   \n").is_empty());
        assert_eq!(
            parse_input_path_lines("#notacomment.h\n"),
            vec!["#notacomment.h"]
        );
    }

    #[test]
    fn parse_input_path_lines_empty_input_is_empty() {
        assert!(parse_input_path_lines("").is_empty());
    }
}
