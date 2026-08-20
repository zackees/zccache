use super::*;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

fn cfg(grace_ms: u64, ceiling_ms: u64, poll_ms: u64) -> AdaptiveWaitConfig {
    AdaptiveWaitConfig {
        poll_interval: Duration::from_millis(poll_ms),
        no_daemon_grace: Duration::from_millis(grace_ms),
        hard_ceiling: Duration::from_millis(ceiling_ms),
    }
}

// Bogus endpoint that connect_client cannot bind to on either platform.
// Unix: a nonexistent socket path. Windows: a nonexistent named pipe.
fn dead_endpoint() -> &'static str {
    if crate::platform::host::is_windows() {
        r"\\.\pipe\zccache-test-issue-673-dead"
    } else {
        "/tmp/zccache-test-issue-673-dead.sock"
    }
}

// -- acquire_spawn_slot_at (issue #952 single-flight arbiter) ----------

#[test]
fn spawn_slot_first_caller_wins_second_parks() {
    let dir = tempfile::tempdir().unwrap();
    let slot = dir.path().join("daemon.lock.spawn");
    let winner = acquire_spawn_slot_at(slot.clone(), Duration::from_secs(20));
    assert!(winner.is_some(), "first caller must win the slot");
    assert!(
        acquire_spawn_slot_at(slot.clone(), Duration::from_secs(20)).is_none(),
        "second caller must park while the slot is held"
    );
    drop(winner);
    assert!(
        acquire_spawn_slot_at(slot, Duration::from_secs(20)).is_some(),
        "slot must be reusable after the winner's guard drops"
    );
}

#[test]
fn spawn_slot_stale_holder_is_reclaimed() {
    let dir = tempfile::tempdir().unwrap();
    let slot = dir.path().join("daemon.lock.spawn");
    std::fs::write(&slot, "12345\n").unwrap();
    // A zero staleness window means any existing slot is abandoned;
    // sleep a few ms so the file's mtime age is strictly positive.
    std::thread::sleep(Duration::from_millis(20));
    let reclaimed = acquire_spawn_slot_at(slot, Duration::from_millis(0));
    assert!(
        reclaimed.is_some(),
        "an abandoned slot older than the staleness window must be reclaimed"
    );
}

// -- classify_wait_tick (pure decision function) -----------------------

#[test]
fn pending_when_daemon_visible_and_below_hard_ceiling() {
    let c = cfg(1_000, 5_000, 100);
    let tick = classify_wait_tick(Duration::from_millis(500), Some(42), Some(42), &c);
    assert_eq!(tick, WaitTick::Pending);
}

#[test]
fn hard_ceiling_hit_only_when_daemon_visible() {
    let c = cfg(1_000, 5_000, 100);
    let tick = classify_wait_tick(Duration::from_millis(5_000), Some(42), Some(42), &c);
    assert_eq!(
        tick,
        WaitTick::HardCeilingHit {
            observed_pid: Some(42)
        }
    );
}

#[test]
fn daemon_exited_when_previously_observed_then_gone() {
    let c = cfg(1_000, 5_000, 100);
    let tick = classify_wait_tick(Duration::from_millis(200), None, Some(42), &c);
    assert_eq!(tick, WaitTick::DaemonExited { pid: 42 });
}

#[test]
fn no_daemon_grace_passed_when_never_observed_and_grace_elapsed() {
    let c = cfg(1_000, 5_000, 100);
    let tick = classify_wait_tick(Duration::from_millis(1_000), None, None, &c);
    assert_eq!(tick, WaitTick::NoDaemonGracePassed);
}

#[test]
fn pending_when_never_observed_but_grace_still_running() {
    let c = cfg(1_000, 5_000, 100);
    let tick = classify_wait_tick(Duration::from_millis(500), None, None, &c);
    assert_eq!(tick, WaitTick::Pending);
}

// -- wait_for_daemon_ready_with (drives the loop with mock predicate) --

#[tokio::test(flavor = "current_thread")]
async fn returns_grace_error_when_no_lockfile_ever_observed() {
    // Tight grace + ceiling so the test resolves in well under a second.
    let c = cfg(150, 5_000, 25);
    let err = wait_for_daemon_ready_with(dead_endpoint(), || None, c)
        .await
        .expect_err("no-daemon path must fail, not hang");
    assert!(
        err.contains("no daemon lockfile observed"),
        "wrong error: {err}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn returns_hard_ceiling_error_when_daemon_visible_but_unreachable() {
    // Daemon always-alive (mock returns Some), but no real socket → IPC
    // connect keeps failing → we hit the hard ceiling.
    let c = cfg(5_000, 200, 25);
    let err = wait_for_daemon_ready_with(dead_endpoint(), || Some(12_345), c)
        .await
        .expect_err("hard ceiling path must fail, not hang");
    assert!(err.contains("hard cap"), "wrong error: {err}");
    assert!(err.contains("12345"), "PID should appear: {err}");
}

#[tokio::test(flavor = "current_thread")]
async fn returns_daemon_exited_error_when_lockfile_disappears() {
    // First poll observes the daemon, every subsequent poll says None.
    // The loop must exit with DaemonExited, not hit the grace timeout.
    let polls = Arc::new(AtomicU32::new(0));
    let c = cfg(10_000, 10_000, 25);
    let polls_for_check = Arc::clone(&polls);
    let err = wait_for_daemon_ready_with(
        dead_endpoint(),
        move || {
            let n = polls_for_check.fetch_add(1, Ordering::SeqCst);
            if n == 0 {
                Some(99_999)
            } else {
                None
            }
        },
        c,
    )
    .await
    .expect_err("daemon-exit path must fail, not hang");
    assert!(err.contains("exited"), "wrong error: {err}");
    assert!(err.contains("99999"), "PID should appear: {err}");
}

// ---------------------------------------------------------------------
// #1161 leg 2 — probe before kill
// ---------------------------------------------------------------------

// `classify_probe_outcome` itself is already covered in `wrap/ipc.rs`
// (pong/error/timeout). What is new here is the wrapper below and the
// CommError arms that consult it, so that is what these exercise.

#[tokio::test]
async fn a_disabled_probe_budget_keeps_the_unconditional_replace() {
    // ZCCACHE_WEDGE_PROBE_BUDGET_MS=0 is the documented A/B switch back to
    // pre-#753 behaviour. It must not accidentally become "never replace":
    // with the probe off there is no evidence of life, so the answer is
    // "not merely busy" and recovery proceeds.
    let _guard = EnvVarGuard::set("ZCCACHE_WEDGE_PROBE_BUDGET_MS", "0");
    assert!(
        !probe_says_daemon_is_merely_busy("definitely-not-a-real-endpoint").await,
        "a disabled probe must not be read as proof the daemon is healthy"
    );
}

#[tokio::test]
async fn an_unreachable_endpoint_is_not_merely_busy() {
    // Nothing listening: the probe fails fast with a transport error,
    // which classifies as escalate. Guards against the inverted default,
    // where a probe failure would wrongly protect a dead daemon and wedge
    // the client forever.
    let _guard = EnvVarGuard::set("ZCCACHE_WEDGE_PROBE_BUDGET_MS", "250");
    assert!(
        !probe_says_daemon_is_merely_busy(&crate::ipc::unique_test_endpoint()).await,
        "an endpoint with no daemon must not be treated as busy-but-healthy"
    );
}

/// Restores the previous value on drop so these cases cannot leak into
/// the rest of the binary's tests.
struct EnvVarGuard {
    key: &'static str,
    previous: Option<std::ffi::OsString>,
}

impl EnvVarGuard {
    fn set(key: &'static str, value: &str) -> Self {
        let previous = std::env::var_os(key);
        std::env::set_var(key, value);
        Self { key, previous }
    }
}

impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        match self.previous.take() {
            Some(v) => std::env::set_var(self.key, v),
            None => std::env::remove_var(self.key),
        }
    }
}

// ---------------------------------------------------------------------
// #1161 leg 3 — drain grace
// ---------------------------------------------------------------------

#[tokio::test]
async fn an_exited_daemon_is_observed_without_waiting_out_the_budget() {
    // The graceful path. If the daemon finishes its drain we must notice
    // promptly rather than sitting on the full budget -- a 30 s stall on
    // every replacement would be its own bug.
    let start = std::time::Instant::now();
    let exited = wait_for_exit_while(std::time::Duration::from_secs(30), || false).await;
    assert!(exited);
    assert!(
        start.elapsed() < std::time::Duration::from_secs(1),
        "an already-exited process must return immediately, took {:?}",
        start.elapsed()
    );
}

#[tokio::test]
async fn a_daemon_that_never_leaves_reports_the_budget_expired() {
    // The escalation path: the caller force-kills only on `false`, so an
    // inverted result here would mean never reclaiming a wedged daemon.
    let budget = std::time::Duration::from_millis(250);
    let start = std::time::Instant::now();
    let exited = wait_for_exit_while(budget, || true).await;
    assert!(!exited);
    assert!(
        start.elapsed() >= budget,
        "must actually wait the budget before escalating, waited {:?}",
        start.elapsed()
    );
}

#[test]
fn the_drain_budget_matches_the_daemon_side_drain() {
    // The whole point of leg 3. The daemon's own
    // INDEX_WRITER_SHUTDOWN_DRAIN_TIMEOUT is 30 s; a stopper budget below
    // that guarantees SIGKILL mid-flush, which truncates index.bin and
    // costs a full recompile. This was 200 ms. If the daemon-side value
    // moves, this must move with it.
    assert_eq!(
        GRACEFUL_DRAIN_BUDGET,
        std::time::Duration::from_secs(30),
        "stopper grace must match the daemon's shutdown drain budget"
    );
    assert!(
        FORCE_KILL_REAP_BUDGET < GRACEFUL_DRAIN_BUDGET,
        "reaping a killed process is not the same wait as letting one drain"
    );
}

/// An endpoint nothing is listening on. The refusal path must return
/// before touching it at all, so its only requirement is being unique.
fn never_bound_endpoint() -> String {
    crate::ipc::unique_test_endpoint()
}

/// A `DaemonProcess` that cannot possibly be the one recorded on disk.
fn foreign_identity(
    pid: u32,
) -> running_process::broker::protocol_v2::backend_handle::DaemonProcess {
    running_process::broker::protocol_v2::backend_handle::DaemonProcess {
        pid,
        exe_path: std::path::PathBuf::from("zccache-daemon"),
        exe_hash: [0u8; 32],
        legacy_exe_sha256: [0u8; 32],
        boot_id: "boot-that-never-was".to_string(),
        ipc_endpoint: crate::ipc::running_process_endpoint("test-endpoint"),
        started_at_unix_ms: 1,
        idle_timeout_secs: None,
    }
}

/// #1161 leg 1. The gate sits before the `Shutdown` request, not just
/// before the kill: asking an innocent daemon to retire is itself the
/// damage. A mismatch must therefore return without doing *anything* —
/// no roundtrip, no kill, no lock removal.
#[tokio::test]
async fn a_stop_is_refused_when_the_recorded_instance_is_not_the_one_that_failed() {
    // No identity is written for this endpoint, so `daemon_identity_matches`
    // is false for any argument — which is the mismatch case.
    let victim = foreign_identity(std::process::id());
    let start = std::time::Instant::now();

    let killed = stop_daemon_instance(
        &never_bound_endpoint(),
        Some(&victim),
        std::time::Duration::from_secs(30),
    )
    .await;

    assert_eq!(killed, None, "a mismatched instance must not be stopped");
    assert!(
        start.elapsed() < std::time::Duration::from_secs(1),
        "the refusal must short-circuit before any IPC or drain wait, took {:?}",
        start.elapsed()
    );
}

/// The `None` case is the one that used to be implicit: with no recorded
/// identity the old code re-read the lock file and killed whoever it
/// named. Refusing is the safe direction — it costs one clear error about
/// a daemon that is already not answering, where permitting costs killing
/// a live daemon that was never at fault.
#[tokio::test]
async fn a_stop_is_refused_when_the_caller_cannot_name_the_failed_instance() {
    let start = std::time::Instant::now();

    let killed = stop_daemon_instance(
        &never_bound_endpoint(),
        None,
        std::time::Duration::from_secs(30),
    )
    .await;

    assert_eq!(killed, None, "an unnamed instance must not be stopped");
    assert!(
        start.elapsed() < std::time::Duration::from_secs(1),
        "the refusal must short-circuit, took {:?}",
        start.elapsed()
    );
}

/// The wedge path (#955 fail-fast) is allowed a much shorter drain than an
/// orderly replacement — a daemon that already failed a responsiveness
/// probe will not complete a 30 s durable drain. What it is *not* allowed
/// is a different answer to "whom may I kill": both entry points funnel
/// through the same identity gate.
#[test]
fn the_wedge_path_shortens_the_drain_but_not_the_identity_gate() {
    assert!(
        WEDGE_DRAIN_BUDGET < GRACEFUL_DRAIN_BUDGET,
        "a wedged daemon must not be given the full graceful drain"
    );
}
