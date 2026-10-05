//! Direct execution paths used when wrapper caching is disabled or unsupported.

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

/// Spawn `cmd` with the caller's original working directory as the child's
/// CWD, then release the wrapper's own CWD handle.
///
/// `child_cwd` is passed in rather than read from
/// `std::env::current_dir()` because by the time this runs the wrapper may
/// already have chdir'd to the temp directory (issue #1909, the
/// `WrapperRoute::ProbeBypass` route in `wrap.rs`). Reading the process cwd
/// here spawned the compiler in `/tmp`, so relative `-I`/`-o` paths in the
/// probe's argv resolved against the temp dir instead of the build tree.
fn run_with_released_cwd(
    cmd: &mut std::process::Command,
    child_cwd: &Path,
) -> std::io::Result<i32> {
    // An empty `child_cwd` is what `wrap.rs`'s
    // `std::env::current_dir().unwrap_or_default()` yields when the cwd could
    // not be read. Falling back to not setting `current_dir` keeps the
    // previous behaviour (inherit the wrapper's cwd) rather than spawning in
    // `""`.
    if !child_cwd.as_os_str().is_empty() {
        release_cwd_for_command(cmd, child_cwd);
    }
    // Wrapper passthrough is foreground execution: preserve Cargo jobserver
    // descriptors and the caller's process group, with no daemon containment
    // or descriptor sanitization. Keep this boundary local rather than
    // borrowing formatter policy for an unrelated compiler invocation.
    Ok(kernal_api::platform::process::foreground_status(cmd)?
        .code()
        .unwrap_or(1))
}

/// Run the compiler/tool directly without caching.
///
/// `reason`: `Some` for user-visible bypasses (`ZCCACHE_DISABLE`) — a yellow
/// warning names the cause so the uncached path is never silent (issue
/// #1211). `None` for the probe bypass (`ZCCACHE_PROBE_BYPASS`), which is
/// machine-invoked: probe callers parse the tool's stderr (`clang -###`
/// writes there), so injecting a warning line would corrupt the probe.
///
/// `child_cwd` is the caller's original working directory, threaded in rather
/// than read from the process so the child is spawned in the build tree even
/// after the wrapper has chdir'd to the temp dir (issue #1909).
pub(super) fn run_passthrough(args: &[String], reason: Option<&str>, child_cwd: &Path) -> ExitCode {
    let tool = &args[0];
    let tool_args = args.get(1..).unwrap_or(&[]);
    let resolved = resolve_compiler_path(tool);

    // #1791: a hit's read-only hardlinked output would make rustc refuse to
    // replace it (Windows). Only the user-visible ZCCACHE_DISABLE bypass.
    if reason.is_some() && super::detach_outputs::is_rustc(&resolved) {
        super::detach_outputs::detach_readonly_rustc_outputs(tool_args);
    }

    if let Some(reason) = reason {
        let warning = format!(
            "zccache[warn][F]: {reason}; running {} directly, uncached\n",
            resolved.display(),
        );
        let _ = super::write_wrapper_warning_line(
            &mut std::io::stderr(),
            warning.as_bytes(),
            super::wrapper_stderr_color_enabled(),
        );
    }

    let mut cmd = std::process::Command::new(&resolved);
    cmd.args(tool_args);
    match run_with_released_cwd(&mut cmd, child_cwd) {
        Ok(code) => exit_code_from_i32(code),
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
#[cfg(test)]
mod tests {
    use super::*;

    fn noop_tool() -> std::path::PathBuf {
        if crate::platform::host::is_windows() {
            std::path::PathBuf::from("cmd.exe")
        } else {
            std::path::PathBuf::from("true")
        }
    }

    fn noop_args() -> Vec<String> {
        if crate::platform::host::is_windows() {
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
        let _ = run_passthrough(&args, None, &canonical_build_dir);

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

    /// Issue #1909: the child's CWD must be the caller-supplied directory,
    /// not the wrapper's own (by this point the temp) cwd. Spawning a shell
    /// that reports its cwd into a relative file proves where the child
    /// actually ran.
    #[test]
    fn run_passthrough_spawns_the_child_in_the_supplied_cwd() {
        let _guard = CWD_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let original_cwd = std::env::current_dir().ok();
        let build_dir = tempfile::tempdir().unwrap();
        let canonical_build_dir = std::fs::canonicalize(build_dir.path()).unwrap();

        let (tool, tool_args): (std::path::PathBuf, Vec<String>) =
            if crate::platform::host::is_windows() {
                (
                    std::path::PathBuf::from("cmd.exe"),
                    vec!["/c".to_string(), "cd > zccache-cwd-probe.txt".to_string()],
                )
            } else {
                (
                    std::path::PathBuf::from("/bin/sh"),
                    vec!["-c".to_string(), "pwd > zccache-cwd-probe.txt".to_string()],
                )
            };

        let mut args = vec![tool.to_string_lossy().into_owned()];
        args.extend(tool_args);
        let code = run_passthrough(&args, None, &canonical_build_dir);

        if let Some(cwd) = original_cwd {
            let _ = std::env::set_current_dir(cwd);
        }

        assert_eq!(code, ExitCode::SUCCESS, "the shim must exit 0");
        let reported = std::fs::read_to_string(canonical_build_dir.join("zccache-cwd-probe.txt"))
            .expect(
                "issue #1909: the child's cwd report must land in the caller-supplied \
                    directory, proving the compiler ran there rather than in the temp dir",
            );
        let reported = reported.trim();
        let expected = std::fs::canonicalize(&canonical_build_dir)
            .unwrap_or_else(|_| canonical_build_dir.clone())
            .to_string_lossy()
            .into_owned();
        assert_eq!(
            std::fs::canonicalize(reported).unwrap_or_else(|_| std::path::PathBuf::from(reported)),
            std::fs::canonicalize(&canonical_build_dir)
                .unwrap_or_else(|_| canonical_build_dir.clone()),
            "issue #1909: the child must run in {expected}, reported {reported}",
        );
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
}
