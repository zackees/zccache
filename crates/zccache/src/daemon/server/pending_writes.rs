//! Pending cache-write registry (issue #610, DD-025 condition 1).
//!
//! The helper functions are exercised by the test module here and by
//! the cross-cutting integration tests in
//! `daemon/server/tests/pending_cache_writes.rs`. They are not yet
//! referenced from production code paths because the wiring lives in a
//! follow-up PR that defers `state.artifacts.insert` (see
//! `daemon/server/handle_compile/miss_store.rs` `store_*` functions).
//! Allow `dead_code` here so the scaffolding can land and be reviewed
//! independently of the wiring, matching the existing #610 cadence
//! (#611, #612, #613, #618, #651 landed adversarial tests on the same
//! "tests first, wiring second" model).
//!
//! Bridges the visibility gap between the daemon's response-return and
//! the *deferred* publication of a cold-miss artifact into
//! `state.artifacts`. Every code path that defers the artifact insert
//! into a `tokio::spawn` task **must** call [`register`] before spawning
//! and [`complete`] after the spawn's work has updated the in-memory
//! cache (or after the spawn has failed and the lookup should re-miss).
//!
//! Concurrent lookups call [`await_pending`] before falling through to
//! a regular `state.artifacts.get()`. If a pending entry exists, they
//! wait briefly on the entry's `Notify` (capped by
//! [`PENDING_WAIT_TIMEOUT`]) and then re-attempt the lookup. If the
//! wait times out, they fall through to a normal miss.
//!
//! ## Failure-mode invariant (DD-025 condition 2)
//!
//! The registry's failure mode is always **miss → recompile**, never a
//! wrong-hit. The artifact's content identity remains bound by `blake3`
//! (DD-005); only the *publication* is deferred. Three sub-cases:
//!
//! - Lookup loses the race (no pending entry yet because the cold-miss
//!   handler hasn't reached [`register`]): observable as a regular miss.
//! - Wait times out: observable as a regular miss.
//! - Daemon crashes between [`register`] and [`complete`]: the registry
//!   is in-process only; on restart it is empty, so the second daemon
//!   sees a miss. The on-disk WAL + artifact files recover any committed
//!   entries (DD-008 / DD-017). The crash-mid-flight adversarial test
//!   `crash_mid_flight_recovery_never_surfaces_wrong_content` in
//!   `tests/deferred_cold_path.rs` (PR #618) is the regression bar.
//!
//! ## Blast-radius bound (DD-025 condition 3)
//!
//! - **Time**: entries live ≤ [`PENDING_ENTRY_TTL_MS`] (100 ms — 30× the
//!   measured p99 of `depgraph_update_ns + persist_enqueue` from #605
//!   iter T2). The TTL is informational; staleness is bounded in
//!   practice by the spawn's actual runtime.
//! - **Count**: bounded above by the daemon's persist semaphore
//!   available-permits — the same semaphore that already bounds C/C++
//!   persist spawns.
//! - **Scope**: per-daemon-process. Restart empties the registry.

use std::sync::Arc;
use std::time::Duration;

use dashmap::DashMap;
use tokio::sync::Notify;

/// Maximum time a lookup will wait on a pending registry entry before
/// falling through to a normal miss. Sized to be a small fraction of the
/// p99 cold-miss compile time (sub-millisecond on Linux for the
/// `depgraph_update` work the registry covers) so a contended warm-after-
/// cold lookup pays at most this much extra wall-clock vs. an extra
/// recompile.
///
/// Lookups that can't afford even this much (e.g., the request-cache
/// fast-path) should pass `Duration::ZERO` to [`await_pending`] and
/// fall through to miss immediately.
pub(super) const PENDING_WAIT_TIMEOUT: Duration = Duration::from_millis(5);

/// Informational upper bound on how long a pending entry is expected to
/// live. Used by adversarial tests to flag leaked entries — not enforced
/// at runtime (the entry is cleaned up by the spawned task's
/// [`complete`] call, not by a timer).
#[cfg(test)]
pub(super) const PENDING_ENTRY_TTL_MS: u64 = 100;

/// Register a pending cache-write for `key`.
///
/// **Must** be called by the cold-miss handler *before* spawning the
/// deferred work that will eventually insert into `state.artifacts`.
/// The returned [`Arc<Notify>`] is held by the spawned task so it can
/// call [`complete`] after the in-memory cache has been updated.
///
/// If a pending entry already exists for `key` (e.g., two parallel
/// cold-misses for the same artifact landed in a tight window), the
/// existing entry's `Notify` is returned. Both spawned tasks will then
/// call `notify_waiters()` on the same handle — idempotent.
pub(super) fn register(pending: &DashMap<String, Arc<Notify>>, key: &str) -> Arc<Notify> {
    pending
        .entry(key.to_string())
        .or_insert_with(|| Arc::new(Notify::new()))
        .clone()
}

/// Mark the pending cache-write for `key` as complete.
///
/// Notifies any waiting lookups and removes the registry entry. Must be
/// called by the spawned task after `state.artifacts.insert(...)` has
/// run (success path) or after the spawn has decided the artifact will
/// never be inserted (error path). In the latter case, waiters wake up
/// and re-attempt the lookup, which will miss and recompile — the
/// DD-025-correct failure mode.
pub(super) fn complete(pending: &DashMap<String, Arc<Notify>>, key: &str) {
    if let Some((_, notify)) = pending.remove(key) {
        notify.notify_waiters();
    }
}

/// If a pending cache-write exists for `key`, wait on its `Notify` up
/// to `timeout` (capped by [`PENDING_WAIT_TIMEOUT`]). Returns `true`
/// if the caller observed (and waited on) a pending entry, `false` if
/// no pending entry existed.
///
/// A `true` return tells the caller it should re-attempt its
/// `state.artifacts.get()` lookup: the spawned task should have inserted
/// by now. A `false` return means there was no pending entry — the
/// caller should fall through to its normal miss path.
///
/// A timeout is reported as `true` (the lookup observed a pending entry
/// but the spawn took longer than expected). Callers re-attempt the
/// lookup; if the second attempt also misses, they fall through to
/// recompile — the DD-025 failure-mode-is-miss invariant holds.
pub(super) async fn await_pending(
    pending: &DashMap<String, Arc<Notify>>,
    key: &str,
    timeout: Duration,
) -> bool {
    let Some(notify) = pending.get(key).map(|entry| Arc::clone(&entry)) else {
        return false;
    };
    // Cap the caller's requested timeout at PENDING_WAIT_TIMEOUT so a
    // mis-specified caller can't extend the registry's blast radius.
    let capped = timeout.min(PENDING_WAIT_TIMEOUT);
    if capped.is_zero() {
        return true;
    }
    let _ = tokio::time::timeout(capped, notify.notified()).await;
    true
}

/// Wait until all currently pending cache writes have completed, bounded by
/// `timeout`. Used during graceful shutdown before draining the artifact-index
/// WAL so deferred persist tasks can publish their `(key, ArtifactIndex)` rows.
pub(super) async fn await_all(pending: &DashMap<String, Arc<Notify>>, timeout: Duration) -> bool {
    let deadline = tokio::time::sleep(timeout);
    tokio::pin!(deadline);
    loop {
        if pending.is_empty() {
            return true;
        }
        tokio::select! {
            () = tokio::time::sleep(Duration::from_millis(10)) => {}
            () = &mut deadline => {
                return pending.is_empty();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    /// `register` returns the same `Arc<Notify>` for repeat calls with
    /// the same key — two racing cold-misses share the wait point.
    #[tokio::test]
    async fn register_is_idempotent_for_same_key() {
        let pending: DashMap<String, Arc<Notify>> = DashMap::new();
        let a = register(&pending, "deadbeef");
        let b = register(&pending, "deadbeef");
        assert!(Arc::ptr_eq(&a, &b));
        assert_eq!(pending.len(), 1);
    }

    /// `complete` removes the entry and wakes waiters.
    #[tokio::test]
    async fn complete_notifies_waiters_and_removes_entry() {
        let pending: Arc<DashMap<String, Arc<Notify>>> = Arc::new(DashMap::new());
        let _notify = register(&pending, "feedface");
        let pending_for_waiter = Arc::clone(&pending);
        let wait = tokio::spawn(async move {
            await_pending(&pending_for_waiter, "feedface", PENDING_WAIT_TIMEOUT).await
        });
        // Give the waiter a moment to enter `notified()`.
        tokio::time::sleep(Duration::from_millis(1)).await;
        complete(&pending, "feedface");
        let observed = wait.await.unwrap();
        assert!(observed, "waiter must observe pending entry");
        assert!(pending.is_empty(), "complete must remove entry");
    }

    /// **DD-025 condition 4 — notify-timeout fall-through.**
    ///
    /// A lookup that finds a pending entry but whose `complete` never
    /// arrives must fall through after at most `PENDING_WAIT_TIMEOUT`.
    /// The return is `true` (pending was observed) so the caller knows
    /// to re-attempt the lookup; if the second attempt also misses, the
    /// caller falls through to its normal miss path. The registry must
    /// NOT leak the `Notify` reference: even after timeout the entry
    /// can still be removed by a later `complete` call.
    #[tokio::test]
    async fn await_pending_times_out_and_does_not_leak() {
        let pending: DashMap<String, Arc<Notify>> = DashMap::new();
        let _registered = register(&pending, "cafebabe");
        let start = Instant::now();
        let observed = await_pending(&pending, "cafebabe", PENDING_WAIT_TIMEOUT).await;
        let elapsed = start.elapsed();
        assert!(observed, "pending entry was present — must report true");
        assert!(
            elapsed >= PENDING_WAIT_TIMEOUT,
            "timeout must elapse, got {elapsed:?}"
        );
        // Generous upper bound to avoid CI flakes; the wait is capped by
        // PENDING_WAIT_TIMEOUT (5 ms) + scheduler latency, not seconds.
        assert!(
            elapsed < Duration::from_millis(PENDING_ENTRY_TTL_MS),
            "timeout must not exceed the blast-radius bound ({PENDING_ENTRY_TTL_MS} ms), got {elapsed:?}"
        );
        // Caller can still complete after timeout — registry didn't lose the entry.
        assert_eq!(pending.len(), 1);
        complete(&pending, "cafebabe");
        assert!(pending.is_empty());
    }

    /// `await_pending` for a key that is not registered returns `false`
    /// immediately and never waits. This is the common case for warm
    /// lookups; the registry must be near-zero overhead at rest.
    #[tokio::test]
    async fn await_pending_returns_false_immediately_when_not_registered() {
        let pending: DashMap<String, Arc<Notify>> = DashMap::new();
        let start = Instant::now();
        let observed = await_pending(&pending, "nothere", PENDING_WAIT_TIMEOUT).await;
        let elapsed = start.elapsed();
        assert!(!observed);
        // Should be sub-millisecond; allow 1 ms for scheduler jitter.
        assert!(
            elapsed < Duration::from_millis(1),
            "no-wait path took {elapsed:?}"
        );
    }

    /// Caller-supplied timeouts are capped at `PENDING_WAIT_TIMEOUT` so
    /// a buggy caller can't blow the DD-025 blast-radius bound.
    #[tokio::test]
    async fn await_pending_caps_caller_timeout_at_the_blast_radius_bound() {
        let pending: DashMap<String, Arc<Notify>> = DashMap::new();
        let _registered = register(&pending, "longshot");
        let start = Instant::now();
        // Ask for a one-second timeout — the registry must cap to 5 ms.
        let _observed = await_pending(&pending, "longshot", Duration::from_secs(1)).await;
        let elapsed = start.elapsed();
        assert!(
            elapsed < Duration::from_millis(PENDING_ENTRY_TTL_MS),
            "caller-supplied 1s timeout was not capped, elapsed {elapsed:?}"
        );
    }
}
