//! Test-only counters for the perf-fix acceptance criteria (#1774): one hash
//! per published output, and a bounded fsync budget per publication.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::thread::ThreadId;

static HASH_CALLS: AtomicU64 = AtomicU64::new(0);
static FILE_SYNCS: AtomicU64 = AtomicU64::new(0);
static DIR_SYNCS: AtomicU64 = AtomicU64::new(0);
static COUNTER_LOCK: Mutex<()> = Mutex::new(());
static ACTIVE_THREAD: Mutex<Option<ThreadId>> = Mutex::new(None);

/// Serializes callers of these process-global counters against each
/// other, and scopes every `record_*` call to the guard-holding test's
/// own thread. `cargo test` runs the whole suite in one process with
/// many threads, so an un-scoped global counter would also pick up
/// publications made by unrelated tests running concurrently on other
/// threads; tests that don't take this guard are themselves unaffected,
/// since only the guard holder's thread is ever counted.
pub(in crate::daemon::server) struct CounterGuard {
    _lock: std::sync::MutexGuard<'static, ()>,
}

impl Drop for CounterGuard {
    fn drop(&mut self) {
        *ACTIVE_THREAD
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = None;
    }
}

pub(in crate::daemon::server) fn guard() -> CounterGuard {
    let lock = COUNTER_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    *ACTIVE_THREAD
        .lock()
        .unwrap_or_else(|error| error.into_inner()) = Some(std::thread::current().id());
    CounterGuard { _lock: lock }
}

fn on_active_thread() -> bool {
    *ACTIVE_THREAD
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        == Some(std::thread::current().id())
}

pub(in crate::daemon::server) fn record_hash() {
    if on_active_thread() {
        HASH_CALLS.fetch_add(1, Ordering::Relaxed);
    }
}

pub(in crate::daemon::server) fn record_file_sync() {
    if on_active_thread() {
        FILE_SYNCS.fetch_add(1, Ordering::Relaxed);
    }
}

pub(in crate::daemon::server) fn record_dir_sync() {
    if on_active_thread() {
        DIR_SYNCS.fetch_add(1, Ordering::Relaxed);
    }
}

/// Must be called only while holding the [`guard`].
pub(in crate::daemon::server) fn reset() {
    HASH_CALLS.store(0, Ordering::Relaxed);
    FILE_SYNCS.store(0, Ordering::Relaxed);
    DIR_SYNCS.store(0, Ordering::Relaxed);
}

/// `(hash_calls, file_content_syncs, directory_syncs)`.
pub(in crate::daemon::server) fn snapshot() -> (u64, u64, u64) {
    (
        HASH_CALLS.load(Ordering::Relaxed),
        FILE_SYNCS.load(Ordering::Relaxed),
        DIR_SYNCS.load(Ordering::Relaxed),
    )
}
