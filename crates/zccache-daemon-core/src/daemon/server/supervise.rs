//! Supervision for the daemon's long-lived background loops (issue #1177).
//!
//! Every periodic task in `run.rs` was a bare `tokio::spawn` whose `JoinHandle`
//! was dropped. A panic inside one of them is therefore **completely silent**:
//! the task disappears, the daemon keeps serving, and the only symptom is that
//! something stops happening — memory is never evicted, the depgraph is never
//! saved, disk is never reclaimed. Those are exactly the failures that present
//! days later as "the cache got slow" or "the disk filled up", with nothing in
//! the log to attribute them to.
//!
//! `spawn_supervised` keeps the handle, awaits it, and — for loops that are
//! safe to restart — brings them back under bounded exponential backoff. Every
//! death and every restart is loud on both surfaces (a `tracing::warn!` and a
//! durable lifecycle event), per the project's timeout-forensics rule.
//!
//! ## What is and is not restartable
//!
//! A loop is restartable when it owns no unique resource and its work is
//! idempotent — re-running an eviction pass or a depgraph save is harmless.
//! A task that owns the receiving half of a channel is **not**: restarting it
//! produces a loop that looks healthy and silently delivers nothing, which is
//! worse than a task that is visibly gone. `Restart::Never` covers that case
//! and is why the watcher consumer (#1276) degrades rather than restarts.

use std::time::Duration;

/// First backoff after a supervised task dies.
const RESTART_INITIAL_BACKOFF: Duration = Duration::from_secs(1);

/// Ceiling for the doubling restart backoff. A task that keeps panicking must
/// not become a busy loop that costs more than the work it was doing.
const RESTART_MAX_BACKOFF: Duration = Duration::from_secs(60);

/// Restarts allowed before the supervisor gives up and leaves the task dead.
///
/// Bounded rather than infinite: a loop that has panicked this many times is
/// failing deterministically, and the honest outcome is a daemon that is
/// visibly degraded rather than one that hides a permanent fault behind an
/// endless restart cycle.
const MAX_RESTARTS: u32 = 5;

/// Whether a dead task should be brought back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Restart {
    /// Idempotent periodic work — safe to run again from scratch.
    Idempotent,
    /// Owns a unique resource (typically a channel receiver). A replacement
    /// would look healthy while delivering nothing, so the task stays dead and
    /// the daemon reports itself degraded instead.
    Never,
}

/// Why a supervised task stopped.
fn exit_reason(outcome: &Result<(), tokio::task::JoinError>) -> &'static str {
    match outcome {
        Ok(()) => "exited",
        Err(err) if err.is_panic() => "panicked",
        Err(_) => "cancelled",
    }
}

/// Spawn a long-lived background loop under supervision.
///
/// `factory` builds the task future; it is called again on each restart, so it
/// must capture whatever the loop needs by clone rather than by move-once.
pub(super) fn spawn_supervised<S, F, Fut>(
    name: &'static str,
    is_shutting_down: S,
    restart: Restart,
    factory: F,
) -> tokio::task::JoinHandle<()>
where
    S: Fn() -> bool + Send + 'static,
    F: Fn() -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    tokio::spawn(async move {
        let mut restarts = 0u32;
        let mut backoff = RESTART_INITIAL_BACKOFF;
        loop {
            let outcome = tokio::spawn(factory()).await;

            // A task ending during shutdown is the expected path, not a fault.
            if is_shutting_down() {
                return;
            }

            let reason = exit_reason(&outcome);
            tracing::warn!(
                task = name,
                reason,
                restarts,
                "supervised background task stopped unexpectedly"
            );
            crate::core::lifecycle::write_event(
                crate::core::lifecycle::EVENT_BACKGROUND_TASK_DIED,
                serde_json::json!({
                    "task": name,
                    "reason": reason,
                    "restarts": restarts,
                    "restartable": restart == Restart::Idempotent,
                }),
            );

            if restart == Restart::Never || restarts >= MAX_RESTARTS {
                tracing::error!(
                    task = name,
                    restarts,
                    "supervised background task will not be restarted; the daemon is degraded"
                );
                return;
            }

            tokio::time::sleep(backoff).await;
            if is_shutting_down() {
                return;
            }
            restarts += 1;
            backoff = (backoff * 2).min(RESTART_MAX_BACKOFF);
            tracing::info!(task = name, restarts, "restarting background task");
            crate::core::lifecycle::write_event(
                crate::core::lifecycle::EVENT_BACKGROUND_TASK_RESTARTED,
                serde_json::json!({ "task": name, "restarts": restarts }),
            );
        }
    })
}

#[cfg(test)]
#[path = "tests/supervise.rs"]
mod tests;
