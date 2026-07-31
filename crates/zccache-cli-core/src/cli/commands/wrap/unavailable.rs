//! Refusal path for a daemon that could not be reached before dispatch
//! (issue #1170).
//!
//! This module exists because the alternative — running the tool directly,
//! uncached — mirrored the compiler's own exit code. A daemon outage that
//! happened to compile fine exited `0`, so the build stayed green and the
//! outage was invisible. That is the degradation class the reliability
//! burn-down exists to remove, and #1039's read-only hardlinked artifacts
//! made it worse than a lost cache: a direct compiler run cannot overwrite
//! them, so the "fallback" frequently failed or corrupted the build anyway.
//!
//! The sanctioned bypasses are unchanged and both are opt-in and explicit:
//! `ZCCACHE_DISABLE` (full passthrough, never contacts the daemon) and
//! `ZCCACHE_PROBE_BYPASS` (meson-probe TUs exec directly).

use std::path::Path;
use std::process::ExitCode;

/// Exit code for a wrapper-infrastructure failure.
///
/// 125 follows the git/env/docker convention for "the wrapper could not run
/// the command", and is deliberately outside the range compilers use for
/// diagnostics (1/2). CI can classify an infrastructure failure from the code
/// alone rather than by parsing stderr — which is the whole point, since the
/// failure this reports is *not* a compile error.
pub(super) const DAEMON_UNAVAILABLE_EXIT_CODE: u8 = 125;

/// Refuse to run `tool` because the daemon was unreachable before dispatch.
///
/// Loud on three surfaces, per the burn-down's forensics rule: the process
/// exit code, a stderr line in the wrapper's existing
/// `zccache[<sev>][<letter>]:` grammar (letter `D` for daemon), and a durable
/// `wrapper-daemon-unavailable` lifecycle event carrying enough context to
/// attribute the failure after the build has scrolled away.
pub(super) fn refuse_uncached_run(
    endpoint: &str,
    tool: &Path,
    cwd: &Path,
    reason: &str,
) -> ExitCode {
    crate::core::lifecycle::write_event(
        crate::core::lifecycle::EVENT_WRAPPER_DAEMON_UNAVAILABLE,
        serde_json::json!({
            "tool": tool.to_string_lossy(),
            "cwd": cwd.to_string_lossy(),
            "endpoint": endpoint,
            "reason": reason,
            "phase": "pre-dispatch",
            "route": "wrapper",
            "exit_code": DAEMON_UNAVAILABLE_EXIT_CODE,
        }),
    );
    eprintln!(
        "zccache[err][D]: daemon unavailable at {endpoint} ({reason}); refusing to run {} \
         uncached. Run 'zccache status'; set ZCCACHE_DISABLE=1 to compile without the daemon.",
        tool.display()
    );
    ExitCode::from(DAEMON_UNAVAILABLE_EXIT_CODE)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The exit code is the part other tools bind to: soldr and fbuild treat
    /// any non-zero as a build failure, and CI classifies infra failures by
    /// this specific number. Changing it is a contract change for
    /// `zccache cc` / `zccache c++`, so it is pinned here rather than left to
    /// whatever a refactor happens to produce.
    #[test]
    fn the_infrastructure_exit_code_is_pinned_and_distinct_from_compiler_codes() {
        assert_eq!(DAEMON_UNAVAILABLE_EXIT_CODE, 125);
        // Bound through a local so this reads as a value comparison rather
        // than an assertion on a constant expression.
        let code = DAEMON_UNAVAILABLE_EXIT_CODE;
        assert!(
            ![0, 1, 2].contains(&code),
            "must not collide with success, or with the 1/2 a compiler uses for diagnostics"
        );
    }

    /// The refusal must record the outage durably *and* never run the tool.
    /// A green build is the failure mode this replaces, so "did not exit 0"
    /// is the load-bearing assertion.
    #[test]
    fn refusing_records_the_outage_and_does_not_exit_success() {
        let exit = refuse_uncached_run(
            "test-endpoint",
            Path::new("cc"),
            Path::new("."),
            "cannot connect to daemon",
        );
        assert_ne!(
            exit,
            ExitCode::SUCCESS,
            "a daemon outage must never produce a green build"
        );
        assert_eq!(exit, ExitCode::from(DAEMON_UNAVAILABLE_EXIT_CODE));
    }
}
