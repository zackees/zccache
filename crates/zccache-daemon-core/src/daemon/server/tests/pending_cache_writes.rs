//! Integration tests for the `pending_cache_writes` registry on
//! `SharedState` (issue #610, DD-025 condition 1).
//!
//! The registry module itself has unit tests in
//! `daemon/server/pending_writes.rs` covering the helper API in
//! isolation. These tests verify the field is properly wired through
//! `SharedState`/`DaemonServer::bind` and that the registry behaves
//! correctly when driven through the `Arc<SharedState>` pathway the
//! cold-miss handler will use in the follow-up PR.

use std::sync::Arc;
use std::time::{Duration, Instant};

use super::super::pending_writes;
use super::super::*;

/// At daemon startup the pending registry must be empty — DD-025
/// condition 3 requires the scope be per-process. A non-empty registry
/// at bind time would mean some other state path is leaking into it.
#[tokio::test]
async fn pending_cache_writes_is_empty_on_fresh_daemon() {
    let endpoint = crate::ipc::unique_test_endpoint();
    let server = DaemonServer::bind(&endpoint).unwrap();
    assert!(
        server.state.pending_cache_writes.is_empty(),
        "fresh daemon must have empty pending_cache_writes registry, found {}",
        server.state.pending_cache_writes.len()
    );
}

/// End-to-end register + complete through the `SharedState` pathway.
/// `register` adds an entry; a concurrent lookup observes the pending
/// entry and waits; `complete` wakes it and clears the registry.
#[tokio::test]
async fn pending_registry_register_and_complete_through_shared_state() {
    let endpoint = crate::ipc::unique_test_endpoint();
    let server = DaemonServer::bind(&endpoint).unwrap();
    let state = Arc::clone(&server.state);

    let key = "deadbeefcafebabe0000000000000000";
    let _registered = pending_writes::register(&state.pending_cache_writes, key);
    assert_eq!(state.pending_cache_writes.len(), 1);

    let state_for_lookup = Arc::clone(&state);
    let waiter = tokio::spawn(async move {
        pending_writes::await_pending(
            &state_for_lookup.pending_cache_writes,
            key,
            Duration::from_millis(5),
        )
        .await
    });

    // Give the waiter a moment to enter `notified()`.
    tokio::time::sleep(Duration::from_millis(1)).await;
    pending_writes::complete(&state.pending_cache_writes, key);

    let observed = waiter.await.unwrap();
    assert!(observed, "waiter must report it saw a pending entry");
    assert!(
        state.pending_cache_writes.is_empty(),
        "complete must clear the registry entry"
    );
}

/// **DD-025 condition 4 — notify-timeout fall-through, through `SharedState`.**
///
/// The unit-level fall-through test lives in `pending_writes::tests`;
/// this case verifies the same property through the actual `SharedState`
/// pathway the cold-miss handler will use. A lookup that observes a
/// pending entry but whose `complete` never arrives must:
/// 1. Wake up within the blast-radius bound (≤ 100 ms).
/// 2. Report `true` so the caller knows to re-attempt the lookup.
/// 3. Not leak the registry entry — a later `complete` must still
///    succeed.
#[tokio::test]
async fn pending_registry_notify_timeout_through_shared_state() {
    let endpoint = crate::ipc::unique_test_endpoint();
    let server = DaemonServer::bind(&endpoint).unwrap();
    let state = Arc::clone(&server.state);

    let key = "feedfacedeadbeef0000000000000000";
    let _registered = pending_writes::register(&state.pending_cache_writes, key);

    let start = Instant::now();
    let observed =
        pending_writes::await_pending(&state.pending_cache_writes, key, Duration::from_millis(5))
            .await;
    let elapsed = start.elapsed();

    assert!(observed, "pending entry was present — must report true");
    // Must wake up within the blast-radius bound, not hang forever.
    assert!(
        elapsed < Duration::from_millis(100),
        "notify-timeout blew the blast-radius bound: {elapsed:?}"
    );

    // Registry still holds the entry — a late `complete` can clean it up.
    assert_eq!(state.pending_cache_writes.len(), 1);
    pending_writes::complete(&state.pending_cache_writes, key);
    assert!(state.pending_cache_writes.is_empty());
}

/// A lookup for a key with no pending entry must fall through
/// immediately — the registry is near-zero overhead at rest.
#[tokio::test]
async fn pending_registry_warm_lookup_pays_no_wait() {
    let endpoint = crate::ipc::unique_test_endpoint();
    let server = DaemonServer::bind(&endpoint).unwrap();
    let state = Arc::clone(&server.state);

    let start = Instant::now();
    let observed = pending_writes::await_pending(
        &state.pending_cache_writes,
        "warmkey0000000000000000000000000",
        Duration::from_millis(5),
    )
    .await;
    let elapsed = start.elapsed();

    assert!(!observed, "no pending entry must report false");
    assert!(
        elapsed < Duration::from_millis(2),
        "warm-lookup no-wait path took {elapsed:?}"
    );
}
