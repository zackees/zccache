//! Lockfile-driven filter tests for `warm_target` plus adversarial
//! scenarios (crate removed, stale file, version bump, corrupted payload,
//! empty lockfile). Helpers `make_test_store` and `write_lockfile` live
//! here because they are only used by these tests.

use std::path::{Path, PathBuf};

use super::super::cache_ops::{artifact_matches_lockfile, parse_lockfile_crates, warm_target};

// ── Helper: create a fake artifact store with test data ──────

fn make_test_store(dir: &Path) -> (PathBuf, PathBuf) {
    let cache_dir = dir.join("cache");
    let artifact_dir = cache_dir.join("artifacts");
    let index_path = cache_dir.join("index.bin");
    std::fs::create_dir_all(&artifact_dir).unwrap();

    let store = zccache_monocrate::artifact::ArtifactStore::open(&index_path).unwrap();

    // serde (in a typical Cargo.lock)
    let k1 = "aaaa0001";
    store.insert(
        k1,
        &zccache_monocrate::artifact::ArtifactIndex::new(
            vec![
                "libserde-abc123.rlib".into(),
                "libserde-abc123.rmeta".into(),
                "serde-abc123.d".into(),
            ],
            vec![100, 50, 10],
            vec![],
            vec![],
            0,
        ),
    );
    std::fs::write(artifact_dir.join(format!("{k1}_0")), b"serde-rlib").unwrap();
    std::fs::write(artifact_dir.join(format!("{k1}_1")), b"serde-rmeta").unwrap();
    std::fs::write(artifact_dir.join(format!("{k1}_2")), b"serde-d").unwrap();

    // proc-macro2 (hyphen → underscore in filename)
    let k2 = "aaaa0002";
    store.insert(
        k2,
        &zccache_monocrate::artifact::ArtifactIndex::new(
            vec!["libproc_macro2-def456.rlib".into()],
            vec![200],
            vec![],
            vec![],
            0,
        ),
    );
    std::fs::write(artifact_dir.join(format!("{k2}_0")), b"proc-macro2-rlib").unwrap();

    // tokio (NOT in our test lockfile)
    let k3 = "aaaa0003";
    store.insert(
        k3,
        &zccache_monocrate::artifact::ArtifactIndex::new(
            vec!["libtokio-ghi789.rlib".into()],
            vec![300],
            vec![],
            vec![],
            0,
        ),
    );
    std::fs::write(artifact_dir.join(format!("{k3}_0")), b"tokio-rlib").unwrap();

    // C++ object file (no crate name pattern)
    let k4 = "aaaa0004";
    store.insert(
        k4,
        &zccache_monocrate::artifact::ArtifactIndex::new(vec!["foo.o".into()], vec![50], vec![], vec![], 0),
    );
    std::fs::write(artifact_dir.join(format!("{k4}_0")), b"cpp-object").unwrap();

    store.flush().unwrap();
    drop(store);
    (index_path, artifact_dir)
}

fn write_lockfile(dir: &Path, crates: &[&str]) -> PathBuf {
    let lockfile = dir.join("Cargo.lock");
    let mut content = String::from("# This file is automatically @generated\nversion = 3\n\n");
    for name in crates {
        content.push_str(&format!(
            "[[package]]\nname = \"{name}\"\nversion = \"1.0.0\"\n\n"
        ));
    }
    std::fs::write(&lockfile, &content).unwrap();
    lockfile
}

// ── Lockfile parsing tests ───────────────────────────────────

#[test]
fn parse_lockfile_extracts_crate_names() {
    let dir = tempfile::tempdir().unwrap();
    let lf = write_lockfile(dir.path(), &["serde", "proc-macro2", "unicode-ident"]);
    let crates = parse_lockfile_crates(&lf).unwrap();
    assert!(crates.contains("serde"));
    assert!(
        crates.contains("proc_macro2"),
        "hyphens should be underscores"
    );
    assert!(crates.contains("unicode_ident"));
    assert!(!crates.contains("tokio"), "tokio not in lockfile");
}

#[test]
fn artifact_matches_lockfile_basic() {
    let mut allowed = std::collections::HashSet::new();
    allowed.insert("serde".to_string());
    allowed.insert("proc_macro2".to_string());

    assert!(artifact_matches_lockfile("libserde-abc123.rlib", &allowed));
    assert!(artifact_matches_lockfile("libserde-abc123.rmeta", &allowed));
    assert!(artifact_matches_lockfile("serde-abc123.d", &allowed));
    assert!(artifact_matches_lockfile(
        "libproc_macro2-def456.rlib",
        &allowed
    ));
    assert!(!artifact_matches_lockfile("libtokio-ghi789.rlib", &allowed));
    // No hash separator → allowed (could be build script output)
    assert!(artifact_matches_lockfile("build_script_build", &allowed));
}

// ── Strategy tests ───────────────────────────────────────────

#[test]
fn warm_without_lockfile_restores_everything() {
    let dir = tempfile::tempdir().unwrap();
    let (index_path, artifact_dir) = make_test_store(dir.path());
    let target_dir = dir.path().join("target");

    let (restored, _, _) =
        warm_target(&index_path, &artifact_dir, &target_dir, "debug", None).unwrap();

    let deps = target_dir.join("debug").join("deps");
    assert_eq!(restored, 6, "without lockfile: restore all 6 files");
    assert!(deps.join("libserde-abc123.rlib").exists());
    assert!(
        deps.join("libtokio-ghi789.rlib").exists(),
        "tokio restored without filter"
    );
    assert!(
        deps.join("foo.o").exists(),
        "C++ file restored without filter"
    );
}

#[test]
fn warm_with_lockfile_filters_to_matching_crates() {
    let dir = tempfile::tempdir().unwrap();
    let (index_path, artifact_dir) = make_test_store(dir.path());
    let target_dir = dir.path().join("target");
    let lockfile = write_lockfile(dir.path(), &["serde", "proc-macro2"]);

    let (restored, skipped, _) = warm_target(
        &index_path,
        &artifact_dir,
        &target_dir,
        "debug",
        Some(&lockfile),
    )
    .unwrap();

    let deps = target_dir.join("debug").join("deps");
    // serde (3) + proc-macro2 (1) + foo.o (1, no hash separator = allowed)
    assert_eq!(restored, 5);
    assert!(deps.join("libserde-abc123.rlib").exists());
    assert!(deps.join("libproc_macro2-def456.rlib").exists());
    assert!(
        !deps.join("libtokio-ghi789.rlib").exists(),
        "tokio NOT in lockfile"
    );
    assert!(
        deps.join("foo.o").exists(),
        "no hash separator = allowed through"
    );
    assert!(skipped > 0, "tokio should be skipped");
}

// ── Adversarial tests ────────────────────────────────────────

#[test]
fn adversarial_crate_removed_from_lockfile() {
    // Scenario: tokio was in the cache from a previous build,
    // but was removed from Cargo.toml/Cargo.lock.
    // Warm should NOT restore it.
    let dir = tempfile::tempdir().unwrap();
    let (index_path, artifact_dir) = make_test_store(dir.path());
    let target_dir = dir.path().join("target");
    // Lockfile has serde but NOT tokio
    let lockfile = write_lockfile(dir.path(), &["serde"]);

    let (restored, _, _) = warm_target(
        &index_path,
        &artifact_dir,
        &target_dir,
        "debug",
        Some(&lockfile),
    )
    .unwrap();

    let deps = target_dir.join("debug").join("deps");
    assert!(deps.join("libserde-abc123.rlib").exists());
    assert!(
        !deps.join("libtokio-ghi789.rlib").exists(),
        "removed crate must NOT be restored"
    );
    // serde (3) + foo.o (1, no hash separator = allowed)
    assert_eq!(restored, 4);
}

#[test]
fn adversarial_stale_file_in_target_from_previous_warm() {
    // Scenario: previous warm restored tokio. Then tokio was removed
    // from Cargo.lock. New warm runs — does it leave the stale file?
    // Answer: yes, warm doesn't delete. But cargo ignores unknown files.
    let dir = tempfile::tempdir().unwrap();
    let (index_path, artifact_dir) = make_test_store(dir.path());
    let target_dir = dir.path().join("target");
    let deps = target_dir.join("debug").join("deps");
    std::fs::create_dir_all(&deps).unwrap();

    // Simulate stale file from previous warm
    std::fs::write(deps.join("libtokio-ghi789.rlib"), b"stale").unwrap();

    // Now warm with lockfile that excludes tokio
    let lockfile = write_lockfile(dir.path(), &["serde"]);
    warm_target(
        &index_path,
        &artifact_dir,
        &target_dir,
        "debug",
        Some(&lockfile),
    )
    .unwrap();

    // Stale file still there (warm doesn't delete)
    assert!(
        deps.join("libtokio-ghi789.rlib").exists(),
        "warm doesn't clean up stale files — cargo ignores them"
    );
    // But it wasn't overwritten with fresh content
    assert_eq!(
        std::fs::read(deps.join("libtokio-ghi789.rlib")).unwrap(),
        b"stale",
        "stale file content unchanged"
    );
}

#[test]
fn adversarial_version_bump_old_artifact_in_cache() {
    // Scenario: cache has serde 1.0.227 artifacts, but Cargo.lock
    // now requires serde 1.0.228. The old artifacts have different
    // hashes in the filename so they won't conflict.
    let dir = tempfile::tempdir().unwrap();
    let cache_dir = dir.path().join("cache");
    let artifact_dir = cache_dir.join("artifacts");
    let index_path = cache_dir.join("index.bin");
    std::fs::create_dir_all(&artifact_dir).unwrap();

    let store = zccache_monocrate::artifact::ArtifactStore::open(&index_path).unwrap();

    // Old version's artifact (different hash suffix)
    let k_old = "bbbb0001";
    store.insert(
        k_old,
        &zccache_monocrate::artifact::ArtifactIndex::new(
            vec!["libserde-old111.rlib".into()],
            vec![100],
            vec![],
            vec![],
            0,
        ),
    );
    std::fs::write(artifact_dir.join(format!("{k_old}_0")), b"old-serde").unwrap();

    // New version's artifact (different hash suffix)
    let k_new = "bbbb0002";
    store.insert(
        k_new,
        &zccache_monocrate::artifact::ArtifactIndex::new(
            vec!["libserde-new222.rlib".into()],
            vec![100],
            vec![],
            vec![],
            0,
        ),
    );
    std::fs::write(artifact_dir.join(format!("{k_new}_0")), b"new-serde").unwrap();

    store.flush().unwrap();
    drop(store);

    let target_dir = dir.path().join("target");
    let lockfile = write_lockfile(dir.path(), &["serde"]);

    let (restored, _, _) = warm_target(
        &index_path,
        &artifact_dir,
        &target_dir,
        "debug",
        Some(&lockfile),
    )
    .unwrap();

    let deps = target_dir.join("debug").join("deps");
    // Both old and new are restored — cargo will use the one matching
    // its own fingerprint and ignore the other
    assert_eq!(restored, 2);
    assert!(deps.join("libserde-old111.rlib").exists());
    assert!(deps.join("libserde-new222.rlib").exists());
    // This is safe: cargo only links the artifact matching its
    // fingerprint hash. The extra file wastes ~100 bytes of disk.
}

#[test]
fn adversarial_corrupted_cache_file() {
    // Scenario: artifact payload on disk is corrupted (truncated).
    // Warm restores it, cargo tries to use it, gets an error,
    // and recompiles from scratch. Verify warm doesn't crash.
    let dir = tempfile::tempdir().unwrap();
    let cache_dir = dir.path().join("cache");
    let artifact_dir = cache_dir.join("artifacts");
    let index_path = cache_dir.join("index.bin");
    std::fs::create_dir_all(&artifact_dir).unwrap();

    let store = zccache_monocrate::artifact::ArtifactStore::open(&index_path).unwrap();
    let key = "cccc0001";
    store.insert(
        key,
        &zccache_monocrate::artifact::ArtifactIndex::new(
            vec!["libserde-abc123.rlib".into()],
            vec![1000], // Claims 1000 bytes
            vec![],
            vec![],
            0,
        ),
    );
    // But payload is only 5 bytes (corrupted/truncated)
    std::fs::write(artifact_dir.join(format!("{key}_0")), b"short").unwrap();
    store.flush().unwrap();
    drop(store);

    let target_dir = dir.path().join("target");
    let (restored, _, errors) =
        warm_target(&index_path, &artifact_dir, &target_dir, "debug", None).unwrap();

    // Warm restores it without error (it doesn't validate content)
    assert_eq!(restored, 1);
    assert_eq!(errors, 0);
    // Cargo will detect the corruption via its own hash check and rebuild
    let deps = target_dir.join("debug").join("deps");
    assert_eq!(
        std::fs::read(deps.join("libserde-abc123.rlib")).unwrap(),
        b"short"
    );
}

#[test]
fn adversarial_empty_lockfile() {
    // Edge case: Cargo.lock exists but has no packages
    let dir = tempfile::tempdir().unwrap();
    let (index_path, artifact_dir) = make_test_store(dir.path());
    let target_dir = dir.path().join("target");
    let lockfile = write_lockfile(dir.path(), &[]);

    let (restored, skipped, _) = warm_target(
        &index_path,
        &artifact_dir,
        &target_dir,
        "debug",
        Some(&lockfile),
    )
    .unwrap();

    // foo.o has no hash separator → allowed through. Everything else skipped.
    assert_eq!(restored, 1, "only foo.o (no hash separator) passes");
    assert!(skipped > 0);
}
