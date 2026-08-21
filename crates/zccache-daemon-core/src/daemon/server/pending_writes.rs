//! Pending cache-write registry (issue #610, DD-025 condition 1).
//!
//! The registry is wired into deferred artifact publication in
//! `daemon/server/handle_compile/miss_store.rs`; focused unit and
//! cross-cutting tests live here and in
//! `daemon/server/tests/pending_cache_writes.rs`.
//!
//! Bridges the visibility gap between the daemon's response-return and
//! the *deferred* publication of a cold-miss artifact into
//! `state.artifacts`. Every code path that defers the artifact insert
//! into a `tokio::spawn` task **must** call [`register`] before spawning
//! and [`complete`] after the spawn's work has updated the in-memory
//! cache (or after the spawn has failed and the lookup should re-miss).
//!
//! Proven cache-hit lookups whose request-specific metadata is not yet
//! visible call [`await_pending_payload`] before re-attempting the lookup.
//! The test-only `await_pending` helper retains coverage of the original
//! bounded-wait registry contract.
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
//! - **Time**: proven-payload waits are capped by
//!   [`PENDING_PAYLOAD_WAIT_TIMEOUT`]; entries remain until every same-key
//!   publisher finishes, and shutdown applies a separate bounded drain.
//! - **Count**: one entry per artifact key; the daemon's persist semaphore
//!   bounds active persistence work.
//! - **Scope**: per-daemon-process. Restart empties the registry.

use std::sync::Arc;
use std::time::Duration;

use dashmap::DashMap;
use tokio::sync::Notify;

/// Per-key publication state. Multiple compiles can publish independent
/// verdict/output metadata for the same shared Rust artifact concurrently.
pub(super) struct PendingWrite {
    active_publishers: usize,
    notify: Arc<Notify>,
}

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
#[cfg(test)]
pub(super) const PENDING_WAIT_TIMEOUT: Duration = Duration::from_millis(5);

/// Longer wait used when the depgraph has already proven a cache hit and the
/// only remaining race is rustc staged-payload publication. This prevents an
/// immediate duplicate compile while a large artifact is being durably
/// snapshotted.
pub(super) const PENDING_PAYLOAD_WAIT_TIMEOUT: Duration = Duration::from_secs(5);

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
/// The returned [`Arc<Notify>`] is held by the spawned task so it can call
/// [`complete`] after the in-memory cache has been updated. Same-key
/// registrations increment the active-publisher count and share the notifier.
///
pub(super) fn register(pending: &DashMap<String, PendingWrite>, key: &str) -> Arc<Notify> {
    let mut entry = pending
        .entry(key.to_string())
        .or_insert_with(|| PendingWrite {
            active_publishers: 0,
            notify: Arc::new(Notify::new()),
        });
    entry.active_publishers += 1;
    Arc::clone(&entry.notify)
}

/// Mark the pending cache-write for `key` as complete.
///
/// Notifies waiting lookups after every publisher finishes, but retains the
/// registry entry until the final same-key publisher completes. Must be called
/// after the publisher has updated `state.artifacts` or decided it cannot.
pub(super) fn complete(pending: &DashMap<String, PendingWrite>, key: &str) {
    use dashmap::mapref::entry::Entry;

    let notify = match pending.entry(key.to_string()) {
        Entry::Occupied(mut entry) if entry.get().active_publishers > 1 => {
            entry.get_mut().active_publishers -= 1;
            Arc::clone(&entry.get().notify)
        }
        Entry::Occupied(entry) => entry.remove().notify,
        Entry::Vacant(_) => return,
    };
    notify.notify_waiters();
}

/// If a pending cache-write exists for `key`, wait on its `Notify` up
/// to `timeout` (capped by [`PENDING_PAYLOAD_WAIT_TIMEOUT`]). Returns `true`
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
#[cfg(test)]
pub(super) async fn await_pending(
    pending: &DashMap<String, PendingWrite>,
    key: &str,
    timeout: Duration,
) -> bool {
    await_pending_capped(pending, key, timeout, PENDING_WAIT_TIMEOUT).await
}

/// Wait for a known payload-publication race. Callers use this only after a
/// cache key has already been proven, so waiting for durable publication is
/// preferable to recompiling the same artifact through an uncached path.
pub(super) async fn await_pending_payload(
    pending: &DashMap<String, PendingWrite>,
    key: &str,
    timeout: Duration,
) -> bool {
    await_pending_capped(pending, key, timeout, PENDING_PAYLOAD_WAIT_TIMEOUT).await
}

async fn await_pending_capped(
    pending: &DashMap<String, PendingWrite>,
    key: &str,
    timeout: Duration,
    maximum: Duration,
) -> bool {
    let Some(entry) = pending.get(key) else {
        return false;
    };
    // Cap the caller's requested timeout at the pending-entry blast radius so
    // a mis-specified caller can't extend the registry's scope indefinitely.
    let capped = timeout.min(maximum);
    if capped.is_zero() {
        return true;
    }
    // Register the waiter while the map guard prevents `complete` from
    // removing/notifying this entry. Otherwise a completion between cloning
    // the Notify and first polling the future could be lost until timeout.
    let notified = Arc::clone(&entry.notify).notified_owned();
    tokio::pin!(notified);
    let notified_before_unlock = notified.as_mut().enable();
    drop(entry);
    if !notified_before_unlock {
        let _ = tokio::time::timeout(capped, notified).await;
    }
    true
}

/// Wait until all currently pending cache writes have completed, bounded by
/// `timeout`. Used during graceful shutdown before draining the artifact-index
/// WAL so deferred persist tasks can publish their `(key, ArtifactIndex)` rows.
pub(super) async fn await_all(pending: &DashMap<String, PendingWrite>, timeout: Duration) -> bool {
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

    /// Repeat registrations share the notifier and retain the entry until the
    /// final same-key publisher completes.
    #[tokio::test]
    async fn same_key_registrations_are_counted() {
        let pending: DashMap<String, PendingWrite> = DashMap::new();
        let a = register(&pending, "deadbeef");
        let b = register(&pending, "deadbeef");
        assert!(Arc::ptr_eq(&a, &b));
        assert_eq!(pending.len(), 1);
        assert_eq!(pending.get("deadbeef").unwrap().active_publishers, 2);
        complete(&pending, "deadbeef");
        assert_eq!(pending.get("deadbeef").unwrap().active_publishers, 1);
        complete(&pending, "deadbeef");
        assert!(pending.is_empty());
    }

    /// `complete` removes the entry and wakes waiters.
    #[tokio::test]
    async fn complete_notifies_waiters_and_removes_entry() {
        let pending: Arc<DashMap<String, PendingWrite>> = Arc::new(DashMap::new());
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
        const SCHEDULER_TOLERANCE: Duration = Duration::from_millis(50);

        let pending: DashMap<String, PendingWrite> = DashMap::new();
        let _registered = register(&pending, "cafebabe");
        let start = Instant::now();
        let observed = await_pending(&pending, "cafebabe", PENDING_WAIT_TIMEOUT).await;
        let elapsed = start.elapsed();
        assert!(observed, "pending entry was present — must report true");
        assert!(
            elapsed >= PENDING_WAIT_TIMEOUT,
            "timeout must elapse, got {elapsed:?}"
        );
        // Tokio requests a wakeup at the deadline; it cannot guarantee the
        // task is polled before unrelated suite work runs. Keep the 100 ms
        // registry blast-radius assertion and add an explicit, small
        // scheduling tolerance instead of requiring an impossible zero
        // overshoot (#1408).
        let upper_bound = Duration::from_millis(PENDING_ENTRY_TTL_MS) + SCHEDULER_TOLERANCE;
        assert!(
            elapsed < upper_bound,
            "timeout must remain within the blast-radius bound plus scheduler tolerance \
             ({upper_bound:?}), got {elapsed:?}"
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
        let pending: DashMap<String, PendingWrite> = DashMap::new();
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

    /// Caller-supplied timeouts are capped at `PENDING_PAYLOAD_WAIT_TIMEOUT` so
    /// a buggy caller can't blow the DD-025 blast-radius bound.
    #[tokio::test]
    async fn await_pending_caps_caller_timeout_at_the_blast_radius_bound() {
        let pending: DashMap<String, PendingWrite> = DashMap::new();
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
