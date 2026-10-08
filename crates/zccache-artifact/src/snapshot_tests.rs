//! Compiler snapshot transport and destination preservation controls.

use super::*;
use crate::{layout_fixtures, resolve_staged_artifact_files, ArtifactIndex, ArtifactVerdict};
use std::{collections::BTreeMap, fs, sync::Arc};

fn fixture() -> (tempfile::TempDir, ArtifactStore, String) {
    let temp = tempfile::tempdir().unwrap();
    let key = "a".repeat(64);
    layout_fixtures::seed_staged_generation(
        &temp.path().join("source/artifacts"),
        &key,
        &[b"obj"],
    );
    let store = ArtifactStore::open_empty(&temp.path().join("source/index.bin"));
    store.insert(
        &key,
        &ArtifactIndex::new(vec!["unit.o".into()], vec![3], vec![], vec![], 0),
    );
    (temp, store, key)
}

#[test]
fn overlapping_nested_destination_does_not_create_directories_in_source() {
    let (temp, store, _) = fixture();
    let compatibility = "b".repeat(64);
    let snapshot = temp.path().join("snapshot");
    export_snapshot(&store, &temp.path().join("source/artifacts"),
        &compatibility, &snapshot).unwrap();
    let before = file_identity(&snapshot);
    let parent = snapshot.join("new-parent");
    assert!(import_snapshot(&snapshot, &compatibility, &parent.join("store")).is_err());
    assert!(!parent.exists());
    assert_eq!(file_identity(&snapshot), before);
    let unresolved = temp.path().join("missing");
    let escaped = unresolved.join("../snapshot/new-store");
    assert!(import_snapshot(&snapshot, &compatibility, &escaped).is_err());
    assert!(!unresolved.exists());
    assert!(!snapshot.join("new-store").exists());
    assert_eq!(file_identity(&snapshot), before);
}

#[test]
fn interrupted_import_preserves_index_and_retries_readonly_generations() {
    let (temp, store, _) = fixture();
    let compatibility = "b".repeat(64);
    let snapshot = temp.path().join("snapshot");
    let artifacts = temp.path().join("source/artifacts");
    let destination = temp.path().join("restored");
    export_snapshot(&store, &artifacts, &compatibility, &snapshot).unwrap();
    import_snapshot(&snapshot, &compatibility, &destination).unwrap();
    let before = fs::read(destination.join("index.bin")).unwrap();
    let key = "c".repeat(64);
    layout_fixtures::seed_staged_generation(&artifacts, &key, &[b"readonly"]);
    let files = resolve_staged_artifact_files(&artifacts, &key, &[8]).unwrap().unwrap();
    let mut permissions = fs::metadata(&files[0]).unwrap().permissions();
    permissions.set_readonly(true);
    fs::set_permissions(&files[0], permissions).unwrap();
    store.insert(&key, &ArtifactIndex::new(
        vec!["readonly.o".into()], vec![8], vec![], vec![], 0,
    ));
    let update = temp.path().join("update");
    export_snapshot(&store, &artifacts, &compatibility, &update).unwrap();
    let error = import_snapshot_before_commit(&update, &compatibility, &destination, || {
        Err(io::Error::new(io::ErrorKind::Interrupted, "publication interrupted"))
    }).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::Interrupted);
    assert_eq!(fs::read(destination.join("index.bin")).unwrap(), before);
    import_snapshot(&update, &compatibility, &destination).unwrap();
    let files = resolve_staged_artifact_files(
        &destination.join("artifacts"), &key, &[8],
    ).unwrap().unwrap();
    assert_eq!(fs::read(&files[0]).unwrap(), b"readonly");
    assert!(fs::metadata(&files[0]).unwrap().permissions().readonly());
}

#[test]
fn repeated_import_preserves_existing_rows_and_adds_new_artifacts() {
    let (temp, source, key) = fixture();
    let compatibility = "b".repeat(64);
    let first = temp.path().join("first-snapshot");
    let destination = temp.path().join("restored");
    let artifacts = temp.path().join("source/artifacts");
    export_snapshot(&source, &artifacts, &compatibility, &first).unwrap();
    import_snapshot(&first, &compatibility, &destination).unwrap();

    let existing = ArtifactStore::open_empty(&destination.join("index.bin"));
    existing.load_from_disk().unwrap();
    let local_key = "c".repeat(64);
    layout_fixtures::seed_staged_generation(
        &destination.join("artifacts"), &local_key, &[b"local"],
    );
    existing.insert(&local_key, &ArtifactIndex::new(
        vec!["local.o".into()], vec![5], vec![], vec![], 0,
    ));
    existing.flush().unwrap();
    let before = fs::read(resolve_staged_artifact_files(
        &destination.join("artifacts"), &key, &[3],
    ).unwrap().unwrap()[0].as_path()).unwrap();

    let added_key = "d".repeat(64);
    layout_fixtures::seed_staged_generation(&artifacts, &added_key, &[b"new"]);
    source.insert(&added_key, &ArtifactIndex::new(
        vec!["new.o".into()], vec![3], vec![], vec![], 0,
    ));
    // An older imported record must not replace a locally retained key.
    layout_fixtures::seed_staged_generation(&artifacts, &key, &[b"old"]);
    let second = temp.path().join("second-snapshot");
    export_snapshot(&source, &artifacts, &compatibility, &second).unwrap();
    let source_before = file_identity(&second);
    import_snapshot(&second, &compatibility, &destination).unwrap();
    import_snapshot(&second, &compatibility, &destination).unwrap();
    let restored = ArtifactStore::open_empty(&destination.join("index.bin"));
    restored.load_from_disk().unwrap();
    let mut keys: Vec<_> = restored.load_all().into_iter().map(|(key, _)| key).collect();
    keys.sort();
    assert_eq!(keys, vec![key.clone(), local_key.clone(), added_key.clone()]);
    for (key, size, expected) in [
        (key, 3, before.as_slice()),
        (local_key, 5, b"local".as_slice()),
        (added_key, 3, b"new".as_slice()),
    ] {
        let paths = resolve_staged_artifact_files(
            &destination.join("artifacts"), &key, &[size],
        ).unwrap().unwrap();
        assert_eq!(fs::read(paths[0].as_path()).unwrap(), expected);
    }
    assert_eq!(file_identity(&second), source_before);
}

#[test]
fn import_refuses_an_existing_store_writer_without_changing_its_files() {
    let (temp, source, _) = fixture();
    let compatibility = "b".repeat(64);
    let snapshot = temp.path().join("snapshot");
    let destination = temp.path().join("restored");
    export_snapshot(&source, &temp.path().join("source/artifacts"),
        &compatibility, &snapshot).unwrap();
    import_snapshot(&snapshot, &compatibility, &destination).unwrap();
    let _writer = zccache_core::cache_root_lock::CacheRootWriterLock::acquire(
        &destination,
    ).unwrap();
    let before = file_identity(&destination);
    assert_eq!(import_snapshot(&snapshot, &compatibility, &destination)
        .unwrap_err().kind(), io::ErrorKind::WouldBlock);
    let after = file_identity(&destination);
    // Writer contention is recorded in the store's lifecycle log.
    // The committed compiler data must remain byte-identical.
    for (path, bytes) in before {
        if !path.starts_with("logs/") {
            assert_eq!(after.get(&path), Some(&bytes), "{path}");
        }
    }
}

#[test]
fn store_context_corruption_is_refused_without_publication_or_source_mutation() {
    for mode in ["digest", "missing", "format"] {
        let (temp, store, _) = fixture();
        let root = temp.path().join("source");
        store.flush().unwrap();
        fs::create_dir(root.join("depgraph")).unwrap();
        zccache_depgraph::save_to_file(
            &zccache_depgraph::DepGraph::new(),
            &root.join("depgraph/depgraph.bin"),
        )
        .unwrap();
        let snapshot = temp.path().join("snapshot");
        let compatibility = "b".repeat(64);
        export_store_snapshot(&root, &compatibility, &snapshot).unwrap();
        let graph = snapshot.join("depgraph/depgraph.bin");
        if mode == "missing" {
            fs::remove_file(&graph).unwrap();
        } else {
            fs::write(&graph, b"invalid dependency context").unwrap();
            if mode == "format" {
                let path = snapshot.join("snapshot.json");
                let mut manifest: SnapshotManifest =
                    serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
                manifest.depgraph_digest = Some(index_digest(&graph).unwrap());
                fs::write(path, serde_json::to_vec(&manifest).unwrap()).unwrap();
            }
        }
        let before = file_identity(&snapshot);
        let destination = temp.path().join("restored");
        assert!(
            import_snapshot(&snapshot, &compatibility, &destination).is_err(),
            "{mode}"
        );
        assert!(!destination.exists(), "{mode}");
        assert_eq!(file_identity(&snapshot), before, "{mode}");
    }
}

fn file_identity(root: &Path) -> BTreeMap<String, Vec<u8>> {
    fn collect(root: &Path, dir: &Path, files: &mut BTreeMap<String, Vec<u8>>) {
        for entry in fs::read_dir(dir).unwrap() {
            let entry = entry.unwrap();
            if entry.file_type().unwrap().is_dir() {
                collect(root, &entry.path(), files);
            } else {
                files.insert(
                    entry
                        .path()
                        .strip_prefix(root)
                        .unwrap()
                        .to_string_lossy()
                        .into_owned(),
                    fs::read(entry.path()).unwrap(),
                );
            }
        }
    }
    let mut files = BTreeMap::new();
    collect(root, root, &mut files);
    files
}

#[test]
fn import_does_not_modify_the_completed_source_snapshot() {
    let (temp, store, _) = fixture();
    let compatibility = "b".repeat(64);
    let snapshot = temp.path().join("snapshot");
    export_snapshot(
        &store,
        &temp.path().join("source/artifacts"),
        &compatibility,
        &snapshot,
    )
    .unwrap();
    let before = file_identity(&snapshot);
    import_snapshot(&snapshot, &compatibility, &temp.path().join("restored")).unwrap();
    assert_eq!(file_identity(&snapshot), before);
}

#[test]
fn import_rejects_missing_staged_pointer_without_mutating_source() {
    let (temp, store, key) = fixture();
    let compatibility = "b".repeat(64);
    for legacy_payload in [false, true] {
        let snapshot = temp.path().join(format!("snapshot-{legacy_payload}"));
        export_snapshot(
            &store,
            &temp.path().join("source/artifacts"),
            &compatibility,
            &snapshot,
        )
        .unwrap();
        fs::remove_file(
            snapshot
                .join("artifacts/.staged-v2")
                .join(format!("{key}.current")),
        )
        .unwrap();
        if legacy_payload {
            fs::write(snapshot.join("artifacts").join(format!("{key}_0")), b"obj").unwrap();
        }
        let before = file_identity(&snapshot);
        let destination = temp.path().join(format!("restored-{legacy_payload}"));
        assert!(import_snapshot(&snapshot, &compatibility, &destination).is_err());
        assert!(!destination.exists());
        assert_eq!(file_identity(&snapshot), before);
    }
}

#[test]
fn import_rejects_an_undecodable_index_even_with_a_matching_transport_digest() {
    let (temp, store, _) = fixture();
    let compatibility = "b".repeat(64);
    let snapshot = temp.path().join("snapshot");
    export_snapshot(
        &store,
        &temp.path().join("source/artifacts"),
        &compatibility,
        &snapshot,
    )
    .unwrap();
    let index = snapshot.join("index.bin");
    fs::write(&index, b"invalid index").unwrap();
    let manifest_path = snapshot.join("snapshot.json");
    let mut manifest: SnapshotManifest =
        serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
    manifest.index_digest = index_digest(&index).unwrap();
    fs::write(manifest_path, serde_json::to_vec(&manifest).unwrap()).unwrap();
    let before = file_identity(&snapshot);
    let destination = temp.path().join("restored");
    assert_eq!(
        import_snapshot(&snapshot, &compatibility, &destination)
            .unwrap_err()
            .kind(),
        io::ErrorKind::InvalidData
    );
    assert!(!destination.exists());
    assert_eq!(file_identity(&snapshot), before);
}

#[test]
fn concurrent_publishers_install_only_one_complete_snapshot() {
    let (temp, store, _) = fixture();
    let compatibility = "b".repeat(64);
    let artifacts = temp.path().join("source/artifacts");
    let destination = temp.path().join("snapshot");
    let barrier = std::sync::Barrier::new(2);
    let run = || {
        barrier.wait();
        export_snapshot(&store, &artifacts, &compatibility, &destination)
    };
    let results = std::thread::scope(|scope| {
        let left = scope.spawn(run);
        let right = scope.spawn(run);
        [left.join().unwrap(), right.join().unwrap()]
    });
    assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 1);
    assert_eq!(results.iter().filter(|r| r.is_err()).count(), 1);
    assert_eq!(
        import_snapshot(&destination, &compatibility, &temp.path().join("restored")).unwrap(),
        SnapshotReceipt {
            entries: 1,
            outputs: 1
        }
    );
}

#[test]
fn export_normalizes_mixed_pack_flat_and_staged_rows_without_dropping_outputs() {
    let (temp, store, staged_key) = fixture();
    let artifacts = temp.path().join("source/artifacts");
    let flat_key = "c".repeat(64);
    let pack_key = "d".repeat(64);
    fs::write(artifacts.join(format!("{flat_key}_0")), b"flat").unwrap();
    store.insert(
        &flat_key,
        &ArtifactIndex::new(
            vec!["flat.o".into()],
            vec![4],
            vec![],
            b"flat warning".to_vec(),
            0,
        ),
    );
    layout_fixtures::seed_pack(&artifacts, &pack_key, &[b"object", b"deps"]);
    store.insert(
        &pack_key,
        &ArtifactIndex::new(
            vec!["packed.o".into(), "packed.d".into()],
            vec![6, 4],
            vec![],
            b"pack warning".to_vec(),
            0,
        ),
    );
    let snapshot = temp.path().join("snapshot");
    let compatibility = "b".repeat(64);
    assert_eq!(
        export_snapshot(&store, &artifacts, &compatibility, &snapshot).unwrap(),
        SnapshotReceipt {
            entries: 3,
            outputs: 4
        }
    );
    let restored = temp.path().join("restored");
    import_snapshot(&snapshot, &compatibility, &restored).unwrap();
    let index = ArtifactStore::open(&restored.join("index.bin")).unwrap();
    assert_eq!(index.len(), 3);
    for (key, expected) in [
        (&staged_key, vec![b"obj".as_slice()]),
        (&flat_key, vec![b"flat".as_slice()]),
        (&pack_key, vec![b"object".as_slice(), b"deps".as_slice()]),
    ] {
        let row = index.get(key).unwrap();
        let files =
            resolve_staged_artifact_files(&restored.join("artifacts"), key, &row.output_sizes)
                .unwrap()
                .unwrap();
        assert_eq!(
            files
                .iter()
                .map(|p| fs::read(p).unwrap())
                .collect::<Vec<_>>(),
            expected
        );
    }
    assert_eq!(&*index.get(&flat_key).unwrap().stderr, b"flat warning");
    assert_eq!(&*index.get(&pack_key).unwrap().stderr, b"pack warning");
}

#[test]
fn corrupt_export_leaves_prior_snapshot_intact_and_no_new_snapshot() {
    let (temp, store, key) = fixture();
    let artifacts = temp.path().join("source/artifacts");
    let prior = temp.path().join("prior");
    let compatibility = "b".repeat(64);
    export_snapshot(&store, &artifacts, &compatibility, &prior).unwrap();
    let prior_index = fs::read(prior.join("index.bin")).unwrap();
    let files = resolve_staged_artifact_files(&artifacts, &key, &[3])
        .unwrap()
        .unwrap();
    fs::write(&files[0], b"bad").unwrap();
    let next = temp.path().join("next");
    assert_eq!(
        export_snapshot(&store, &artifacts, &compatibility, &next)
            .unwrap_err()
            .kind(),
        io::ErrorKind::InvalidData
    );
    assert!(!next.exists());
    assert_eq!(fs::read(prior.join("index.bin")).unwrap(), prior_index);
    assert!(import_snapshot(&prior, &compatibility, &temp.path().join("restored")).is_ok());
    assert_eq!(
        export_snapshot(&store, &artifacts, &compatibility, &prior)
            .unwrap_err()
            .kind(),
        io::ErrorKind::AlreadyExists
    );
}

#[test]
fn import_rejects_wrong_compatibility_and_modified_index_before_publication() {
    let (temp, store, _) = fixture();
    let compatibility = "b".repeat(64);
    let snapshot = temp.path().join("snapshot");
    export_snapshot(
        &store,
        &temp.path().join("source/artifacts"),
        &compatibility,
        &snapshot,
    )
    .unwrap();
    let destination = temp.path().join("restored");
    assert_eq!(
        import_snapshot(&snapshot, &"c".repeat(64), &destination)
            .unwrap_err()
            .kind(),
        io::ErrorKind::InvalidData
    );
    assert!(!destination.exists());
    fs::write(snapshot.join("index.bin"), b"corrupt").unwrap();
    assert_eq!(
        import_snapshot(&snapshot, &compatibility, &destination)
            .unwrap_err()
            .kind(),
        io::ErrorKind::InvalidData
    );
    assert!(!destination.exists());
}

#[test]
fn empty_snapshot_round_trips_without_inventing_cache_entries() {
    let temp = tempfile::tempdir().unwrap();
    let store = ArtifactStore::open_empty(&temp.path().join("index.bin"));
    let compatibility = "b".repeat(64);
    let snapshot = temp.path().join("snapshot");
    let receipt = export_snapshot(
        &store,
        &temp.path().join("artifacts"),
        &compatibility,
        &snapshot,
    )
    .unwrap();
    assert_eq!(
        receipt,
        SnapshotReceipt {
            entries: 0,
            outputs: 0
        }
    );
    assert_eq!(
        import_snapshot(&snapshot, &compatibility, &temp.path().join("restored")).unwrap(),
        receipt
    );
}

#[test]
fn import_rejects_corrupt_payload_without_publishing() {
    let (temp, store, key) = fixture();
    let compatibility = "b".repeat(64);
    let snapshot = temp.path().join("snapshot");
    export_snapshot(
        &store,
        &temp.path().join("source/artifacts"),
        &compatibility,
        &snapshot,
    )
    .unwrap();
    let files = resolve_staged_artifact_files(&snapshot.join("artifacts"), &key, &[3])
        .unwrap()
        .unwrap();
    fs::write(&files[0], b"bad").unwrap();
    let destination = temp.path().join("restored");
    assert_eq!(
        import_snapshot(&snapshot, &compatibility, &destination)
            .unwrap_err()
            .kind(),
        io::ErrorKind::InvalidData
    );
    assert!(!destination.exists());
}

#[test]
fn export_refuses_to_drop_a_row_with_missing_payloads() {
    let (temp, store, _) = fixture();
    store.insert(
        &"c".repeat(64),
        &ArtifactIndex::new(vec!["absent.o".into()], vec![3], vec![], vec![], 0),
    );
    let snapshot = temp.path().join("snapshot");
    assert!(export_snapshot(
        &store,
        &temp.path().join("source/artifacts"),
        &"b".repeat(64),
        &snapshot
    )
    .is_err());
    assert!(!snapshot.exists());
}

#[test]
fn export_retains_readonly_payload_and_its_stored_mtime() {
    let (temp, store, key) = fixture();
    let artifacts = temp.path().join("source/artifacts");
    let files = resolve_staged_artifact_files(&artifacts, &key, &[3])
        .unwrap()
        .unwrap();
    zccache_core::mtime::stamp_mtime(
        &files[0],
        zccache_core::mtime::FileTime::from_unix_time(1_700_000_000, 0),
    )
    .unwrap();
    let mut permissions = fs::metadata(&files[0]).unwrap().permissions();
    permissions.set_readonly(true);
    fs::set_permissions(&files[0], permissions).unwrap();
    let expected_mtime = fs::metadata(&files[0]).unwrap().modified().unwrap();
    let snapshot = temp.path().join("snapshot");
    export_snapshot(&store, &artifacts, &"b".repeat(64), &snapshot).unwrap();
    let copied = resolve_staged_artifact_files(&snapshot.join("artifacts"), &key, &[3])
        .unwrap()
        .unwrap();
    let metadata = fs::metadata(&copied[0]).unwrap();
    assert_eq!(metadata.modified().unwrap(), expected_mtime);
    assert!(metadata.permissions().readonly());
}

#[test]
fn snapshot_retains_multi_output_names_modes_and_compiler_verdicts() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source");
    let artifacts = source.join("artifacts");
    let key = "a".repeat(64);
    layout_fixtures::seed_staged_generation(&artifacts, &key, &[b"obj", b"depfile"]);
    let store = ArtifactStore::open_empty(&source.join("index.bin"));
    let meta = ArtifactIndex {
        output_names: Arc::from(["unit.rlib".into(), "unit.d".into()]),
        output_sizes: vec![3, 7],
        stdout: Arc::new(b"stdout".to_vec()),
        stderr: Arc::new(b"warning".to_vec()),
        exit_code: 0,
        total_size: 10,
        stored_at_secs: 100,
        rustc_verdicts: BTreeMap::from([(
            "context".into(),
            ArtifactVerdict {
                stdout: Arc::new(b"verdict stdout".to_vec()),
                stderr: Arc::new(b"verdict stderr".to_vec()),
                exit_code: 0,
            },
        )]),
        output_modes: vec![0o644, 0o600],
    };
    store.insert(&key, &meta);
    let destination = temp.path().join("snapshot");
    let receipt = export_snapshot(&store, &artifacts, &"b".repeat(64), &destination).unwrap();
    assert_eq!(
        receipt,
        SnapshotReceipt {
            entries: 1,
            outputs: 2
        }
    );
    let restored = ArtifactStore::open(&destination.join("index.bin")).unwrap();
    let row = restored.get(&key).unwrap();
    assert_eq!(&*row.output_names, &["unit.rlib", "unit.d"]);
    assert_eq!(row.output_modes, vec![0o644, 0o600]);
    assert_eq!(&*row.stderr, b"warning");
    assert_eq!(&*row.rustc_verdicts["context"].stdout, b"verdict stdout");
    assert_eq!(&*row.rustc_verdicts["context"].stderr, b"verdict stderr");
    let payloads = resolve_staged_artifact_files(&destination.join("artifacts"), &key, &[3, 7])
        .unwrap()
        .unwrap();
    assert_eq!(fs::read(&payloads[0]).unwrap(), b"obj");
    assert_eq!(fs::read(&payloads[1]).unwrap(), b"depfile");
}
