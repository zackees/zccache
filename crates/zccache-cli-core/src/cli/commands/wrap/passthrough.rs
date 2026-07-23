//! Direct execution paths used when wrapper caching is disabled or unsupported.

use std::io::Write;
use std::path::Path;
use std::process::ExitCode;

use super::super::util::exit_code_from_i32;
use super::tool_resolution::resolve_compiler_path;

#[cfg(test)]
pub(super) static CWD_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Release the wrapper's own CWD handle on the build dir before spawning
/// a child, while keeping the child's CWD pointing at the original
/// directory so relative paths in argv still resolve.
///
/// Issue #555: in the `ZCCACHE_DISABLE` / unsupported-tool early-exit
/// paths the wrapper bypasses the chdir-to-temp at `wrap.rs:59`. On
/// Windows the parent's CWD holds an implicit kernel handle on the
/// build directory, blocking `shutil.rmtree` until the wrapper exits.
/// This helper restores parity with the cached-path behavior.
pub(super) fn release_cwd_for_command(cmd: &mut std::process::Command, child_cwd: &Path) {
    cmd.current_dir(child_cwd);
    // Release the wrapper's own CWD handle before spawning. The child inherits
    // `cmd.current_dir(...)` regardless of where the parent ends up, so
    // argv-relative paths still resolve from the caller-supplied directory.
    let _ = std::env::set_current_dir(std::env::temp_dir());
}

fn run_with_released_cwd(
    cmd: &mut std::process::Command,
) -> std::io::Result<std::process::ExitStatus> {
    if let Ok(cwd) = std::env::current_dir() {
        release_cwd_for_command(cmd, &cwd);
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

/// Run the wrapped tool directly after a daemon failure that is known to have
/// happened before request dispatch. The caller must not use this for a
/// transport failure after a request may have reached the daemon: that would
/// allow two compiler processes to write the same outputs.
pub(super) fn run_locally(
    tool: &Path,
    args: &[String],
    cwd: &Path,
    env: &[(String, String)],
    stdin_bytes: &[u8],
    reason: &str,
) -> ExitCode {
    run_locally_with_lifecycle_root(tool, args, cwd, env, stdin_bytes, reason, None)
}

fn run_locally_with_lifecycle_root(
    tool: &Path,
    args: &[String],
    cwd: &Path,
    env: &[(String, String)],
    stdin_bytes: &[u8],
    reason: &str,
    lifecycle_root: Option<&Path>,
) -> ExitCode {
    let warning = format!(
        "zccache[warn][F]: {reason}; running {} directly, uncached\n",
        tool.display(),
    );
    let _ = super::write_wrapper_warning_line(
        &mut std::io::stderr(),
        warning.as_bytes(),
        super::wrapper_stderr_color_enabled(),
    );
    let event = serde_json::json!({
        "tool": tool.to_string_lossy(),
        "cwd": cwd.to_string_lossy(),
        "reason": reason,
        "phase": "pre-dispatch",
        "route": "wrapper",
    });
    if let Some(root) = lifecycle_root {
        crate::core::lifecycle::write_event_in_cache_root(
            root,
            crate::core::lifecycle::EVENT_WRAPPER_LOCAL_FALLBACK,
            event,
        );
    } else {
        crate::core::lifecycle::write_event(
            crate::core::lifecycle::EVENT_WRAPPER_LOCAL_FALLBACK,
            event,
        );
    }

    let mut command = std::process::Command::new(tool);
    command
        .args(args)
        .envs(env.iter().map(|(key, value)| (key, value)));
    command.stdin(if stdin_bytes.is_empty() {
        std::process::Stdio::inherit()
    } else {
        std::process::Stdio::piped()
    });
    release_cwd_for_command(&mut command, cwd);

    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            eprintln!(
                "zccache[err][F]: failed to run {} locally: {error}",
                tool.display()
            );
            return ExitCode::FAILURE;
        }
    };
    if !stdin_bytes.is_empty() {
        if let Some(mut stdin) = child.stdin.take() {
            if let Err(error) = stdin.write_all(stdin_bytes) {
                eprintln!("zccache[err][F]: failed to replay compiler stdin: {error}");
                return ExitCode::FAILURE;
            }
        }
    }
    match child.wait() {
        Ok(status) => exit_code_from_i32(status.code().unwrap_or(1)),
        Err(error) => {
            eprintln!(
                "zccache[err][F]: failed waiting for {}: {error}",
                tool.display()
            );
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn direct_rustfmt_policy_releases_wrapper_cwd() {
        let _guard = CWD_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let original_cwd = std::env::current_dir().ok();
        let build_dir = tempfile::tempdir().unwrap();
        let canonical_build_dir = std::fs::canonicalize(build_dir.path()).unwrap();
        std::env::set_current_dir(&canonical_build_dir).unwrap();

        let tool = noop_tool();
        let args: Vec<String> = noop_args();
        let mut command = std::process::Command::new(&tool);
        command.args(&args);
        release_cwd_for_command(&mut command, &canonical_build_dir);
        let _ = command.status();

        let after = std::env::current_dir().unwrap();
        let after_canonical = std::fs::canonicalize(&after).unwrap_or(after);
        assert_ne!(
            after_canonical, canonical_build_dir,
            "issue #555: direct rustfmt execution must release the wrapper's CWD",
        );

        if let Some(cwd) = original_cwd {
            let _ = std::env::set_current_dir(cwd);
        }
    }

    #[test]
    fn local_fallback_preserves_tool_exit_code() {
        let _guard = CWD_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let original_cwd = std::env::current_dir().ok();
        let cache_root = tempfile::tempdir().unwrap();
        let tool = noop_tool();
        let args = noop_args();
        let exit = run_locally_with_lifecycle_root(
            &tool,
            &args,
            &std::env::current_dir().unwrap(),
            &[("ZCCACHE_TEST_FALLBACK".to_string(), "1".to_string())],
            &[],
            "test pre-dispatch failure",
            Some(cache_root.path()),
        );

        assert_eq!(exit, ExitCode::SUCCESS);
        let report = zccache_audit::audit_cache_root(
            cache_root.path(),
            zccache_audit::LogAuditContext::Integration,
            &zccache_audit::AuditOptions::default().allow_for_test(
                "wrap::passthrough::local_fallback_preserves_tool_exit_code",
                [zccache_audit::RuleId("no-wrapper-local-fallback")],
            ),
        )
        .unwrap();
        assert!(report.passed(), "{}", report.format_human());
        assert_eq!(
            report.test_allow_name.as_deref(),
            Some("wrap::passthrough::local_fallback_preserves_tool_exit_code")
        );
        if let Some(cwd) = original_cwd {
            let _ = std::env::set_current_dir(cwd);
        }
    }
}
