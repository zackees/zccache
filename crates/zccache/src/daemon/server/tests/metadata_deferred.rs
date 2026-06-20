//! Tests for the issue #784 phase 2b deferred-load path on the
//! `MetadataCache` snapshot. Mirrors the compiler-hash phase-2a tests
//! in `tests/compiler_hash.rs` ("Issue #784: deferred compiler-hash-
//! cache load" section).

use super::super::*;

/// `bind_with_cache_dir` no longer reads the `metadata.bin` snapshot
/// from disk. The cache starts empty regardless of what is on disk;
/// the daemon binary's `metadata_cache_loader().load_and_install()`
/// does the merge after the readiness lockfile is written.
#[tokio::test]
async fn bind_does_not_load_metadata_cache_from_disk() {
    let tmp = tempfile::tempdir().unwrap();
    let cache_dir = crate::core::NormalizedPath::new(tmp.path());
    let snapshot_path = crate::core::config::metadata_path_from_cache_dir(&cache_dir);

    // Pre-write a snapshot with a synthetic entry.
    let pre = crate::fscache::MetadataCache::new();
    let file_a = tmp.path().join("a.txt");
    let file_b = tmp.path().join("b.txt");
    std::fs::write(&file_a, b"file a").unwrap();
    std::fs::write(&file_b, b"file b").unwrap();
    pre.insert(
        crate::core::NormalizedPath::new(&file_a),
        synthetic_metadata([0xAA; 32]),
    );
    pre.insert(
        crate::core::NormalizedPath::new(&file_b),
        synthetic_metadata([0xBB; 32]),
    );
    pre.save_to_disk(snapshot_path.as_path()).unwrap();
    assert!(snapshot_path.as_path().exists());

    // Bind — must NOT read the snapshot (the load is deferred to the
    // background loader fired post-lockfile by the daemon binary).
    let endpoint = crate::ipc::unique_test_endpoint();
    let server = DaemonServer::bind_with_cache_dir(&endpoint, &cache_dir).unwrap();
    let state = server.test_state();
    assert_eq!(
        state.cache_system.metadata().len(),
        0,
        "bind must start with an empty metadata cache",
    );
    assert!(
        !state.metadata_cache_loaded.load(Ordering::Acquire),
        "the loaded flag must start false until the background loader runs",
    );
}

/// The `metadata_cache_loader()` handle reads the snapshot and merges
/// it into the live cache. Confirms the deferred-load path reaches
/// functional parity with the old sync-in-bind path.
#[tokio::test]
async fn metadata_cache_loader_merges_snapshot_into_live_cache() {
    let tmp = tempfile::tempdir().unwrap();
    let cache_dir = crate::core::NormalizedPath::new(tmp.path());
    let snapshot_path = crate::core::config::metadata_path_from_cache_dir(&cache_dir);

    let pre = crate::fscache::MetadataCache::new();
    let file = tmp.path().join("source.cpp");
    std::fs::write(&file, b"int main() {}").unwrap();
    let file_path = crate::core::NormalizedPath::new(&file);
    pre.insert(file_path.clone(), synthetic_metadata([0x42; 32]));
    pre.save_to_disk(snapshot_path.as_path()).unwrap();

    let endpoint = crate::ipc::unique_test_endpoint();
    let server = DaemonServer::bind_with_cache_dir(&endpoint, &cache_dir).unwrap();

    // Run the loader synchronously (it's `spawn_blocking`-safe but
    // calling it inline in the test is equivalent for observability).
    server.metadata_cache_loader().load_and_install();

    let state = server.test_state();
    assert_eq!(
        state.cache_system.metadata().len(),
        1,
        "loader must merge the persisted entry into the live cache",
    );
    assert!(
        state.metadata_cache_loaded.load(Ordering::Acquire),
        "loader must set metadata_cache_loaded=true so shutdown save fires",
    );
    assert!(
        state.cache_system.metadata().get(&file_path).is_some(),
        "the merged entry must be observable via the live cache API",
    );
}

/// Missing on-disk snapshot is not an error: the loader logs a warning,
/// leaves the live cache empty, and still flips the loaded flag so
/// shutdown save fires (and short-circuits because the cache is empty
/// per `save_to_disk`'s empty-cache early-exit).
#[tokio::test]
async fn metadata_cache_loader_tolerates_missing_snapshot() {
    let tmp = tempfile::tempdir().unwrap();
    let cache_dir = crate::core::NormalizedPath::new(tmp.path());
    let endpoint = crate::ipc::unique_test_endpoint();
    let server = DaemonServer::bind_with_cache_dir(&endpoint, &cache_dir).unwrap();

    server.metadata_cache_loader().load_and_install();

    let state = server.test_state();
    assert_eq!(state.cache_system.metadata().len(), 0);
    assert!(
        state.metadata_cache_loaded.load(Ordering::Acquire),
        "loaded flag flips even when the snapshot was absent",
    );
}

/// `merge_from` is `&self`, takes ownership of the loaded cache, and
/// drains all entries. Documents the contract the deferred loader
/// relies on.
#[test]
fn metadata_cache_merge_from_drains_other() {
    let tmp = tempfile::tempdir().unwrap();
    let a = tmp.path().join("a");
    let b = tmp.path().join("b");
    std::fs::write(&a, b"a").unwrap();
    std::fs::write(&b, b"b").unwrap();

    let live = crate::fscache::MetadataCache::new();
    live.insert(
        crate::core::NormalizedPath::new(&a),
        synthetic_metadata([1; 32]),
    );

    let loaded = crate::fscache::MetadataCache::new();
    loaded.insert(
        crate::core::NormalizedPath::new(&b),
        synthetic_metadata([2; 32]),
    );

    live.merge_from(loaded);

    assert_eq!(live.len(), 2);
    assert!(live.get(&crate::core::NormalizedPath::new(&a)).is_some());
    assert!(live.get(&crate::core::NormalizedPath::new(&b)).is_some());
}

fn synthetic_metadata(hash: [u8; 32]) -> crate::fscache::FileMetadata {
    crate::fscache::FileMetadata {
        mtime: std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000),
        size: 42,
        content_hash: Some(hash),
        confidence: crate::fscache::Confidence::High,
        last_verified: std::time::Instant::now(),
    }
}
