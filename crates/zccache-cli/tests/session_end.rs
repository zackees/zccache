//! Integration tests for `zccache session-end`.
//!
//! Issue #150: when the daemon process is gone entirely, soldr's at-exit
//! `session-end` call hits a vanished pipe / socket and the CLI used to
//! exit 1 — cascading up through `cargo test` teardown on Windows CI.
//!
//! Mirrors #137's daemon-side idempotency at the CLI connection layer:
//! a daemon-unreachable error must yield exit 0 with a one-line warning
//! to stderr.

use std::process::Command;

use zccache_core::NormalizedPath;

fn zccache_bin() -> NormalizedPath {
    let mut path = std::env::current_exe()
        .expect("current_exe")
        .parent()
        .expect("parent of test binary")
        .parent()
        .expect("target dir")
        .to_path_buf();

    if cfg!(windows) {
        path.push("zccache.exe");
    } else {
        path.push("zccache");
    }

    assert!(
        path.exists(),
        "zccache binary not found at {path:?}. Run `cargo build` first."
    );
    NormalizedPath::new(path)
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
#[test]
fn session_end_with_unreachable_daemon_is_idempotent() {
    let bin = zccache_bin();
    let endpoint = unreachable_endpoint();

    let output = Command::new(bin.as_path())
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
