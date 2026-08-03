//! Inline-fixture tests for `cache_ops::warm_target` — focused on the
//! restoration mechanics (file paths, mtime, missing-payload handling,
//! missing-index error). Lockfile-driven filtering is exercised in
//! `warm_lockfile.rs`.

use super::super::cache_ops::warm_target;

#[derive(serde::Serialize)]
struct StagedManifestFixture {
    version: u32,
    key_hex: String,
    generation_hex: String,
    outputs: Vec<StagedOutputFixture>,
}

#[derive(serde::Serialize)]
struct StagedOutputFixture {
    index: usize,
    size: u64,
    digest_hex: String,
}

fn seed_staged_fixture(artifact_dir: &std::path::Path, key_hex: &str, payload: &[u8]) {
    let output = StagedOutputFixture {
        index: 0,
        size: payload.len() as u64,
        digest_hex: blake3::hash(payload).to_hex().to_string(),
    };
    let mut hasher = blake3::Hasher::new();
    hasher.update(key_hex.as_bytes());
    hasher.update(&output.index.to_le_bytes());
    hasher.update(&output.size.to_le_bytes());
    hasher.update(output.digest_hex.as_bytes());
    let generation_hex = hasher.finalize().to_hex().to_string();
    let generation_dir = artifact_dir
        .join(".staged-v2")
        .join(key_hex)
        .join(&generation_hex);
    std::fs::create_dir_all(&generation_dir).unwrap();
    std::fs::write(generation_dir.join("output-0"), payload).unwrap();
    std::fs::write(
        generation_dir.join("manifest.bin"),
        bincode::serialize(&StagedManifestFixture {
            version: 1,
            key_hex: key_hex.to_string(),
            generation_hex: generation_hex.clone(),
            outputs: vec![output],
        })
        .unwrap(),
    )
    .unwrap();
    std::fs::write(
        artifact_dir
            .join(".staged-v2")
            .join(format!("{key_hex}.current")),
        generation_hex,
    )
    .unwrap();
}

fn build_pack_fixture(payload: &[u8]) -> Vec<u8> {
    let mut pack = Vec::with_capacity(24 + payload.len());
    pack.extend_from_slice(b"ZCPK");
    pack.extend_from_slice(&1_u32.to_le_bytes());
    pack.extend_from_slice(&24_u64.to_le_bytes());
    pack.extend_from_slice(&(payload.len() as u64).to_le_bytes());
    pack.extend_from_slice(payload);
    pack
}

#[test]
fn warm_restores_rust_artifacts_to_correct_paths() {
    let dir = tempfile::tempdir().unwrap();
    let cache_dir = dir.path().join("cache");
    let artifact_dir = cache_dir.join("artifacts");
    let index_path = cache_dir.join("index.bin");
    let target_dir = dir.path().join("target");

    std::fs::create_dir_all(&artifact_dir).unwrap();

    // Create a fake artifact store with two Rust crates
    let store = crate::artifact::ArtifactStore::open(&index_path).unwrap();

    // Artifact 1: libserde-abc123.rlib + libserde-abc123.rmeta + serde-abc123.d
    let key1 = "aaaaaaaabbbbbbbb";
    let idx1 = crate::artifact::ArtifactIndex::new(
        vec![
            "libserde-abc123.rlib".to_string(),
            "libserde-abc123.rmeta".to_string(),
            "serde-abc123.d".to_string(),
        ],
        vec![12, 13, 8],
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
    let idx2 = crate::artifact::ArtifactIndex::new(
        vec!["libproc_macro2-def456.rlib".to_string()],
        vec![16],
        vec![],
        vec![],
        0,
    );
    store.insert(key2, &idx2);
    std::fs::write(artifact_dir.join(format!("{key2}_0")), b"proc-macro2-rlib").unwrap();

    // Artifact 3: NOT Rust (C++ object file) — should be filtered out
    let key3 = "eeeeeeeeffffffff";
    let idx3 =
        crate::artifact::ArtifactIndex::new(vec!["foo.o".to_string()], vec![11], vec![], vec![], 0);
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
fn warm_restores_staged_and_packed_artifacts() {
    let dir = tempfile::tempdir().unwrap();
    let cache_dir = dir.path().join("cache");
    let artifact_dir = cache_dir.join("artifacts");
    let index_path = cache_dir.join("index.bin");
    let target_dir = dir.path().join("target");
    std::fs::create_dir_all(&artifact_dir).unwrap();

    let staged_key = "1".repeat(64);
    let pack_key = "2".repeat(64);
    let staged_payload = b"staged-warm";
    let pack_payload = b"packed-warm";
    seed_staged_fixture(&artifact_dir, &staged_key, staged_payload);
    std::fs::write(
        artifact_dir.join(format!("{pack_key}.pack")),
        build_pack_fixture(pack_payload),
    )
    .unwrap();

    let store = crate::artifact::ArtifactStore::open(&index_path).unwrap();
    store.insert(
        &staged_key,
        &crate::artifact::ArtifactIndex::new(
            vec!["libstaged-warm.rlib".to_string()],
            vec![staged_payload.len() as u64],
            vec![],
            vec![],
            0,
        ),
    );
    store.insert(
        &pack_key,
        &crate::artifact::ArtifactIndex::new(
            vec!["libpacked-warm.rlib".to_string()],
            vec![pack_payload.len() as u64],
            vec![],
            vec![],
            0,
        ),
    );
    store.flush().unwrap();
    drop(store);

    let (restored, skipped, errors) =
        warm_target(&index_path, &artifact_dir, &target_dir, "debug", None).unwrap();
    assert_eq!((restored, skipped, errors), (2, 0, 0));
    let deps = target_dir.join("debug").join("deps");
    assert_eq!(
        std::fs::read(deps.join("libstaged-warm.rlib")).unwrap(),
        staged_payload
    );
    assert_eq!(
        std::fs::read(deps.join("libpacked-warm.rlib")).unwrap(),
        pack_payload
    );
}

#[test]
fn warm_skips_missing_payloads() {
    let dir = tempfile::tempdir().unwrap();
    let cache_dir = dir.path().join("cache");
    let artifact_dir = cache_dir.join("artifacts");
    let index_path = cache_dir.join("index.bin");
    let target_dir = dir.path().join("target");

    std::fs::create_dir_all(&artifact_dir).unwrap();

    let store = crate::artifact::ArtifactStore::open(&index_path).unwrap();
    let key = "1111111122222222";
    let idx = crate::artifact::ArtifactIndex::new(
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
        &dir.path().join("nonexistent.bin"),
        &dir.path().join("artifacts"),
        &dir.path().join("target"),
        "debug",
        None,
    );
    assert!(result.is_err());
}

/// `warm` runs in the CLI process while a daemon may be running GC, so it must
/// take the staged store's shared lock across resolve->materialize. Pinning it
/// via the exclusive side is what makes this test discriminate: without the
/// guard, `warm_target` completes immediately even while GC holds the store.
#[test]
fn warm_waits_for_the_staged_store_lock_before_materializing() {
    use std::sync::mpsc;

    let dir = tempfile::tempdir().unwrap();
    let artifact_dir = dir.path().join("artifacts");
    let target_dir = dir.path().join("target");
    let index_path = dir.path().join("index.bin");
    std::fs::create_dir_all(&artifact_dir).unwrap();

    let key = "aaaaaaaabbbbbbbb";
    let store = crate::artifact::ArtifactStore::open(&index_path).unwrap();
    store.insert(
        key,
        &crate::artifact::ArtifactIndex::new(
            vec!["libstaged-abc123.rlib".to_string()],
            vec![b"staged-rlib".len() as u64],
            vec![],
            vec![],
            0,
        ),
    );
    store.flush().unwrap();
    drop(store);
    seed_staged_fixture(&artifact_dir, key, b"staged-rlib");

    // Stand in for daemon maintenance holding the store exclusively.
    let staged = zccache_artifact::staged_lock::staged_root(&artifact_dir);
    let gc_lock = zccache_artifact::staged_lock::open_store_lock(staged.as_path()).unwrap();
    fs2::FileExt::lock_exclusive(&gc_lock).unwrap();

    // The outcome travels through the channel, not just a unit tick, so that an
    // early `Err` return (which would also unblock the recv and look exactly
    // like "warm ignored the lock") names itself instead of failing as a
    // mystery.
    let (tx, rx) = mpsc::channel();
    let warm = std::thread::spawn(move || {
        let outcome = warm_target(&index_path, &artifact_dir, &target_dir, "debug", None);
        tx.send(outcome.clone()).unwrap();
        outcome
    });

    if let Ok(early) = rx.recv_timeout(std::time::Duration::from_millis(250)) {
        panic!(
            "warm returned {early:?} while the staged store was held exclusively; \
             it must block instead -- returning here means it either materialized \
             without the guard or failed before reaching it"
        );
    }

    drop(gc_lock);

    let outcome = rx
        .recv_timeout(std::time::Duration::from_secs(30))
        .expect("warm must proceed once the exclusive lock is released");
    warm.join().unwrap().ok();
    let (restored, _skipped, errors) =
        outcome.expect("warm should succeed once it can take the lock");
    assert_eq!(errors, 0, "warm should succeed after acquiring the lock");
    assert_eq!(restored, 1, "the staged payload should be restored");
}
