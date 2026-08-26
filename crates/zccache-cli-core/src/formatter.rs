//! Daemon-free rustfmt format-cache API for embedding hosts.
//!
//! This module deliberately has no dependency on the standalone CLI lifecycle.
//! Hosts own child-process policy through [`run_rustfmt_cached_with_runner`].

use crate::core::NormalizedPath;
use std::path::Path;
use std::process::ExitCode;

/// Run rustfmt with format caching and the default child-process policy.
///
/// Explicitly non-recursive invocations can skip files whose content hash is
/// already known formatted. Recursive invocations always run rustfmt because
/// the explicit crate-root arguments do not describe child modules discovered
/// by rustfmt itself.
pub fn run_rustfmt_cached(
    rustfmt_path: &Path,
    args: &[String],
    cwd: &Path,
    cache_root: Option<&Path>,
) -> ExitCode {
    match run_rustfmt_cached_with_runner(rustfmt_path, args, cwd, cache_root, |cmd| {
        Ok(cmd.status()?.code().unwrap_or(1))
    }) {
        Ok(code) => exit_code_from_i32(code),
        Err(error) => {
            eprintln!("zccache: failed to run rustfmt: {error}");
            ExitCode::FAILURE
        }
    }
}

/// Run rustfmt with format caching while delegating child execution to `runner`.
///
/// The runner receives the exact command to execute and returns the exact child
/// exit status. This preserves host timeout, environment, and platform spawn
/// policy without entering zccache's CLI or daemon lifecycle.
pub fn run_rustfmt_cached_with_runner<F>(
    rustfmt_path: &Path,
    args: &[String],
    cwd: &Path,
    cache_root: Option<&Path>,
    runner: F,
) -> std::io::Result<i32>
where
    F: FnOnce(&mut std::process::Command) -> std::io::Result<i32>,
{
    use crate::compiler::parse_rustfmt::{find_rustfmt_config, parse_rustfmt_invocation};

    let parsed = match parse_rustfmt_invocation(args) {
        Some(parsed) => parsed,
        None => return run_direct(rustfmt_path, args, cwd, runner),
    };

    if !explicitly_skips_children(&parsed.flags) {
        return run_direct(rustfmt_path, args, cwd, runner);
    }

    let context_hash = {
        let mut hasher = crate::hash::StreamHasher::new();
        hasher.update(b"zccache-fmt-v2-nonrecursive");
        if let Ok(bin_hash) = crate::hash::hash_file(rustfmt_path) {
            hasher.update(bin_hash.as_bytes());
        } else {
            hasher.update(b"unknown-binary");
        }
        let config_path = parsed
            .config_path
            .clone()
            .or_else(|| find_rustfmt_config(cwd));
        if let Some(config_path) = config_path {
            if let Ok(config_hash) = crate::hash::hash_file(&config_path) {
                hasher.update(config_hash.as_bytes());
            }
        }
        for flag in &parsed.flags {
            hasher.update(flag.as_bytes());
            hasher.update(b"\0");
        }
        hasher.finalize().to_hex()
    };

    let cache_dir = cache_root
        .map(NormalizedPath::new)
        .unwrap_or_else(crate::core::config::default_cache_dir)
        .join("fmt")
        .join(&context_hash);
    let _ = std::fs::create_dir_all(&cache_dir);

    use rayon::prelude::*;
    let results: Vec<(NormalizedPath, bool, Option<crate::hash::ContentHash>)> = parsed
        .source_files
        .par_iter()
        .map(|source| {
            let absolute = if source.is_absolute() {
                source.clone()
            } else {
                cwd.join(source).into()
            };
            let (is_hit, hash) = match crate::hash::hash_file(&absolute) {
                Ok(content_hash) => {
                    let marker = cache_dir.join(content_hash.to_hex());
                    (marker.exists(), Some(content_hash))
                }
                Err(_) => (false, None),
            };
            (absolute, is_hit, hash)
        })
        .collect();

    let mut miss_files = Vec::new();
    for (absolute, is_hit, _) in &results {
        if !is_hit {
            miss_files.push(absolute.clone());
        }
    }
    if miss_files.is_empty() {
        return Ok(0);
    }

    let mut command = std::process::Command::new(rustfmt_path);
    command.args(&parsed.flags);
    for file in &miss_files {
        command.arg(file);
    }
    release_cwd_for_command(&mut command, cwd);
    let exit_code = runner(&mut command)?;

    if exit_code == 0 {
        for (absolute, was_hit, cached_hash) in results {
            if was_hit {
                continue;
            }
            let new_hash = if parsed.check_mode {
                cached_hash
            } else {
                crate::hash::hash_file(&absolute).ok()
            };
            if let Some(hash) = new_hash {
                let marker = cache_dir.join(hash.to_hex());
                let _ = std::fs::write(marker, b"");
            }
        }
    }
    Ok(exit_code)
}

fn run_direct<F>(
    rustfmt_path: &Path,
    args: &[String],
    cwd: &Path,
    runner: F,
) -> std::io::Result<i32>
where
    F: FnOnce(&mut std::process::Command) -> std::io::Result<i32>,
{
    let mut command = std::process::Command::new(rustfmt_path);
    command.args(args);
    release_cwd_for_command(&mut command, cwd);
    runner(&mut command)
}

/// Release the parent's build-directory handle while preserving the child's
/// requested cwd. On Windows this prevents the host's cwd from blocking tree
/// deletion after rustfmt returns (zccache#555).
fn release_cwd_for_command(command: &mut std::process::Command, child_cwd: &Path) {
    command.current_dir(child_cwd);
    let _ = std::env::set_current_dir(std::env::temp_dir());
}

fn exit_code_from_i32(code: i32) -> ExitCode {
    let truncated = (code & 0xFF) as u8;
    if code != 0 && truncated == 0 {
        ExitCode::from(1)
    } else {
        ExitCode::from(truncated)
    }
}

fn explicitly_skips_children(flags: &[String]) -> bool {
    let mut effective = None;
    let mut index = 0;
    while index < flags.len() {
        let flag = flags[index].as_str();
        if flag == "--config" {
            if let Some(value) = flags.get(index + 1) {
                if let Some(assignment) = config_skip_children_assignment(value) {
                    effective = Some(assignment);
                }
            }
            index += 2;
            continue;
        }
        if let Some(value) = flag.strip_prefix("--config=") {
            if let Some(assignment) = config_skip_children_assignment(value) {
                effective = Some(assignment);
            }
        }
        index += 1;
    }
    effective == Some(true)
}

fn config_skip_children_assignment(config: &str) -> Option<bool> {
    config.split(',').fold(None, |effective, entry| {
        let Some((key, value)) = entry.split_once('=') else {
            return effective;
        };
        if !key.trim().eq_ignore_ascii_case("skip_children") {
            return effective;
        }
        match value.trim().to_ascii_lowercase().as_str() {
            "true" => Some(true),
            "false" => Some(false),
            _ => effective,
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    static CWD_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    struct CwdRestore(Option<std::path::PathBuf>);

    impl Drop for CwdRestore {
        fn drop(&mut self) {
            if let Some(cwd) = self.0.take() {
                let _ = std::env::set_current_dir(cwd);
            }
        }
    }

    #[test]
    fn public_runner_controls_child_and_preserves_exact_exit_code() {
        let _lock = CWD_TEST_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let _cwd_restore = CwdRestore(std::env::current_dir().ok());
        let root = tempfile::tempdir().unwrap();
        let rustfmt = root.path().join("rustfmt-test-bin");
        let source = root.path().join("input.rs");
        std::fs::write(&rustfmt, b"fake formatter identity").unwrap();
        std::fs::write(&source, b"fn main( ) {}\n").unwrap();
        let args = vec![source.display().to_string()];
        let mut called = false;

        let code = run_rustfmt_cached_with_runner(
            &rustfmt,
            &args,
            root.path(),
            Some(&root.path().join("cache")),
            |command| {
                called = true;
                assert_eq!(command.get_program(), rustfmt.as_os_str());
                assert_eq!(command.get_current_dir(), Some(root.path()));
                assert!(command.get_args().any(|arg| arg == source.as_os_str()));
                Ok(37)
            },
        )
        .unwrap();

        assert!(called);
        assert_eq!(code, 37);
    }

    #[test]
    fn recursive_invocation_never_uses_root_only_marker() {
        let _lock = CWD_TEST_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let _cwd_restore = CwdRestore(std::env::current_dir().ok());
        let root = tempfile::tempdir().unwrap();
        let rustfmt = root.path().join("rustfmt-test-bin");
        let source_dir = root.path().join("src");
        let crate_root = source_dir.join("lib.rs");
        let child = source_dir.join("child.rs");
        let cache_root = root.path().join("cache");
        std::fs::create_dir_all(&source_dir).unwrap();
        std::fs::write(&rustfmt, b"fake formatter identity").unwrap();
        std::fs::write(&crate_root, b"mod child;\n").unwrap();
        std::fs::write(&child, b"pub fn child( ) {}\n").unwrap();
        let args = vec![crate_root.display().to_string()];

        let mut first_called = false;
        run_rustfmt_cached_with_runner(&rustfmt, &args, root.path(), Some(&cache_root), |_| {
            first_called = true;
            std::fs::write(&child, b"pub fn child() {}\n")?;
            Ok(0)
        })
        .unwrap();
        assert!(first_called);

        std::fs::write(&child, b"pub fn child( ) {}\n").unwrap();
        let mut second_called = false;
        run_rustfmt_cached_with_runner(&rustfmt, &args, root.path(), Some(&cache_root), |_| {
            second_called = true;
            std::fs::write(&child, b"pub fn child() {}\n")?;
            Ok(0)
        })
        .unwrap();

        assert!(
            second_called,
            "recursive rustfmt must re-check child modules"
        );
        assert_eq!(std::fs::read(&child).unwrap(), b"pub fn child() {}\n");
    }

    #[test]
    fn explicit_skip_children_can_use_content_marker() {
        let _lock = CWD_TEST_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let _cwd_restore = CwdRestore(std::env::current_dir().ok());
        let root = tempfile::tempdir().unwrap();
        let rustfmt = root.path().join("rustfmt-test-bin");
        let source = root.path().join("input.rs");
        let cache_root = root.path().join("cache");
        std::fs::write(&rustfmt, b"fake formatter identity").unwrap();
        std::fs::write(&source, b"fn main() {}\n").unwrap();
        let args = vec![
            "--config".to_owned(),
            "skip_children=true".to_owned(),
            source.display().to_string(),
        ];

        run_rustfmt_cached_with_runner(&rustfmt, &args, root.path(), Some(&cache_root), |_| Ok(0))
            .unwrap();
        let mut second_called = false;
        let code =
            run_rustfmt_cached_with_runner(&rustfmt, &args, root.path(), Some(&cache_root), |_| {
                second_called = true;
                Ok(0)
            })
            .unwrap();

        assert!(!second_called);
        assert_eq!(code, 0);
    }

    #[test]
    fn skip_children_config_parser_uses_last_assignment() {
        assert!(explicitly_skips_children(&[
            "--config".to_owned(),
            "max_width=100, skip_children = TRUE".to_owned(),
        ]));
        assert!(!explicitly_skips_children(&[
            "--config=skip_children=true,skip_children=false".to_owned(),
        ]));
    }
}
