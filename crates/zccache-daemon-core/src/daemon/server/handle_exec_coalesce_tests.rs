//! Deterministic coverage for the exact-exec coalescing races from #971.

use super::{coalesce_wait, CoalesceOutcome};
use dashmap::DashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Notify;

async fn guarded(future: impl std::future::Future<Output = CoalesceOutcome>) -> CoalesceOutcome {
    tokio::time::timeout(Duration::from_secs(30), future)
        .await
        .expect("coalesce_wait hung — the #971 fix regressed")
}

#[tokio::test]
async fn slot_gone_resolves_without_waiting() {
    let map = DashMap::new();
    let ours = Arc::new(Notify::new());
    let outcome = guarded(coalesce_wait(&map, "k", ours, Duration::from_secs(30))).await;
    assert_eq!(outcome, CoalesceOutcome::SlotResolved);
}

#[tokio::test]
async fn slot_replaced_resolves_without_waiting() {
    let map = DashMap::new();
    let ours = Arc::new(Notify::new());
    map.insert("k".to_string(), Arc::new(Notify::new()));
    let outcome = guarded(coalesce_wait(&map, "k", ours, Duration::from_secs(30))).await;
    assert_eq!(outcome, CoalesceOutcome::SlotResolved);
}

#[tokio::test]
async fn owner_notification_wakes_waiter() {
    let map = DashMap::new();
    let shared = Arc::new(Notify::new());
    map.insert("k".to_string(), Arc::clone(&shared));
    let waker = {
        let shared = Arc::clone(&shared);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            shared.notify_waiters();
        })
    };
    let outcome = guarded(coalesce_wait(&map, "k", shared, Duration::from_secs(30))).await;
    assert_eq!(outcome, CoalesceOutcome::Woken);
    waker.await.unwrap();
}

#[tokio::test]
async fn wedged_owner_times_out() {
    let map = DashMap::new();
    let shared = Arc::new(Notify::new());
    map.insert("k".to_string(), Arc::clone(&shared));
    let outcome = guarded(coalesce_wait(&map, "k", shared, Duration::from_millis(50))).await;
    assert_eq!(outcome, CoalesceOutcome::TimedOut);
}
