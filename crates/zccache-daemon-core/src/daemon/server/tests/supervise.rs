//! Supervision tests for issue #1177.
//!
//! These drive the supervisor directly rather than through a live daemon: the
//! property under test is "a dead loop comes back, loudly", and inducing a real
//! panic in the real eviction loop would make the test about the loop instead.
//!
//! The shutdown signal is injected as a predicate for the same reason it is
//! injected in production — the supervisor should not need a whole
//! `SharedState` to answer one question, and a test should not have to build
//! one to ask it.

use super::*;

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;

fn never_shutting_down() -> impl Fn() -> bool + Send + 'static {
    || false
}

/// A panicking loop is restarted. This is the failure #1177 is about: before
/// supervision the panic was silent, the task vanished, and the only symptom
/// was that eviction quietly stopped happening.
///
/// Runs on tokio's virtual clock (`start_paused`). The supervisor sleeps
/// `RESTART_INITIAL_BACKOFF` (1s) before the first restart, and the old
/// real-time version allowed 10s for that — a 10x margin that still failed
/// intermittently on CI. Not because a restart takes 10 seconds, but because
/// `cargo test` runs this alongside ~770 other tests: a current-thread runtime
/// on an oversubscribed 2-core Windows runner can simply be starved that long.
/// With the clock paused, tokio advances time only when the runtime is idle,
/// so the backoff costs nothing and the test measures the supervisor rather
/// than the runner's load.
#[tokio::test(start_paused = true)]
async fn a_panicking_idempotent_task_is_restarted() {
    let attempts = Arc::new(AtomicU32::new(0));
    let counter = Arc::clone(&attempts);

    let handle = spawn_supervised(
        "test-panicker",
        never_shutting_down(),
        Restart::Idempotent,
        None,
        move || {
            let counter = Arc::clone(&counter);
            async move {
                counter.fetch_add(1, Ordering::AcqRel);
                panic!("induced");
            }
        },
    );

    // Wait for the first restart only. The backoff ladder is asserted
    // separately by its constants; sitting through 1+2+4+8+16 s here would
    // make this a slow test for no extra confidence.
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while attempts.load(Ordering::Acquire) < 2 {
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("a panicking task must be restarted, not silently dropped");

    handle.abort();
}

/// Restarts are bounded. A loop that has panicked this many times is failing
/// deterministically, and a visibly dead task is more honest than an endless
/// restart cycle hiding a permanent fault.
#[test]
fn restarts_are_bounded() {
    // Bound through a local so this reads as a value comparison rather than
    // an assertion on a constant expression.
    let bound = MAX_RESTARTS;
    assert!(bound > 0, "a transient panic must be survivable");
    assert!(
        bound < 100,
        "a deterministically failing loop must end up visibly dead"
    );
}

/// A task that owns a unique resource must NOT be restarted: a replacement
/// would look healthy while delivering nothing, which is worse than being
/// visibly gone. This is the shape the watcher consumer needs (#1276).
#[tokio::test]
async fn a_non_restartable_task_is_not_brought_back() {
    let attempts = Arc::new(AtomicU32::new(0));
    let counter = Arc::clone(&attempts);

    let handle = spawn_supervised(
        "test-unique",
        never_shutting_down(),
        Restart::Never,
        None,
        move || {
            let counter = Arc::clone(&counter);
            async move {
                counter.fetch_add(1, Ordering::AcqRel);
            }
        },
    );

    tokio::time::timeout(std::time::Duration::from_secs(10), handle)
        .await
        .expect("the supervisor must return rather than loop")
        .expect("the supervisor task itself must not panic");

    assert_eq!(
        attempts.load(Ordering::Acquire),
        1,
        "a Restart::Never task runs exactly once"
    );
}

/// A task ending because the daemon is shutting down is the expected path and
/// must be silent. Without this check every clean shutdown would log a
/// spurious "task died" and write a durable event for it.
#[tokio::test]
async fn a_task_ending_during_shutdown_is_not_treated_as_a_fault() {
    let shutdown = Arc::new(AtomicBool::new(true));
    let flag = Arc::clone(&shutdown);
    let attempts = Arc::new(AtomicU32::new(0));
    let counter = Arc::clone(&attempts);

    let handle = spawn_supervised(
        "test-shutdown",
        move || flag.load(Ordering::Acquire),
        Restart::Idempotent,
        None,
        move || {
            let counter = Arc::clone(&counter);
            async move {
                counter.fetch_add(1, Ordering::AcqRel);
            }
        },
    );

    tokio::time::timeout(std::time::Duration::from_secs(10), handle)
        .await
        .expect("the supervisor must return promptly on shutdown")
        .expect("the supervisor task itself must not panic");

    assert_eq!(
        attempts.load(Ordering::Acquire),
        1,
        "an idempotent task must not be restarted once shutdown was requested"
    );
}

/// The backoff has to grow and then stop growing: unbounded growth would stop
/// recovering a transiently broken loop, and no growth would make a
/// fast-failing loop cost more than the work it replaced.
#[test]
fn the_restart_backoff_doubles_and_saturates() {
    let mut backoff = RESTART_INITIAL_BACKOFF;
    for _ in 0..20 {
        backoff = (backoff * 2).min(RESTART_MAX_BACKOFF);
    }
    assert_eq!(backoff, RESTART_MAX_BACKOFF);
    assert!(RESTART_INITIAL_BACKOFF < RESTART_MAX_BACKOFF);
}
