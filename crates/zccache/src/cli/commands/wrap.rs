//! `zccache <compiler> ...` and `zccache wrap` — compiler/linker/archiver wrapping.
//!
//! Hosts the dispatch into compile / link / rustfmt / passthrough paths plus
//! the IPC client helpers for each.

use crate::compiler::strict_paths::StrictPathsMode;
use crate::core::NormalizedPath;
use std::path::Path;
use std::process::ExitCode;

use super::daemon::{ensure_daemon, which_on_path};
use super::util::{connect, exit_code_from_i32, resolve_endpoint, run_async, slurp_stdin_if_piped};

/// Run the compiler/tool directly without caching (ZCCACHE_DISABLE mode).
fn run_passthrough(args: &[String]) -> ExitCode {
    let tool = &args[0];
    let tool_args = if args.len() > 1 { &args[1..] } else { &[] };

    // Resolve the tool path (normalize MSYS paths, search PATH)
    let resolved = resolve_compiler_path(tool);

    match std::process::Command::new(&resolved)
        .args(tool_args)
        .status()
    {
        Ok(status) => exit_code_from_i32(status.code().unwrap_or(1)),
        Err(e) => {
            eprintln!("zccache: failed to run {}: {e}", resolved.display());
            ExitCode::FAILURE
        }
    }
}

// ─── Rustfmt caching ───────────────────────────────────────────────────────

/// Run rustfmt with format caching.
///
/// Files whose content hash is already in the format cache are skipped entirely,
/// preserving their mtime and avoiding unnecessary downstream rebuilds.
/// After formatting, the new content hash of each file is stored in the cache.
fn run_rustfmt_cached(rustfmt_path: &Path, args: &[String], cwd: &Path) -> ExitCode {
    use crate::compiler::parse_rustfmt::{find_rustfmt_config, parse_rustfmt_invocation};

    let parsed = match parse_rustfmt_invocation(args) {
        Some(p) => p,
        None => {
            // --help, --version, or stdin mode: pass through
            return run_tool_direct(rustfmt_path, args);
        }
    };

    // Build format context: rustfmt binary identity + config + flags.
    // Changes to any of these invalidate the entire format cache scope.
    let context_hash = {
        let mut hasher = crate::hash::StreamHasher::new();
        hasher.update(b"zccache-fmt-v1");

        // Hash rustfmt binary content for version identity
        if let Ok(bin_hash) = crate::hash::hash_file(rustfmt_path) {
            hasher.update(bin_hash.as_bytes());
        } else {
            hasher.update(b"unknown-binary");
        }

        // Hash config file content (if found)
        let config_path = parsed
            .config_path
            .clone()
            .or_else(|| find_rustfmt_config(cwd));
        if let Some(ref cfg) = config_path {
            if let Ok(cfg_hash) = crate::hash::hash_file(cfg) {
                hasher.update(cfg_hash.as_bytes());
            }
        }

        // Hash flags (edition, --check, etc.)
        for flag in &parsed.flags {
            hasher.update(flag.as_bytes());
            hasher.update(b"\0");
        }

        hasher.finalize().to_hex()
    };

    // Format cache directory: {cache_dir}/fmt/{context_hash}/
    let cache_dir = crate::core::config::default_cache_dir()
        .join("fmt")
        .join(&context_hash);

    // Ensure cache dir exists
    let _ = std::fs::create_dir_all(&cache_dir);

    // Resolve source files to absolute paths and check cache (parallel)
    use rayon::prelude::*;

    let results: Vec<(NormalizedPath, bool, Option<crate::hash::ContentHash>)> = parsed
        .source_files
        .par_iter()
        .map(|src| {
            let abs = if src.is_absolute() {
                src.clone()
            } else {
                cwd.join(src).into()
            };
            let (is_hit, hash) = match crate::hash::hash_file(&abs) {
                Ok(content_hash) => {
                    let marker = cache_dir.join(content_hash.to_hex());
                    (marker.exists(), Some(content_hash))
                }
                Err(_) => (false, None),
            };
            (abs, is_hit, hash)
        })
        .collect();

    let mut miss_files: Vec<NormalizedPath> = Vec::new();
    let mut all_files: Vec<(NormalizedPath, bool, Option<crate::hash::ContentHash>)> = Vec::new();
    for (abs, is_hit, hash) in results {
        if !is_hit {
            miss_files.push(abs.clone());
        }
        all_files.push((abs, is_hit, hash));
    }

    // All files are cache hits — skip rustfmt entirely (mtime preserved!)
    if miss_files.is_empty() {
        if parsed.check_mode {
            // --check: all files are known-formatted → exit 0
            return ExitCode::SUCCESS;
        }
        // Normal mode: all files already formatted → nothing to do
        return ExitCode::SUCCESS;
    }

    // Run rustfmt on miss files only (normal mode) or all files (--check mode)
    let exit_code = if parsed.check_mode {
        // --check mode: run on miss files only; if all would pass, we
        // already returned above. For misses, we must run to determine
        // if they're formatted.
        run_rustfmt_on_files(rustfmt_path, args, &miss_files, &parsed)
    } else {
        // Normal mode: run on miss files only
        run_rustfmt_on_files(rustfmt_path, args, &miss_files, &parsed)
    };

    let exit_i32 = match exit_code {
        Ok(code) => code,
        Err(e) => {
            eprintln!("zccache: failed to run rustfmt: {e}");
            return ExitCode::FAILURE;
        }
    };

    // On success (exit 0), store new content hashes in format cache
    if exit_i32 == 0 {
        // For --check mode with exit 0: the miss files were already formatted
        // (we just didn't know it). Reuse the hash from the lookup phase.
        // For normal mode with exit 0: files were reformatted. Must re-hash.
        for (abs, was_hit, cached_hash) in &all_files {
            if *was_hit {
                continue; // Already in cache
            }
            let new_hash = if parsed.check_mode {
                *cached_hash
            } else {
                crate::hash::hash_file(abs).ok()
            };
            if let Some(h) = new_hash {
                let marker = cache_dir.join(h.to_hex());
                let _ = std::fs::write(&marker, b"");
            }
        }
    }

    exit_code_from_i32(exit_i32)
}

/// Run rustfmt on a specific set of files, reconstructing the argument list.
fn run_rustfmt_on_files(
    rustfmt_path: &Path,
    original_args: &[String],
    files: &[NormalizedPath],
    parsed: &crate::compiler::parse_rustfmt::ParsedRustfmt,
) -> Result<i32, std::io::Error> {
    // Reconstruct args: flags + the miss files (not the original file list)
    let mut cmd = std::process::Command::new(rustfmt_path);
    cmd.args(&parsed.flags);
    for f in files {
        cmd.arg(f);
    }

    // Suppress original args' source files — we pass our filtered list above.
    // But we need to preserve any non-file, non-flag args. In practice,
    // flags + files covers everything.
    let _ = original_args; // intentionally unused — we reconstruct from parsed

    let status = cmd.status()?;
    Ok(status.code().unwrap_or(1))
}

/// Run a tool directly and return its exit code.
fn run_tool_direct(tool: &Path, args: &[String]) -> ExitCode {
    match std::process::Command::new(tool).args(args).status() {
        Ok(status) => exit_code_from_i32(status.code().unwrap_or(1)),
        Err(e) => {
            eprintln!("zccache: failed to run {}: {e}", tool.display());
            ExitCode::FAILURE
        }
    }
}

pub(crate) fn strip_leading_strict_paths_flags(
    args: &[String],
) -> Result<(Option<StrictPathsMode>, Vec<String>), String> {
    let mut strict_paths = None;
    let mut index = 0;

    while let Some(arg) = args.get(index) {
        if arg == "--strict-paths" {
            strict_paths = Some(StrictPathsMode::Absolute);
            index += 1;
        } else if let Some(value) = arg.strip_prefix("--strict-paths=") {
            strict_paths = Some(StrictPathsMode::parse(value).map_err(|err| err.to_string())?);
            index += 1;
        } else {
            break;
        }
    }

    Ok((strict_paths, args[index..].to_vec()))
}

pub(crate) fn parse_optional_strict_paths(
    value: Option<&str>,
) -> Result<Option<StrictPathsMode>, String> {
    value
        .map(|value| StrictPathsMode::parse(value).map_err(|err| err.to_string()))
        .transpose()
}

fn effective_strict_paths_mode(
    strict_paths_override: Option<StrictPathsMode>,
) -> Result<StrictPathsMode, String> {
    if let Some(mode) = strict_paths_override {
        return Ok(mode);
    }

    match std::env::var("ZCCACHE_STRICT_PATHS") {
        Ok(value) => StrictPathsMode::parse(&value).map_err(|err| err.to_string()),
        Err(std::env::VarError::NotPresent) => Ok(StrictPathsMode::Off),
        Err(std::env::VarError::NotUnicode(_)) => {
            Err("ZCCACHE_STRICT_PATHS is not valid Unicode".to_string())
        }
    }
}

fn set_client_env(env: &mut Vec<(String, String)>, key: &str, value: String) {
    if let Some((_, existing)) = env.iter_mut().find(|(env_key, _)| env_key == key) {
        *existing = value;
    } else {
        env.push((key.to_string(), value));
    }
}

/// Wrap a compiler or tool invocation.
///
/// `args` is the full command: ["clang++", "-c", "foo.cpp", "-o", "foo.o"]
/// or ["ar", "rcs", "libfoo.a", "a.o", "b.o"]
///
/// If the first arg is a known archiver (ar, llvm-ar, lib.exe), routes to
/// the link/archive path. Otherwise, routes to the compile path.
///
/// If ZCCACHE_SESSION_ID is set, uses that session and sends the tool
/// as a per-request override. If unset, auto-creates an ephemeral session.
pub(crate) fn run_wrap(
    args: &[String],
    strict_paths_override: Option<StrictPathsMode>,
) -> ExitCode {
    if args.is_empty() {
        eprintln!("usage: zccache <compiler|tool> <args...>");
        return ExitCode::FAILURE;
    }

    // ZCCACHE_DISABLE=1 — passthrough to compiler/tool without caching.
    if std::env::var("ZCCACHE_DISABLE").is_ok_and(|v| v == "1" || v.eq_ignore_ascii_case("true")) {
        return run_passthrough(args);
    }

    let strict_paths_mode = match effective_strict_paths_mode(strict_paths_override) {
        Ok(mode) => mode,
        Err(err) => {
            eprintln!("zccache: {err}");
            return ExitCode::FAILURE;
        }
    };

    // Normalize MSYS paths (e.g. /c/Users/... → C:\Users\...) on Windows,
    // then resolve to an absolute path so the daemon can find it.
    let wrapped_tool = resolve_compiler_path(&args[0]);
    let tool_args: Vec<String> = if args.len() > 1 {
        args[1..].to_vec()
    } else {
        Vec::new()
    };

    let cwd = std::env::current_dir().unwrap_or_default();

    let mut client_env: Vec<(String, String)> = std::env::vars().collect();
    if let Some(mode) = strict_paths_override {
        set_client_env(
            &mut client_env,
            "ZCCACHE_STRICT_PATHS",
            mode.as_str().to_string(),
        );
    }
    let endpoint = resolve_endpoint(None);

    // Release the CWD handle on the build directory. On Windows, a process's
    // CWD holds an implicit kernel handle that prevents the directory from
    // being deleted. We've captured everything we need into local variables.
    let _ = std::env::set_current_dir(std::env::temp_dir());

    // Check if this is a rustfmt invocation — handle via format cache path
    if crate::compiler::detect_family(&args[0]).is_formatter() {
        return run_rustfmt_cached(&wrapped_tool, &tool_args, &cwd);
    }

    // Check if this is an archiver or linker tool (including gcc -shared)
    if crate::compiler::parse_archiver::is_archiver(&args[0])
        || crate::compiler::parse_linker::is_link_invocation(&args[0], &tool_args)
    {
        return run_async(cmd_link_ephemeral(
            &endpoint,
            &wrapped_tool,
            tool_args,
            cwd.into(),
            client_env,
        ));
    }

    if let Err(err) = crate::compiler::strict_paths::validate_args(&tool_args, strict_paths_mode) {
        eprintln!("{}", err.diagnostic(&args[0], &tool_args));
        return ExitCode::FAILURE;
    }

    // Otherwise, treat as a compiler invocation
    match std::env::var("ZCCACHE_SESSION_ID") {
        Ok(session_id) => {
            if session_id.is_empty() {
                eprintln!("ZCCACHE_SESSION_ID is empty");
                return ExitCode::FAILURE;
            }
            run_async(cmd_compile(
                &endpoint,
                &session_id,
                tool_args,
                cwd.into(),
                wrapped_tool,
                client_env,
            ))
        }
        Err(_) => {
            // No session — auto-create an ephemeral one for this compilation.
            run_async(cmd_compile_ephemeral(
                &endpoint,
                &wrapped_tool,
                tool_args,
                cwd.into(),
                client_env,
            ))
        }
    }
}

/// Resolve a compiler name/path to an absolute path.
/// Normalizes MSYS paths on Windows, then searches PATH if not already absolute.
fn resolve_compiler_path(compiler: &str) -> NormalizedPath {
    let normalized = crate::core::path::normalize_msys_path(compiler);
    let path = Path::new(&normalized);

    // Already absolute — return as-is.
    if path.is_absolute() {
        return normalized.into();
    }

    // Search PATH for the compiler.
    match which_on_path(&normalized) {
        Some(abs) => abs,
        None => normalized.into(), // Let the daemon report the error.
    }
}

async fn cmd_compile(
    endpoint: &str,
    session_id: &str,
    args: Vec<String>,
    cwd: NormalizedPath,
    compiler: NormalizedPath,
    client_env: Vec<(String, String)>,
) -> ExitCode {
    let stdin_bytes = slurp_stdin_if_piped();
    let mut conn = match connect(endpoint).await {
        Ok(c) => c,
        Err(e) => {
            eprintln!("zccache[err][C]: cannot connect to daemon at {endpoint}: {e}");
            return ExitCode::FAILURE;
        }
    };

    if let Err(e) = conn
        .send(&crate::protocol::Request::Compile {
            session_id: session_id.to_string(),
            args,
            cwd,
            compiler,
            env: Some(client_env),
            stdin: stdin_bytes,
        })
        .await
    {
        eprintln!("zccache[err][S]: failed to send to daemon: {e}");
        return ExitCode::FAILURE;
    }

    let recv_result = match conn.recv().await {
        Ok(r) => r,
        Err(e) => {
            eprintln!("zccache[err][R]: broken connection to daemon: {e}");
            return ExitCode::FAILURE;
        }
    };
    match recv_result {
        Some(crate::protocol::Response::CompileResult {
            exit_code,
            stdout,
            stderr,
            ..
        }) => {
            // Relay compiler output
            use std::io::Write;
            let _ = std::io::stdout().write_all(&stdout);
            let _ = std::io::stderr().write_all(&stderr);
            exit_code_from_i32(exit_code)
        }
        Some(crate::protocol::Response::Error { message }) => {
            eprintln!("zccache[err][E]: daemon error: {message}");
            ExitCode::FAILURE
        }
        None => {
            eprintln!("zccache[err][R]: lost connection to daemon (no response). Often a daemon-CLI protocol version mismatch — try `zccache stop`");
            ExitCode::FAILURE
        }
        Some(other) => {
            eprintln!("zccache[err][U]: unexpected response from daemon: {other:?}");
            ExitCode::FAILURE
        }
    }
}

/// Ephemeral session: single-roundtrip compile (session start + compile + session end
/// in one IPC message). Used when ZCCACHE_SESSION_ID is not set (drop-in mode).
async fn cmd_compile_ephemeral(
    endpoint: &str,
    compiler: &Path,
    args: Vec<String>,
    cwd: NormalizedPath,
    client_env: Vec<(String, String)>,
) -> ExitCode {
    // Ensure daemon is running and version-compatible.
    if let Err(e) = ensure_daemon(endpoint).await {
        eprintln!("zccache[err][D]: cannot start daemon at {endpoint}: {e}");
        return ExitCode::FAILURE;
    }
    let mut conn = match connect(endpoint).await {
        Ok(c) => c,
        Err(e) => {
            eprintln!("zccache[err][C]: cannot connect to daemon at {endpoint}: {e}");
            return ExitCode::FAILURE;
        }
    };

    let stdin_bytes = slurp_stdin_if_piped();
    if let Err(e) = conn
        .send(&crate::protocol::Request::CompileEphemeral {
            client_pid: std::process::id(),
            working_dir: cwd.clone(),
            compiler: compiler.into(),
            args,
            cwd,
            env: Some(client_env),
            stdin: stdin_bytes,
        })
        .await
    {
        eprintln!("zccache[err][S]: failed to send to daemon: {e}");
        return ExitCode::FAILURE;
    }

    let recv_result = match conn.recv().await {
        Ok(r) => r,
        Err(e) => {
            eprintln!("zccache[err][R]: broken connection to daemon: {e}");
            return ExitCode::FAILURE;
        }
    };
    match recv_result {
        Some(crate::protocol::Response::CompileResult {
            exit_code,
            stdout,
            stderr,
            ..
        }) => {
            use std::io::Write;
            let _ = std::io::stdout().write_all(&stdout);
            let _ = std::io::stderr().write_all(&stderr);
            exit_code_from_i32(exit_code)
        }
        Some(crate::protocol::Response::Error { message }) => {
            eprintln!("zccache[err][E]: daemon error: {message}");
            ExitCode::FAILURE
        }
        None => {
            eprintln!("zccache[err][R]: lost connection to daemon (no response). Often a daemon-CLI protocol version mismatch — try `zccache stop`");
            ExitCode::FAILURE
        }
        Some(other) => {
            eprintln!("zccache[err][U]: unexpected response from daemon: {other:?}");
            ExitCode::FAILURE
        }
    }
}

/// Ephemeral link/archive: single-roundtrip for `zccache ar ...` etc.
async fn cmd_link_ephemeral(
    endpoint: &str,
    tool: &Path,
    args: Vec<String>,
    cwd: NormalizedPath,
    client_env: Vec<(String, String)>,
) -> ExitCode {
    if let Err(e) = ensure_daemon(endpoint).await {
        eprintln!("zccache[err][D]: cannot start daemon at {endpoint}: {e}");
        return ExitCode::FAILURE;
    }
    let mut conn = match connect(endpoint).await {
        Ok(c) => c,
        Err(e) => {
            eprintln!("zccache[err][C]: cannot connect to daemon at {endpoint}: {e}");
            return ExitCode::FAILURE;
        }
    };

    if let Err(e) = conn
        .send(&crate::protocol::Request::LinkEphemeral {
            client_pid: std::process::id(),
            tool: tool.into(),
            args,
            cwd,
            env: Some(client_env),
        })
        .await
    {
        eprintln!("zccache[err][S]: failed to send to daemon: {e}");
        return ExitCode::FAILURE;
    }

    let recv_result = match conn.recv().await {
        Ok(r) => r,
        Err(e) => {
            eprintln!("zccache[err][R]: broken connection to daemon: {e}");
            return ExitCode::FAILURE;
        }
    };
    match recv_result {
        Some(crate::protocol::Response::LinkResult {
            exit_code,
            stdout,
            stderr,
            warning,
            ..
        }) => {
            use std::io::Write;
            let _ = std::io::stdout().write_all(&stdout);
            let _ = std::io::stderr().write_all(&stderr);
            if let Some(w) = warning {
                eprintln!("zccache warning: {w}");
            }
            exit_code_from_i32(exit_code)
        }
        Some(crate::protocol::Response::Error { message }) => {
            eprintln!("zccache[err][E]: daemon error: {message}");
            ExitCode::FAILURE
        }
        None => {
            eprintln!("zccache[err][R]: lost connection to daemon (no response). Often a daemon-CLI protocol version mismatch — try `zccache stop`");
            ExitCode::FAILURE
        }
        Some(other) => {
            eprintln!("zccache[err][U]: unexpected response from daemon: {other:?}");
            ExitCode::FAILURE
        }
    }
}
