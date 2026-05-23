//! Daemon-bootstrap / teardown regression tests. These cover the
//! protocol-mismatch auto-recovery path (issue #27) and the bounded wait
//! after a clean stop.

use super::super::daemon::{ensure_daemon, wait_for_daemon_teardown};

// ── Protocol mismatch recovery (issue #27) ──────────────────

/// Regression test for <https://github.com/zackees/zccache/issues/27>.
///
/// When a stale daemon is running but can't communicate (protocol mismatch
/// or corrupt pipe), `ensure_daemon` should auto-recover instead of telling
/// the user to manually run `zccache stop`.
///
/// This test creates a fake "stale daemon" — an IPC listener that accepts
/// connections and immediately drops them, causing `check_daemon_version`
/// to return `CommError`. We then verify that `ensure_daemon` does NOT
/// return the "Run `zccache stop` first" error.
#[tokio::test]
#[ignore] // Integration test — needs daemon binary. Run with `test --full`.
async fn ensure_daemon_auto_recovers_on_comm_error() {
    let endpoint = zccache_monocrate::ipc::unique_test_endpoint();

    // Spawn a fake stale daemon: accepts one connection, drops it (CommError),
    // then shuts down so the endpoint is released for the real daemon.
    let ep = endpoint.clone();
    let mut listener = zccache_monocrate::ipc::IpcListener::bind(&ep).unwrap();
    let server = tokio::spawn(async move {
        // Accept the connection from check_daemon_version, drop it immediately
        let _ = listener.accept().await;
        // Listener drops here, releasing the endpoint
    });

    // Give the listener time to be ready
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    let result = ensure_daemon(&endpoint).await;

    // Ensure server task has completed
    let _ = server.await;

    // The OLD behavior (bug): returns Err("...Run `zccache stop` first.")
    // The NEW behavior (fix): auto-recovers — either succeeds or fails
    // for a different reason (e.g., daemon binary not found).
    if let Err(msg) = &result {
        assert!(
            !msg.contains("zccache stop"),
            "Bug #27: ensure_daemon requires manual `zccache stop` instead of \
             auto-recovering on protocol mismatch: {msg}"
        );
    }
}

/// The bounded wait loop must return promptly when the IPC endpoint is
/// already unreachable (typical CI shape after a clean stop).
#[test]
fn wait_for_daemon_teardown_returns_when_endpoint_unreachable() {
    let tmp = tempfile::tempdir().expect("tempdir");
    std::env::set_var("ZCCACHE_STOP_TIMEOUT_SECS", "2");

    let unreachable_endpoint = if cfg!(windows) {
        r"\\.\pipe\zccache-test-does-not-exist-182".to_string()
    } else {
        tmp.path()
            .join("does-not-exist.sock")
            .to_string_lossy()
            .into_owned()
    };

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let started = std::time::Instant::now();
    rt.block_on(wait_for_daemon_teardown(&unreachable_endpoint));
    let elapsed = started.elapsed();
    std::env::remove_var("ZCCACHE_STOP_TIMEOUT_SECS");

    assert!(
        elapsed < std::time::Duration::from_secs(2),
        "wait_for_daemon_teardown blocked for {elapsed:?} despite endpoint unreachable at t=0"
    );
}
