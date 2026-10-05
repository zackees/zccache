//! Unit tests for `daemon::startup_lockfile` (#1903).
//!
//! Split into a sibling file per the crate convention (`process.rs` +
//! `process_tests.rs`, `child_watchdog.rs` + `child_watchdog_tests.rs`).
//!
//! These drive the real call sites rather than hand-emitting payloads: the
//! failure arm forces `write_lock_file` to fail for real (a regular file
//! standing where the lock file's parent directory must go) and the policy
//! arm reads the lifecycle row back out of the log `write_event` resolved.

use std::path::Path;

use super::*;
use crate::daemon::server::tests::CacheDirEnvGuard;

/// Point the process-global cache dir at `path` for the rest of the test.
///
/// `CacheDirEnvGuard` also clears `ZCCACHE_DAEMON_NAMESPACE`, so the lock file
/// path resolves the same way it does for an un-namespaced daemon — which is
/// what makes the fixture's blocker file reach `write_lock_file`.
fn guarded_cache_dir(path: &Path) -> CacheDirEnvGuard {
    CacheDirEnvGuard::set(path)
}

/// Cache dir whose lock-file parent can never be created: a regular file sits
/// where the directory would have to go, so `create_dir_all_private(parent)`
/// fails on every supported platform.
fn cache_dir_behind_a_file(temp: &Path) -> std::path::PathBuf {
    let cache_root = temp.join("cache");
    std::fs::create_dir_all(&cache_root).unwrap();
    std::fs::write(cache_root.join("blocker"), b"not a directory").unwrap();
    cache_root.join("blocker").join("nested")
}

/// #1903: a daemon that wins the endpoint bind but cannot write its PID into
/// the lock file is undiscoverable — `check_running_daemon` and
/// `probe_existing_daemon` both read the lock file, so they keep reporting the
/// daemon as absent while it holds the cache root. Startup must classify that
/// as `RefuseServing`, not as a successful start.
#[test]
fn unwritable_lock_file_parent_refuses_serving() {
    let temp = tempfile::tempdir().unwrap();
    let _guard = guarded_cache_dir(&cache_dir_behind_a_file(temp.path()));

    match record_ownership(std::process::id()) {
        StartupDecision::RefuseServing { error } => {
            assert!(
                !error.trim().is_empty(),
                "a refused start must carry the underlying IO error, or a \
                 post-incident search has nothing to grep for"
            );
        }
        StartupDecision::Serve => {
            panic!(
                "write_lock_file must fail under a non-directory parent; a pass \
                     here means the degraded arm is unreachable, not that the bug is fixed"
            )
        }
    }
}

/// The GREEN control for the failure test above: with a writable cache dir the
/// same call records this daemon's PID and serves. Without it,
/// `unwritable_lock_file_parent_refuses_serving` could pass because
/// `write_lock_file` never succeeds at all.
#[test]
fn writable_lock_file_parent_serves() {
    let temp = tempfile::tempdir().unwrap();
    let cache_dir = temp.path().join("cache");
    let _guard = guarded_cache_dir(&cache_dir);

    let decision = record_ownership(std::process::id());

    assert!(
        matches!(decision, StartupDecision::Serve),
        "a writable cache dir must record ownership and serve"
    );
    assert_eq!(
        crate::ipc::read_lock_file_pid(),
        Some(std::process::id()),
        "the lock file is the only ownership record `check_running_daemon` reads"
    );

    crate::ipc::remove_lock_file();
}

/// The refusal must leave a durable lifecycle row behind, and that row's name
/// must be in the shared catalog — log-audit tooling and operator docs key on
/// `EVENT_ALL`, so an uncatalogued event is invisible to them (the same
/// regression the depgraph reset events guard against).
#[test]
fn the_refusal_emits_one_catalogued_lifecycle_row() {
    assert!(
        crate::core::lifecycle::EVENT_ALL
            .contains(&crate::daemon::lifecycle::EVENT_DAEMON_LOCKFILE_UNRECORDED),
        "an event outside the catalog cannot be given a log-audit rule"
    );

    let temp = tempfile::tempdir().unwrap();
    let cache_dir = temp.path().join("cache");
    let _guard = guarded_cache_dir(&cache_dir);

    report_unrecorded("test://endpoint", "lock file parent is not a directory");

    // Resolve the log exactly the way production's `write_event` does, so the
    // test cannot pass by reading a path the daemon never writes. The tempdir
    // is unique per test, so a concurrently running test cannot append a row
    // of ours between the write and the read.
    let log = crate::core::lifecycle::log_file_path();
    let body = std::fs::read_to_string(log.as_path()).unwrap_or_else(|e| {
        panic!(
            "a refused start must write a durable lifecycle event at {}: {e}",
            log.as_path().display()
        )
    });
    let matching = body
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter(|record: &serde_json::Value| {
            record["event"] == crate::daemon::lifecycle::EVENT_DAEMON_LOCKFILE_UNRECORDED
        })
        .count();
    assert_eq!(
        matching,
        1,
        "the refusal must append exactly one `{}` row to {}",
        crate::daemon::lifecycle::EVENT_DAEMON_LOCKFILE_UNRECORDED,
        log.as_path().display()
    );
}
