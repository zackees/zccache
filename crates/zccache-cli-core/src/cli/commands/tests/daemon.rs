//! Daemon-bootstrap / teardown regression tests. These cover the
//! protocol-mismatch auto-recovery path (issue #27), the bounded wait
//! after a clean stop, the bounded Status-probe (issue #554), and the
//! endpoint-scoped ownership records in `zccache stop` (issue #1904).

use std::process::ExitCode;

use super::super::daemon::{
    cmd_stop, profile_env_overrides, tokio_console_bind, wait_for_daemon_teardown,
};
// #1161: the recovery path these tests cover now has a single implementation
// in `cli::runtime`. The duplicate in `commands::daemon` — which had none of
// the identity gating, probe-before-replace, or drain budget — is gone.
use crate::cli::runtime::{check_daemon_version, ensure_daemon, VersionCheck};

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
    let endpoint = crate::ipc::unique_test_endpoint();

    // Spawn a fake stale daemon: accepts one connection, drops it (CommError),
    // then shuts down so the endpoint is released for the real daemon.
    let ep = endpoint.clone();
    let mut listener = crate::ipc::IpcListener::bind(&ep).unwrap();
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

#[test]
fn tokio_console_bind_prefers_cli_over_env_over_default() {
    let old = std::env::var("TOKIO_CONSOLE_BIND").ok();
    std::env::set_var("TOKIO_CONSOLE_BIND", "localhost:5555");

    assert_eq!(tokio_console_bind(Some("localhost:1234")), "localhost:1234");
    assert_eq!(tokio_console_bind(None), "localhost:5555");

    match old {
        Some(value) => std::env::set_var("TOKIO_CONSOLE_BIND", value),
        None => std::env::remove_var("TOKIO_CONSOLE_BIND"),
    }
    assert_eq!(tokio_console_bind(None), "127.0.0.1:6669");
}

#[test]
fn profile_env_overrides_enable_tokio_console_profile() {
    let env = profile_env_overrides("localhost:1234", true);

    assert!(env.contains(&(
        "ZCCACHE_DAEMON_PROFILE".to_string(),
        "tokio-console".to_string()
    )));
    assert!(env.contains(&(
        "TOKIO_CONSOLE_BIND".to_string(),
        "localhost:1234".to_string()
    )));
    assert!(env.contains(&("ZCCACHE_TOKIO_CONSOLE_OPEN".to_string(), "1".to_string())));
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

// ── Wedged-daemon Status probe (issue #554) ──────────────────────────

/// Regression test for <https://github.com/zackees/zccache/issues/554>.
///
/// When the daemon's IPC accepts connections but never answers `Request::Status`,
/// `check_daemon_version` must surface `CommError` within the configured probe
/// timeout — not the 5-minute global recv default. Before the fix this took
/// 300 s; with the fix it takes ≤ 2 s (or whatever
/// `ZCCACHE_STATUS_PROBE_TIMEOUT_SECS` is set to).
#[tokio::test]
async fn check_daemon_version_is_bounded_when_daemon_never_answers() {
    // Force a 1-second probe so the test stays fast.
    std::env::set_var("ZCCACHE_STATUS_PROBE_TIMEOUT_SECS", "1");

    let endpoint = crate::ipc::unique_test_endpoint();
    let ep = endpoint.clone();

    // Fake wedged daemon: accept once, hold the connection open without
    // responding so the client's recv hits the timeout (not ConnectionClosed).
    let mut listener = crate::ipc::IpcListener::bind(&ep).expect("bind listener");
    let server = tokio::spawn(async move {
        let _conn = listener.accept().await;
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
    });

    // Give the listener a moment to be ready.
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    let started = std::time::Instant::now();
    let verdict = check_daemon_version(&endpoint).await;
    let elapsed = started.elapsed();

    server.abort();
    std::env::remove_var("ZCCACHE_STATUS_PROBE_TIMEOUT_SECS");

    assert!(
        matches!(verdict, VersionCheck::CommError),
        "unresponsive Status probe should route to CommError recovery"
    );
    assert!(
        elapsed < std::time::Duration::from_secs(3),
        "check_daemon_version took {elapsed:?} against an unresponsive daemon — \
         issue #554 expects bounded fail-fast (≤ probe timeout + epsilon)"
    );
}

/// End-to-end guard for the `ensure_daemon` recovery branch behind the bounded
/// Status probe. Ignored for the same reason as the issue #27 integration test:
/// with a daemon binary available, this may spawn a real replacement daemon.
#[tokio::test]
#[ignore] // Integration test — may spawn the daemon binary. Run with `test --full`.
async fn ensure_daemon_is_bounded_when_status_probe_never_answers() {
    std::env::set_var("ZCCACHE_STATUS_PROBE_TIMEOUT_SECS", "1");

    let endpoint = crate::ipc::unique_test_endpoint();
    let ep = endpoint.clone();

    let mut listener = crate::ipc::IpcListener::bind(&ep).expect("bind listener");
    let server = tokio::spawn(async move {
        let _conn = listener.accept().await;
        tokio::time::sleep(std::time::Duration::from_secs(15)).await;
    });

    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    let started = std::time::Instant::now();
    let result = ensure_daemon(&endpoint).await;
    let elapsed = started.elapsed();

    server.abort();
    std::env::remove_var("ZCCACHE_STATUS_PROBE_TIMEOUT_SECS");

    assert!(
        elapsed < std::time::Duration::from_secs(10),
        "ensure_daemon took {elapsed:?} against an unresponsive daemon; result={result:?}"
    );
}

// ── Endpoint-scoped lifecycle (issue #1904) ───────────────────────────
//
// `cmd_stop` receives the endpoint it was asked to stop. When the IPC
// shutdown roundtrip is unreachable it has to answer "is a daemon serving
// THIS endpoint?" from THIS endpoint's ownership record. Reaching for the
// default endpoint's lock makes a healthy daemon on an unrelated endpoint
// look like the failure being cleaned up — and makes a stop against a custom
// endpoint clear the default daemon's lock file.

/// These tests mutate process-global cache-dir env, so they must not overlap
/// with each other. (`cargo test` threads a single process; nextest gives each
/// test its own.)
static CACHE_ROOT_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Pin `ZCCACHE_CACHE_DIR` / `ZCCACHE_DAEMON_NAMESPACE` to an isolated root so
/// the default-endpoint ownership records these tests read and write are the
/// tempdir's, never the developer's real daemon's. Restored on drop.
struct IsolatedCacheRoot {
    _tmp: tempfile::TempDir,
    _serialized: std::sync::MutexGuard<'static, ()>,
    previous_cache_dir: Option<String>,
    previous_namespace: Option<String>,
}

impl IsolatedCacheRoot {
    const NAMESPACE: &str = "stop-endpoint-scope";

    fn set() -> Self {
        let serialized = CACHE_ROOT_ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let tmp = tempfile::tempdir().expect("tempdir");
        let previous_cache_dir = std::env::var("ZCCACHE_CACHE_DIR").ok();
        let previous_namespace = std::env::var("ZCCACHE_DAEMON_NAMESPACE").ok();
        std::env::set_var("ZCCACHE_CACHE_DIR", tmp.path());
        std::env::set_var("ZCCACHE_DAEMON_NAMESPACE", Self::NAMESPACE);
        Self {
            _tmp: tmp,
            _serialized: serialized,
            previous_cache_dir,
            previous_namespace,
        }
    }
}

impl Drop for IsolatedCacheRoot {
    fn drop(&mut self) {
        for (name, previous) in [
            ("ZCCACHE_CACHE_DIR", &self.previous_cache_dir),
            ("ZCCACHE_DAEMON_NAMESPACE", &self.previous_namespace),
        ] {
            match previous {
                Some(value) => std::env::set_var(name, value),
                None => std::env::remove_var(name),
            }
        }
    }
}

/// A process id that is guaranteed not to be running: spawn a short-lived
/// child, reap it, then hand back its pid.
fn dead_pid() -> u32 {
    let mut child = if cfg!(windows) {
        std::process::Command::new("powershell")
            .args(["-NoProfile", "-Command", "Start-Sleep -Milliseconds 1"])
            .spawn()
            .expect("spawn reaped child")
    } else {
        std::process::Command::new("true").spawn().expect("spawn reaped child")
    };
    let pid = child.id();
    child.wait().expect("reap child");
    pid
}

/// An endpoint nothing serves, distinct from the (isolated) default one.
fn unserved_endpoint() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let unique = format!("zccache-stop-scope-{}-{nanos}", std::process::id());
    if cfg!(windows) {
        format!(r"\\.\pipe\{unique}")
    } else {
        std::env::temp_dir()
            .join(format!("{unique}.sock"))
            .to_string_lossy()
            .into_owned()
    }
}

/// Regression test for <https://github.com/zackees/zccache/issues/1904>.
///
/// `zccache stop` against a custom endpoint must not consult — or clear — the
/// DEFAULT endpoint's ownership record. Before the fix the unreachable-IPC arm
/// called the endpoint-less `check_running_daemon()`, which reads the default
/// lock, confirms its pid dead, deletes that lock and retires the default
/// endpoint: a stop aimed at one daemon silently dismantled another's.
#[tokio::test]
async fn stop_preserves_the_default_endpoints_stale_lock() {
    let _root = IsolatedCacheRoot::set();
    let endpoint = unserved_endpoint();
    assert_ne!(
        endpoint,
        crate::ipc::default_endpoint(),
        "the unserved endpoint must differ from the default one for this test to mean anything"
    );

    let default_lock = crate::ipc::lock_file_path();
    crate::ipc::write_lock_file(dead_pid()).expect("write default-endpoint lock");

    let code = cmd_stop(&endpoint).await;

    assert_eq!(
        code,
        ExitCode::SUCCESS,
        "`zccache stop` for an unserved endpoint must report success"
    );
    assert!(
        default_lock.as_path().exists(),
        "#1904: stopping {endpoint} cleared the DEFAULT endpoint's lock file at {}",
        default_lock.as_path().display()
    );
}

/// The other half of #1904's Failure A: a live daemon on the DEFAULT endpoint
/// must not be mistaken for the daemon the caller failed to reach on a custom
/// endpoint. With the pid still alive the endpoint-less lookup returned it, and
/// the forced-kill arm refused (no persisted identity), so the stop failed on a
/// clean, unrelated endpoint instead of reporting "not running at {endpoint}".
#[tokio::test]
async fn stop_ignores_a_live_default_endpoint_daemon() {
    let _root = IsolatedCacheRoot::set();
    let endpoint = unserved_endpoint();

    let mut other = if cfg!(windows) {
        std::process::Command::new("powershell")
            .args(["-NoProfile", "-Command", "Start-Sleep -Seconds 30"])
            .spawn()
            .expect("spawn bystander")
    } else {
        std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("spawn bystander")
    };
    let other_pid = other.id();
    crate::ipc::write_lock_file(other_pid).expect("write default-endpoint lock");

    let code = cmd_stop(&endpoint).await;

    let survived = other.try_wait().expect("poll bystander").is_none();
    let _ = other.kill();
    let _ = other.wait();

    assert_eq!(
        code,
        ExitCode::SUCCESS,
        "#1904: an unreachable custom endpoint must report 'daemon not running at \
         {endpoint}', not fail on the live daemon serving the DEFAULT endpoint"
    );
    assert!(
        survived,
        "#1904: stopping {endpoint} must never signal process {other_pid}, which is \
         serving a different endpoint"
    );
}
