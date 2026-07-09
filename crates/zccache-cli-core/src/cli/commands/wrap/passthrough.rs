//! Direct execution paths used when wrapper caching is disabled or unsupported.

use std::path::Path;
use std::process::ExitCode;

use super::super::util::exit_code_from_i32;
use super::tool_resolution::resolve_compiler_path;

/// Release the wrapper's own CWD handle on the build dir before spawning
/// a child, while keeping the child's CWD pointing at the original
/// directory so relative paths in argv still resolve.
///
/// Issue #555: in the `ZCCACHE_DISABLE` / unsupported-tool early-exit
/// paths the wrapper bypasses the chdir-to-temp at `wrap.rs:59`. On
/// Windows the parent's CWD holds an implicit kernel handle on the
/// build directory, blocking `shutil.rmtree` until the wrapper exits.
/// This helper restores parity with the cached-path behavior.
fn run_with_released_cwd(
    cmd: &mut std::process::Command,
) -> std::io::Result<std::process::ExitStatus> {
    if let Ok(cwd) = std::env::current_dir() {
        cmd.current_dir(&cwd);
        // Release the wrapper's own CWD handle before spawning. The
        // child inherits `cmd.current_dir(...)` regardless of where the
        // parent ends up, so argv-relative paths still resolve from
        // the build dir.
        let _ = std::env::set_current_dir(std::env::temp_dir());
    }
    cmd.status()
}

/// Run the compiler/tool directly without caching (`ZCCACHE_DISABLE` mode).
pub(super) fn run_passthrough(args: &[String]) -> ExitCode {
    let tool = &args[0];
    let tool_args = args.get(1..).unwrap_or(&[]);
    let resolved = resolve_compiler_path(tool);

    let mut cmd = std::process::Command::new(&resolved);
    cmd.args(tool_args);
    match run_with_released_cwd(&mut cmd) {
        Ok(status) => exit_code_from_i32(status.code().unwrap_or(1)),
        Err(e) => {
            eprintln!("zccache: failed to run {}: {e}", resolved.display());
            ExitCode::FAILURE
        }
    }
}

/// Run a tool directly and return its exit code.
pub(super) fn run_tool_direct(tool: &Path, args: &[String]) -> ExitCode {
    let mut cmd = std::process::Command::new(tool);
    cmd.args(args);
    match run_with_released_cwd(&mut cmd) {
        Ok(status) => exit_code_from_i32(status.code().unwrap_or(1)),
        Err(e) => {
            eprintln!("zccache: failed to run {}: {e}", tool.display());
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Process-global lock so the two CWD-mutating tests don't race.
    /// `env::set_current_dir` is process-wide; running these in parallel
    /// would produce nondeterministic results.
    static CWD_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn noop_tool() -> std::path::PathBuf {
        if cfg!(windows) {
            std::path::PathBuf::from("cmd.exe")
        } else {
            std::path::PathBuf::from("true")
        }
    }

    fn noop_args() -> Vec<String> {
        if cfg!(windows) {
            vec!["/c".to_string(), "exit".to_string(), "0".to_string()]
        } else {
            Vec::new()
        }
    }

    /// Issue #555: `run_passthrough` must release the wrapper's CWD
    /// before/while spawning the child, so the build dir is not held
    /// by the wrapper's kernel CWD handle on Windows. Verified by
    /// asserting `env::current_dir()` no longer points at the build
    /// dir after the helper returns.
    #[test]
    fn run_passthrough_releases_wrapper_cwd() {
        let _guard = CWD_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let original_cwd = std::env::current_dir().ok();
        let build_dir = tempfile::tempdir().unwrap();
        let canonical_build_dir = std::fs::canonicalize(build_dir.path()).unwrap();
        std::env::set_current_dir(&canonical_build_dir).unwrap();

        let mut args = vec![noop_tool().to_string_lossy().into_owned()];
        args.extend(noop_args());
        let _ = run_passthrough(&args);

        let after = std::env::current_dir().unwrap();
        // `tempfile`'s tempdir under `%TEMP%` would itself canonicalize
        // to the same path as `canonical_build_dir` on weird CI
        // configurations, so compare canonicalized forms.
        let after_canonical = std::fs::canonicalize(&after).unwrap_or(after);
        assert_ne!(
            after_canonical, canonical_build_dir,
            "issue #555: run_passthrough must release the wrapper's CWD \
             before returning so the build dir is not pinned by the wrapper's \
             kernel handle on Windows",
        );

        // Restore CWD so the rest of the test process is unaffected.
        if let Some(cwd) = original_cwd {
            let _ = std::env::set_current_dir(cwd);
        }
    }

    /// `run_tool_direct` (used by the rustfmt help/version/stdin early
    /// exit) must also release the wrapper's CWD — same correctness
    /// rationale as `run_passthrough`.
    #[test]
    fn run_tool_direct_releases_wrapper_cwd() {
        let _guard = CWD_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let original_cwd = std::env::current_dir().ok();
        let build_dir = tempfile::tempdir().unwrap();
        let canonical_build_dir = std::fs::canonicalize(build_dir.path()).unwrap();
        std::env::set_current_dir(&canonical_build_dir).unwrap();

        let tool = noop_tool();
        let args: Vec<String> = noop_args();
        let _ = run_tool_direct(&tool, &args);

        let after = std::env::current_dir().unwrap();
        let after_canonical = std::fs::canonicalize(&after).unwrap_or(after);
        assert_ne!(
            after_canonical, canonical_build_dir,
            "issue #555: run_tool_direct must release the wrapper's CWD",
        );

        if let Some(cwd) = original_cwd {
            let _ = std::env::set_current_dir(cwd);
        }
    }
}
