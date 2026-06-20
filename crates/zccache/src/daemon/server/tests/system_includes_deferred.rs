//! Tests for the issue #784 phase 2c deferred-load path on the
//! `SystemIncludeCache` snapshot. Mirrors the compiler-hash phase-2a
//! tests in `tests/compiler_hash.rs` ("Issue #784: deferred compiler-
//! hash-cache load" section) and the metadata phase-2b tests, with the
//! `tokio::sync::Mutex` wrinkle on the live cache.

use super::super::*;

/// `bind_with_cache_dir` no longer reads the system-includes snapshot
/// from disk. The cache starts empty regardless of what is on disk;
/// the daemon binary's `system_includes_loader().load_and_install()`
/// does the merge after the readiness lockfile is written.
#[tokio::test]
async fn bind_does_not_load_system_includes_from_disk() {
    let tmp = tempfile::tempdir().unwrap();
    let cache_dir = crate::core::NormalizedPath::new(tmp.path());
    let snapshot_path = crate::core::config::system_includes_cache_path_from_cache_dir(&cache_dir);

    // Pre-write a snapshot with a synthetic compiler-includes entry.
    let mut pre = crate::depgraph::SystemIncludeCache::new();
    let fake_compiler = tmp.path().join("fake-clang");
    let synthetic_include = tmp.path().join("include");
    std::fs::write(&fake_compiler, b"fake compiler").unwrap();
    std::fs::create_dir_all(&synthetic_include).unwrap();
    pre.insert(
        crate::core::NormalizedPath::new(&fake_compiler),
        vec![crate::core::NormalizedPath::new(&synthetic_include)],
    );
    pre.save_to_disk(snapshot_path.as_path()).unwrap();
    assert!(snapshot_path.as_path().exists());

    // Bind — must NOT read the snapshot (the load is deferred to the
    // background loader fired post-lockfile by the daemon binary).
    let endpoint = crate::ipc::unique_test_endpoint();
    let server = DaemonServer::bind_with_cache_dir(&endpoint, &cache_dir).unwrap();
    let state = server.test_state();
    {
        let live = state.system_includes.lock().await;
        assert_eq!(
            live.len(),
            0,
            "bind must start with an empty system-includes cache",
        );
    }
    assert!(
        !state.system_includes_loaded.load(Ordering::Acquire),
        "the loaded flag must start false until the background loader runs",
    );
}

/// The `system_includes_loader()` handle reads the snapshot and merges
/// it into the live cache. Confirms the deferred-load path reaches
/// functional parity with the old sync-in-bind path.
#[tokio::test]
async fn system_includes_loader_merges_snapshot_into_live_cache() {
    let tmp = tempfile::tempdir().unwrap();
    let cache_dir = crate::core::NormalizedPath::new(tmp.path());
    let snapshot_path = crate::core::config::system_includes_cache_path_from_cache_dir(&cache_dir);

    let mut pre = crate::depgraph::SystemIncludeCache::new();
    let fake_compiler = tmp.path().join("fake-gcc");
    let synthetic_include = tmp.path().join("include");
    std::fs::write(&fake_compiler, b"fake gcc").unwrap();
    std::fs::create_dir_all(&synthetic_include).unwrap();
    let compiler_key = crate::core::NormalizedPath::new(&fake_compiler);
    pre.insert(
        compiler_key.clone(),
        vec![crate::core::NormalizedPath::new(&synthetic_include)],
    );
    pre.save_to_disk(snapshot_path.as_path()).unwrap();

    let endpoint = crate::ipc::unique_test_endpoint();
    let server = DaemonServer::bind_with_cache_dir(&endpoint, &cache_dir).unwrap();

    // Loader uses `blocking_lock()` on the tokio mutex, which would
    // panic inside a runtime; production calls it from
    // `tokio::task::spawn_blocking`, so do the same here.
    let loader = server.system_includes_loader();
    tokio::task::spawn_blocking(move || loader.load_and_install())
        .await
        .unwrap();

    let state = server.test_state();
    {
        let live = state.system_includes.lock().await;
        assert_eq!(
            live.len(),
            1,
            "loader must merge the persisted entry into the live cache",
        );
        assert!(
            live.get(&fake_compiler).is_some(),
            "the merged entry must be observable via the live cache API",
        );
    }
    assert!(
        state.system_includes_loaded.load(Ordering::Acquire),
        "loader must set system_includes_loaded=true so shutdown save fires",
    );
}

/// Missing on-disk snapshot is not an error: the loader logs a warning,
/// leaves the live cache empty, and still flips the loaded flag so
/// shutdown save fires (and short-circuits because the cache is empty
/// per `save_to_disk`'s empty-cache early-exit).
#[tokio::test]
async fn system_includes_loader_tolerates_missing_snapshot() {
    let tmp = tempfile::tempdir().unwrap();
    let cache_dir = crate::core::NormalizedPath::new(tmp.path());
    let endpoint = crate::ipc::unique_test_endpoint();
    let server = DaemonServer::bind_with_cache_dir(&endpoint, &cache_dir).unwrap();

    let loader = server.system_includes_loader();
    tokio::task::spawn_blocking(move || loader.load_and_install())
        .await
        .unwrap();

    let state = server.test_state();
    {
        let live = state.system_includes.lock().await;
        assert_eq!(live.len(), 0);
    }
    assert!(
        state.system_includes_loaded.load(Ordering::Acquire),
        "loaded flag flips even when the snapshot was absent",
    );
}

/// `merge_from` is `&mut self`, takes ownership of the loaded cache,
/// and drains all entries. Documents the contract the deferred loader
/// relies on.
#[test]
fn system_includes_merge_from_drains_other() {
    let tmp = tempfile::tempdir().unwrap();
    let compiler_a = tmp.path().join("a");
    let compiler_b = tmp.path().join("b");
    let include_a = tmp.path().join("inc_a");
    let include_b = tmp.path().join("inc_b");
    std::fs::write(&compiler_a, b"a").unwrap();
    std::fs::write(&compiler_b, b"b").unwrap();
    std::fs::create_dir_all(&include_a).unwrap();
    std::fs::create_dir_all(&include_b).unwrap();

    let mut live = crate::depgraph::SystemIncludeCache::new();
    live.insert(
        crate::core::NormalizedPath::new(&compiler_a),
        vec![crate::core::NormalizedPath::new(&include_a)],
    );

    let mut loaded = crate::depgraph::SystemIncludeCache::new();
    loaded.insert(
        crate::core::NormalizedPath::new(&compiler_b),
        vec![crate::core::NormalizedPath::new(&include_b)],
    );

    live.merge_from(loaded);

    assert_eq!(live.len(), 2);
    assert!(live.get(&compiler_a).is_some());
    assert!(live.get(&compiler_b).is_some());
}
