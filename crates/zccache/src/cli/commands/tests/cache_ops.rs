//! Inline-fixture tests for `cache_ops::warm_target` — focused on the
//! restoration mechanics (file paths, mtime, missing-payload handling,
//! missing-index error). Lockfile-driven filtering is exercised in
//! `warm_lockfile.rs`.

use super::super::cache_ops::warm_target;

#[test]
fn warm_restores_rust_artifacts_to_correct_paths() {
    let dir = tempfile::tempdir().unwrap();
    let cache_dir = dir.path().join("cache");
    let artifact_dir = cache_dir.join("artifacts");
    let index_path = cache_dir.join("index.bin");
    let target_dir = dir.path().join("target");

    std::fs::create_dir_all(&artifact_dir).unwrap();

    // Create a fake artifact store with two Rust crates
    let store = zccache::artifact::ArtifactStore::open(&index_path).unwrap();

    // Artifact 1: libserde-abc123.rlib + libserde-abc123.rmeta + serde-abc123.d
    let key1 = "aaaaaaaabbbbbbbb";
    let idx1 = zccache::artifact::ArtifactIndex::new(
        vec![
            "libserde-abc123.rlib".to_string(),
            "libserde-abc123.rmeta".to_string(),
            "serde-abc123.d".to_string(),
        ],
        vec![100, 50, 10],
        vec![],
        vec![],
        0,
    );
    store.insert(key1, &idx1);
    // Write payload files on disk
    std::fs::write(artifact_dir.join(format!("{key1}_0")), b"rlib-content").unwrap();
    std::fs::write(artifact_dir.join(format!("{key1}_1")), b"rmeta-content").unwrap();
    std::fs::write(artifact_dir.join(format!("{key1}_2")), b"dep-info").unwrap();

    // Artifact 2: libproc_macro2-def456.rlib
    let key2 = "ccccccccdddddddd";
    let idx2 = zccache::artifact::ArtifactIndex::new(
        vec!["libproc_macro2-def456.rlib".to_string()],
        vec![200],
        vec![],
        vec![],
        0,
    );
    store.insert(key2, &idx2);
    std::fs::write(artifact_dir.join(format!("{key2}_0")), b"proc-macro2-rlib").unwrap();

    // Artifact 3: NOT Rust (C++ object file) — should be filtered out
    let key3 = "eeeeeeeeffffffff";
    let idx3 = zccache::artifact::ArtifactIndex::new(
        vec!["foo.o".to_string()],
        vec![300],
        vec![],
        vec![],
        0,
    );
    store.insert(key3, &idx3);
    std::fs::write(artifact_dir.join(format!("{key3}_0")), b"object-file").unwrap();

    store.flush().unwrap();
    store.flush().unwrap();
    drop(store);

    // Run warm
    let (restored, skipped, errors) =
        warm_target(&index_path, &artifact_dir, &target_dir, "debug", None).unwrap();

    // Verify counts
    assert_eq!(errors, 0, "should have 0 errors");
    assert_eq!(
        restored, 5,
        "should restore all 5 files (3 serde + 1 proc_macro2 + 1 C++ .o)"
    );
    assert_eq!(skipped, 0, "all payloads exist on disk");

    // Verify files exist at correct paths
    let deps = target_dir.join("debug").join("deps");
    assert!(
        deps.join("libserde-abc123.rlib").exists(),
        "serde rlib missing"
    );
    assert!(
        deps.join("libserde-abc123.rmeta").exists(),
        "serde rmeta missing"
    );
    assert!(
        deps.join("serde-abc123.d").exists(),
        "serde dep-info missing"
    );
    assert!(
        deps.join("libproc_macro2-def456.rlib").exists(),
        "proc_macro2 rlib missing"
    );

    // Verify content is correct
    assert_eq!(
        std::fs::read(deps.join("libserde-abc123.rlib")).unwrap(),
        b"rlib-content"
    );
    assert_eq!(
        std::fs::read(deps.join("libproc_macro2-def456.rlib")).unwrap(),
        b"proc-macro2-rlib"
    );

    // Verify C++ artifact IS restored (warm restores everything, not just Rust)
    assert!(
        deps.join("foo.o").exists(),
        "C++ .o file should also be in deps/"
    );
    assert_eq!(std::fs::read(deps.join("foo.o")).unwrap(), b"object-file");

    // Verify mtime is recent (within 5 seconds)
    let meta = std::fs::metadata(deps.join("libserde-abc123.rlib")).unwrap();
    let age = meta.modified().unwrap().elapsed().unwrap();
    assert!(age.as_secs() < 5, "mtime should be fresh, got {age:?}");
}

#[test]
fn warm_skips_missing_payloads() {
    let dir = tempfile::tempdir().unwrap();
    let cache_dir = dir.path().join("cache");
    let artifact_dir = cache_dir.join("artifacts");
    let index_path = cache_dir.join("index.bin");
    let target_dir = dir.path().join("target");

    std::fs::create_dir_all(&artifact_dir).unwrap();

    let store = zccache::artifact::ArtifactStore::open(&index_path).unwrap();
    let key = "1111111122222222";
    let idx = zccache::artifact::ArtifactIndex::new(
        vec!["libfoo-xyz.rlib".to_string()],
        vec![100],
        vec![],
        vec![],
        0,
    );
    store.insert(key, &idx);
    // DON'T write the payload file — simulate missing artifact on disk
    store.flush().unwrap();
    drop(store);

    let (restored, skipped, errors) =
        warm_target(&index_path, &artifact_dir, &target_dir, "debug", None).unwrap();

    assert_eq!(restored, 0);
    assert_eq!(skipped, 1, "should skip 1 missing payload");
    assert_eq!(errors, 0);
}

#[test]
fn warm_returns_error_on_missing_index() {
    let dir = tempfile::tempdir().unwrap();
    let result = warm_target(
        &dir.path().join("nonexistent.redb"),
        &dir.path().join("artifacts"),
        &dir.path().join("target"),
        "debug",
        None,
    );
    assert!(result.is_err());
}
