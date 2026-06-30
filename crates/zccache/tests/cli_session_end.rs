//! Integration tests for `zccache session-end`.
//!
//! Issue #150: when the daemon process is gone entirely, soldr's at-exit
//! `session-end` call hits a vanished pipe / socket and the CLI used to
//! exit 1 — cascading up through `cargo test` teardown on Windows CI.
//!
//! Mirrors #137's daemon-side idempotency at the CLI connection layer:
//! a daemon-unreachable error must yield exit 0 with a one-line warning
//! to stderr.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::panic_in_result_fn, clippy::unwrap_in_result)]

use std::path::PathBuf;
use std::process::Command;

/// Path to the zccache binary built by cargo for this integration test.
///
/// Using `CARGO_BIN_EXE_zccache` makes cargo build the binary automatically
/// before running the test, so this works under `cargo test --workspace`
/// without a separate `cargo build` step.
fn zccache_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_zccache"))
}

/// Returns an endpoint guaranteed to have no daemon listening — exactly
/// the state soldr observes when the daemon process has already exited
/// before its at-exit `session-end` runs.
fn unreachable_endpoint() -> String {
    let pid = std::process::id();
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    #[cfg(windows)]
    {
        // Pipe name that has never existed.
        format!(r"\\.\pipe\zccache-issue150-{pid}-{nonce}")
    }
    #[cfg(unix)]
    {
        // Socket path inside a guaranteed-empty tempdir parent — and we
        // don't create the file, so connect() will see ENOENT.
        let tmp = std::env::temp_dir();
        tmp.join(format!("zccache-issue150-{pid}-{nonce}.sock"))
            .to_string_lossy()
            .into_owned()
    }
}

/// Regression test for issue #150: `zccache session-end <uuid>` against
/// a non-existent endpoint must exit 0 (not 1) and emit a one-line
/// warning to stderr.
///
/// `#[ignore]` because this end-to-end test needs the freshly-built
/// `zccache` binary on the test runner. The unit-level coverage for
/// the underlying predicate `is_daemon_unreachable_err` lives in
/// `main.rs`'s `tests` module and is the regression check that runs
/// in normal CI; this test is the integration smoke test that runs
/// under `./test --integration`.
#[test]
#[ignore]
fn session_end_with_unreachable_daemon_is_idempotent() {
    let bin = zccache_bin();
    let endpoint = unreachable_endpoint();

    let output = Command::new(&bin)
        .arg("session-end")
        .arg("00000000-0000-0000-0000-000000000000")
        .arg("--endpoint")
        .arg(&endpoint)
        .output()
        .expect("failed to run zccache session-end");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        output.status.success(),
        "session-end against unreachable daemon should exit 0 (issue #150). \
         exit={:?} stdout={stdout} stderr={stderr}",
        output.status.code(),
    );
    assert!(
        stderr.contains("daemon unreachable"),
        "expected 'daemon unreachable' warning on stderr, got: {stderr}"
    );
}
