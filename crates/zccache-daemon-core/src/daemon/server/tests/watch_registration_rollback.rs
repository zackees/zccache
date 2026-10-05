//! Watch-registration rollback regressions for issue #1906.
//!
//! `watch_directories` marks every raw path in `watched_raw_dirs` *before* it
//! registers a watch, so a registration failure has to roll back both halves
//! of the reservation. Rolling back only `watched_dirs` left the raw
//! pre-filter entry behind, which meant the next call short-circuited on
//! `new_raw.is_empty()` and the directory stayed permanently unwatched — with
//! no watcher event to ever invalidate it, `try_fast_hit` authorizes a hit on
//! a source tree that changed.
//!
//! The deliberate half of the same rule is pinned too: a path whose
//! *canonicalize* fails stays marked, because re-issuing a syscall that is
//! guaranteed to fail on every subsequent compile is not worth the retry.

use super::super::watch::watch_directories;
use super::super::*;

/// The canonical form `watch_directories` stores in `watched_dirs`.
fn canonical_of(raw: &NormalizedPath) -> NormalizedPath {
    let canonical = std::fs::canonicalize(raw.as_path()).expect("test fixture must canonicalize");
    crate::platform::fs::path::strip_verbatim_prefix(&canonical).into()
}

/// A failed registration must clear both reservation halves, so the next
/// call re-canonicalizes and actually registers the directory.
///
/// The `None` watcher arm is the deterministic way in: an unavailable watcher
/// fails every registration in the batch, and it is the same rollback path a
/// failing `w.watch(dir)` takes (an `inotify_add_watch` `ENOSPC` is the
/// realistic trigger, #1156). Deliberately drops the "watch syscall fails"
/// variant — inducing a per-platform `Err` for one watched path is racy, and a
/// racy test is worse than no second test.
#[tokio::test]
async fn a_failed_watch_registration_is_retried_on_the_next_call() {
    let dir = tempfile::tempdir().unwrap();
    let cache_dir: NormalizedPath = dir.path().join("cache").into();
    let server =
        DaemonServer::bind_with_cache_dir(&crate::ipc::unique_test_endpoint(), &cache_dir).unwrap();
    let state = server.test_state_arc();

    let src = dir.path().join("src");
    std::fs::create_dir(&src).unwrap();
    let raw: NormalizedPath = src.as_path().into();
    let canonical = canonical_of(&raw);

    // No watcher: every registration in the batch fails.
    *state.watcher.lock().await = None;
    watch_directories(&state, std::slice::from_ref(&raw)).await;

    assert!(
        !state.watched_dirs.lock().await.contains(&canonical),
        "a failed watch registration must release its watched_dirs reservation"
    );
    assert!(
        !state.watched_raw_dirs.contains_key(&raw),
        "a failed watch registration must not leave the raw path marked, or the \
         pre-filter never retries and fast hits go unverified"
    );

    // With a real watcher in place the retry must reach `w.watch(dir)` rather
    // than short-circuit on the stale raw entry.
    let ignore = crate::watcher::IgnoreFilter::default();
    let (watcher, _events) =
        NotifyWatcher::new(Arc::new(ignore)).expect("notify watcher must initialize");
    *state.watcher.lock().await = Some(watcher);
    watch_directories(&state, std::slice::from_ref(&raw)).await;

    assert!(
        state.watched_dirs.lock().await.contains(&canonical),
        "the retry after a failed registration must actually watch the directory, \
         not short-circuit on the raw pre-filter entry"
    );

    state.shutdown_requested.store(true, Ordering::Release);
    state.shutdown.notify_waiters();
}

/// A canonicalize failure is deliberately *not* rolled back: the raw path stays
/// marked so a nonexistent path does not re-issue a failing canonicalize on
/// every compile.
#[tokio::test]
async fn a_canonicalize_failure_still_marks_the_raw_path() {
    let dir = tempfile::tempdir().unwrap();
    let cache_dir: NormalizedPath = dir.path().join("cache").into();
    let server =
        DaemonServer::bind_with_cache_dir(&crate::ipc::unique_test_endpoint(), &cache_dir).unwrap();
    let state = server.test_state_arc();

    let missing: NormalizedPath = dir.path().join("does-not-exist").into();
    watch_directories(&state, std::slice::from_ref(&missing)).await;

    assert!(
        state.watched_raw_dirs.contains_key(&missing),
        "a path that cannot canonicalize must stay marked, or every later call \
         re-runs a canonicalize that is guaranteed to fail"
    );
    assert!(
        state.watched_dirs.lock().await.is_empty(),
        "a path that never canonicalized must never become watched"
    );

    state.shutdown_requested.store(true, Ordering::Release);
    state.shutdown.notify_waiters();
}
