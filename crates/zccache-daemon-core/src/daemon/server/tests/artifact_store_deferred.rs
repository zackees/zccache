//! Tests for the issue #784 phase 2d deferred-load path on the
//! `ArtifactStore` index blob. Companion to the on-disk-fallback test
//! at `tests/daemon_perf_artifact_fallback_test.rs` — these cover the
//! deferred-load invariants, the perf test covers the in-process
//! disk-fallback contract.

use super::super::*;

/// `bind_with_cache_dir` no longer reads the on-disk `index.bin` blob.
/// The store starts empty regardless of what is on disk.
#[tokio::test]
async fn bind_does_not_load_artifact_index_from_disk() {
    let tmp = tempfile::tempdir().unwrap();
    let cache_dir = crate::core::NormalizedPath::new(tmp.path());
    let index_path = crate::core::config::index_path_from_cache_dir(&cache_dir);

    // Pre-write an index blob with synthetic entries.
    let pre = crate::artifact::ArtifactStore::open(index_path.as_path()).unwrap();
    pre.insert("key-a", &synthetic_index_entry(7));
    pre.insert("key-b", &synthetic_index_entry(11));
    pre.flush().unwrap();
    assert!(index_path.as_path().exists());

    // Bind — must NOT read the blob.
    let endpoint = crate::ipc::unique_test_endpoint();
    let server = DaemonServer::bind_with_cache_dir(&endpoint, &cache_dir).unwrap();
    let state = server.test_state();
    assert!(
        state.artifact_store.get("key-a").is_none(),
        "bind must start with an empty artifact store (no key-a)",
    );
    assert!(
        state.artifact_store.get("key-b").is_none(),
        "bind must start with an empty artifact store (no key-b)",
    );
    assert!(
        !state.artifact_store_loaded.load(Ordering::Acquire),
        "the loaded flag must start false until the background loader runs",
    );
}

/// The background loader reads the blob and merges its entries into
/// the live store, then flips the loaded flag.
#[tokio::test]
async fn artifact_store_loader_merges_index_into_live_store() {
    let tmp = tempfile::tempdir().unwrap();
    let cache_dir = crate::core::NormalizedPath::new(tmp.path());
    let index_path = crate::core::config::index_path_from_cache_dir(&cache_dir);

    let pre = crate::artifact::ArtifactStore::open(index_path.as_path()).unwrap();
    pre.insert("hot-key", &synthetic_index_entry(0x42));
    pre.flush().unwrap();

    let endpoint = crate::ipc::unique_test_endpoint();
    let server = DaemonServer::bind_with_cache_dir(&endpoint, &cache_dir).unwrap();

    server.artifact_store_loader().load_and_install();

    let state = server.test_state();
    let entry = state
        .artifact_store
        .get("hot-key")
        .expect("loader must merge the persisted entry into the live store");
    assert_eq!(entry.total_size, 0x42);
    assert!(
        state.artifact_store_loaded.load(Ordering::Acquire),
        "loader must set artifact_store_loaded=true",
    );
}

/// Missing on-disk blob is not an error: the loader is a no-op and
/// still flips the loaded flag.
#[tokio::test]
async fn artifact_store_loader_tolerates_missing_index_file() {
    let tmp = tempfile::tempdir().unwrap();
    let cache_dir = crate::core::NormalizedPath::new(tmp.path());
    let endpoint = crate::ipc::unique_test_endpoint();
    let server = DaemonServer::bind_with_cache_dir(&endpoint, &cache_dir).unwrap();

    server.artifact_store_loader().load_and_install();

    let state = server.test_state();
    assert!(state.artifact_store.get("any").is_none());
    assert!(
        state.artifact_store_loaded.load(Ordering::Acquire),
        "loaded flag flips even when the blob was absent",
    );
}

/// `ArtifactStore::open_empty` produces a usable store rooted at the
/// given path without touching disk.
#[test]
fn artifact_store_open_empty_does_not_touch_disk() {
    let tmp = tempfile::tempdir().unwrap();
    let missing_path = tmp.path().join("does-not-exist").join("index.bin");

    let store = crate::artifact::ArtifactStore::open_empty(&missing_path);

    assert!(store.get("anything").is_none());
    assert!(
        !missing_path.exists(),
        "open_empty must not create the index file",
    );
}

/// **Key correctness property for #794-v2**: if a lookup races ahead
/// of the background loader, it triggers a synchronous `load_from_disk`
/// on the spot. Mirrors the property the existing perf test
/// `perf_artifact_lookup_hits_before_background_load_completes` locks
/// in, but at the unit-test level so a future refactor of
/// `lookup_artifact_with_disk_fallback` can't drop it silently.
#[tokio::test]
async fn lookup_triggers_synchronous_load_when_background_load_pending() {
    let tmp = tempfile::tempdir().unwrap();
    let cache_dir = crate::core::NormalizedPath::new(tmp.path());
    let index_path = crate::core::config::index_path_from_cache_dir(&cache_dir);

    // Seed a payload + index entry on disk (so the daemon would have
    // hit if it had loaded; the test asserts it hits even WITHOUT the
    // background loader running).
    let artifact_dir = crate::core::config::artifacts_dir_from_cache_dir(&cache_dir);
    std::fs::create_dir_all(artifact_dir.as_path()).unwrap();
    let key_hex = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";
    let payload: &[u8] = b"abcd";
    let payload_path = artifact_dir.join(format!("{key_hex}_0"));
    std::fs::write(payload_path.as_path(), payload).unwrap();

    let pre = crate::artifact::ArtifactStore::open(index_path.as_path()).unwrap();
    pre.insert(
        key_hex,
        &crate::artifact::ArtifactIndex::new(
            vec!["foo.o".to_string()],
            vec![payload.len() as u64],
            b"".to_vec(),
            b"".to_vec(),
            0,
        ),
    );
    pre.flush().unwrap();
    drop(pre);

    let endpoint = crate::ipc::unique_test_endpoint();
    let server = DaemonServer::bind_with_cache_dir(&endpoint, &cache_dir).unwrap();
    let state = server.test_state();

    // Confirm preconditions: empty store, loaded flag false (background
    // loader has NOT been called).
    assert!(state.artifact_store.get(key_hex).is_none());
    assert!(!state.artifact_store_loaded.load(Ordering::Acquire));

    // The test seam. Internally calls
    // `lookup_artifact_with_disk_fallback`, which should detect the
    // loaded flag is false and call `load_from_disk` synchronously.
    let found = server.test_lookup_artifact(key_hex);
    assert!(
        found,
        "lookup must hit even when background loader hasn't run yet \
         — the helper itself triggers load_from_disk on the spot",
    );

    // Postcondition: the synchronous load flipped the flag, so a
    // subsequent miss on a different key won't redundantly hit disk.
    assert!(
        state.artifact_store_loaded.load(Ordering::Acquire),
        "synchronous fallback load must flip the loaded flag",
    );
}

fn synthetic_index_entry(total_size: u64) -> crate::artifact::ArtifactIndex {
    use std::sync::Arc;
    crate::artifact::ArtifactIndex {
        output_names: Arc::from(vec!["foo.o".to_string()]),
        output_sizes: vec![total_size],
        stdout: Arc::new(Vec::new()),
        stderr: Arc::new(Vec::new()),
        exit_code: 0,
        total_size,
        stored_at_secs: 0,
    }
}
