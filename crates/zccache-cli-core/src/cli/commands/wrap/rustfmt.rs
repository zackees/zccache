//! Rustfmt format-cache wrapper path.

use crate::core::NormalizedPath;
use std::path::Path;
use std::process::ExitCode;

use super::super::util::exit_code_from_i32;
/// Run rustfmt with format caching.
///
/// Files whose content hash is already in the format cache are skipped entirely,
/// preserving their mtime and avoiding unnecessary downstream rebuilds. After
/// formatting, the new content hash of each file is stored in the cache.
pub(super) fn run_rustfmt_cached(
    rustfmt_path: &Path,
    args: &[String],
    cwd: &Path,
    cache_root: Option<&Path>,
) -> ExitCode {
    match run_rustfmt_cached_with_runner(rustfmt_path, args, cwd, cache_root, |cmd| {
        Ok(cmd.status()?.code().unwrap_or(1))
    }) {
        Ok(code) => exit_code_from_i32(code),
        Err(e) => {
            eprintln!("zccache: failed to run rustfmt: {e}");
            ExitCode::FAILURE
        }
    }
}

pub(super) fn run_rustfmt_cached_with_runner<F>(
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
        Some(p) => p,
        None => {
            // --help, --version, or stdin mode: pass through.
            let mut cmd = std::process::Command::new(rustfmt_path);
            cmd.args(args);
            super::passthrough::release_cwd_for_command(&mut cmd, cwd);
            return runner(&mut cmd);
        }
    };

    // Build format context: rustfmt binary identity + config + flags.
    // Changes to any of these invalidate the entire format cache scope.
    let context_hash = {
        let mut hasher = crate::hash::StreamHasher::new();
        hasher.update(b"zccache-fmt-v1");

        if let Ok(bin_hash) = crate::hash::hash_file(rustfmt_path) {
            hasher.update(bin_hash.as_bytes());
        } else {
            hasher.update(b"unknown-binary");
        }

        let config_path = parsed
            .config_path
            .clone()
            .or_else(|| find_rustfmt_config(cwd));
        if let Some(ref cfg) = config_path {
            if let Ok(cfg_hash) = crate::hash::hash_file(cfg) {
                hasher.update(cfg_hash.as_bytes());
            }
        }

        for flag in &parsed.flags {
            hasher.update(flag.as_bytes());
            hasher.update(b"\0");
        }

        hasher.finalize().to_hex()
    };

    let cache_dir = cache_root
        .map(crate::core::NormalizedPath::new)
        .unwrap_or_else(crate::core::config::default_cache_dir)
        .join("fmt")
        .join(&context_hash);

    let _ = std::fs::create_dir_all(&cache_dir);

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

    if miss_files.is_empty() {
        return Ok(0);
    }

    let exit_i32 = run_rustfmt_on_files(rustfmt_path, args, cwd, &miss_files, &parsed, runner)?;

    if exit_i32 == 0 {
        for (abs, was_hit, cached_hash) in &all_files {
            if *was_hit {
                continue;
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

    Ok(exit_i32)
}

fn run_rustfmt_on_files<F>(
    rustfmt_path: &Path,
    original_args: &[String],
    cwd: &Path,
    files: &[NormalizedPath],
    parsed: &crate::compiler::parse_rustfmt::ParsedRustfmt,
    runner: F,
) -> Result<i32, std::io::Error>
where
    F: FnOnce(&mut std::process::Command) -> std::io::Result<i32>,
{
    let mut cmd = std::process::Command::new(rustfmt_path);
    cmd.args(&parsed.flags);
    for f in files {
        cmd.arg(f);
    }
    super::passthrough::release_cwd_for_command(&mut cmd, cwd);

    let _ = original_args;

    runner(&mut cmd)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct CwdRestore(Option<std::path::PathBuf>);

    impl Drop for CwdRestore {
        fn drop(&mut self) {
            if let Some(cwd) = self.0.take() {
                let _ = std::env::set_current_dir(cwd);
            }
        }
    }

    #[test]
    fn embedded_runner_controls_child_and_preserves_exact_exit_code() {
        let _lock = super::super::passthrough::CWD_TEST_LOCK
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
}
