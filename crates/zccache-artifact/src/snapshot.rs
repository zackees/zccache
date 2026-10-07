//! Immutable compiler-store snapshots for external cache transport.
//! The backend retains its index and staged layout; callers provide only a
//! compatibility identity and a new publication path, never internal paths.

use serde::{Deserialize, Serialize};
use std::{fs, io, path::Path};

use crate::{ArtifactStore, StagedReadGuard};

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SnapshotManifest {
    schema: u32,
    compatibility: String,
    index_digest: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    depgraph_digest: Option<String>,
}

/// Completed backend publication, separate from test-result evidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct SnapshotReceipt {
    pub entries: usize,
    pub outputs: usize,
}

/// Export a complete store to a new immutable snapshot directory.
/// The caller must quiesce writers and flush or supply the authoritative index.
/// A failed export must leave every previously committed snapshot intact.
pub fn export_snapshot(
    store: &ArtifactStore,
    artifact_dir: &Path,
    compatibility: &str,
    destination: &Path,
) -> io::Result<SnapshotReceipt> {
    validate_compatibility(compatibility)?;
    let _guard = StagedReadGuard::acquire_if_present(artifact_dir)?;
    publish_snapshot(
        store,
        artifact_dir,
        compatibility,
        destination,
        crate::layout::SnapshotLayout::NormalizeLegacy,
        None,
    )
}

/// Export a flushed store from disk, refusing a live daemon's writer lease.
pub fn export_store_snapshot(
    cache_root: &Path,
    compatibility: &str,
    destination: &Path,
) -> io::Result<SnapshotReceipt> {
    validate_compatibility(compatibility)?;
    let _writer = zccache_core::cache_root_lock::CacheRootWriterLock::acquire(cache_root)?;
    let index = cache_root.join("index.bin");
    let store = ArtifactStore::from_snapshot(&index, &fs::read(&index)?)?;
    let graph = match fs::read(cache_root.join("depgraph/depgraph.bin")) {
        Ok(bytes) => Some(bytes),
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => return Err(error),
    };
    let artifacts = cache_root.join("artifacts");
    let _guard = StagedReadGuard::acquire_if_present(&artifacts)?;
    publish_snapshot(
        &store,
        &artifacts,
        compatibility,
        destination,
        crate::layout::SnapshotLayout::NormalizeLegacy,
        graph.as_deref(),
    )
}

fn publish_snapshot(
    store: &ArtifactStore,
    artifact_dir: &Path,
    compatibility: &str,
    destination: &Path,
    layout: crate::layout::SnapshotLayout,
    depgraph: Option<&[u8]>,
) -> io::Result<SnapshotReceipt> {
    crate::staged_lock::validate_staged_root_path(
        crate::staged_lock::staged_root(artifact_dir).as_path(),
    )?;
    // Caller-controlled destination parent; never replace a committed snapshot.
    match fs::symlink_metadata(destination) {
        Ok(_) => {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "snapshot already exists",
            ))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => (),
        Err(error) => return Err(error),
    }
    let parent = destination
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    let pending = tempfile::Builder::new()
        .prefix(".compiler-snapshot-")
        .tempdir_in(parent)?;
    let depgraph_digest = if let Some(bytes) = depgraph {
        let directory = pending.path().join("depgraph");
        fs::create_dir(&directory)?;
        let path = directory.join("depgraph.bin");
        let mut file = fs::File::create(&path)?;
        std::io::Write::write_all(&mut file, bytes)?;
        file.sync_all()?;
        drop(file);
        // The dependency-graph owner validates its format. Transport does
        // not duplicate its decoder, version handling or context semantics.
        zccache_depgraph::load_from_file(&path).map_err(invalid)?;
        kernal_api::platform::fs::sync_directory_if_supported(&directory)?;
        Some(kernal_api::hash::blake3_bytes(bytes).to_hex().to_string())
    } else {
        None
    };
    let rows = store.load_all();
    let copied = ArtifactStore::open_empty(&pending.path().join("index.bin"));
    let target = pending.path().join("artifacts");
    fs::create_dir_all(&target)?;
    let mut outputs = 0;
    for (key, meta) in &rows {
        if meta.output_names.len() != meta.output_sizes.len()
            || (!meta.output_modes.is_empty() && meta.output_modes.len() != meta.output_sizes.len())
        {
            return Err(invalid("compiler output metadata count mismatch"));
        }
        crate::layout::copy_artifact_generation(
            artifact_dir,
            key,
            &meta.output_sizes,
            &target,
            &layout,
        )?;
        copied.insert(key, meta);
        outputs += meta.output_sizes.len();
    }
    copied.flush()?;
    let index = pending.path().join("index.bin");
    fs::OpenOptions::new()
        .write(true)
        .open(&index)?
        .sync_all()?;
    let manifest = SnapshotManifest {
        schema: 1,
        compatibility: compatibility.into(),
        index_digest: index_digest(&index)?,
        depgraph_digest,
    };
    let manifest_path = pending.path().join("snapshot.json");
    let mut manifest_file = fs::File::create(&manifest_path)?;
    std::io::Write::write_all(
        &mut manifest_file,
        &serde_json::to_vec(&manifest).map_err(invalid)?,
    )?;
    manifest_file.sync_all()?;
    // Windows refuses a directory rename while a child file is still open.
    drop(manifest_file);
    kernal_api::platform::fs::sync_directory_if_supported(&target)?;
    kernal_api::platform::fs::sync_directory_if_supported(pending.path())?;
    kernal_api::platform::fs::replacement::rename_generation(pending.path(), destination)?;
    kernal_api::platform::fs::sync_directory_if_supported(parent)?;
    Ok(SnapshotReceipt {
        entries: rows.len(),
        outputs,
    })
}

/// Validate compatibility and every staged payload before publishing a new
/// private store. Import must precede daemon startup; it never merges a live
/// mutable store. Local and hosted callers use their own transport and stores.
pub fn import_snapshot(
    source: &Path,
    compatibility: &str,
    destination: &Path,
) -> io::Result<SnapshotReceipt> {
    validate_compatibility(compatibility)?;
    let manifest: SnapshotManifest =
        serde_json::from_slice(&fs::read(source.join("snapshot.json"))?).map_err(invalid)?;
    if manifest.schema != 1 || manifest.compatibility != compatibility {
        return Err(invalid("snapshot schema or compatibility mismatch"));
    }
    let index = source.join("index.bin");
    let bytes = fs::read(&index)?;
    if kernal_api::hash::blake3_bytes(&bytes).to_hex().as_str() != manifest.index_digest.as_str() {
        return Err(invalid("snapshot index digest mismatch"));
    }
    let store = ArtifactStore::from_snapshot(&index, &bytes)?;
    let depgraph = manifest.depgraph_digest.map(|expected| {
        let bytes = fs::read(source.join("depgraph/depgraph.bin"))?;
        if kernal_api::hash::blake3_bytes(&bytes).to_hex().as_str() != expected.as_str() {
            return Err(invalid("snapshot dependency-context digest mismatch"));
        }
        Ok(bytes)
    }).transpose()?;
    // Completed transport snapshots are immutable and have no maintenance
    // writer. Do not create/open a writable store lock in the source snapshot.
    publish_snapshot(
        &store,
        &source.join("artifacts"),
        compatibility,
        destination,
        crate::layout::SnapshotLayout::StagedOnly,
        depgraph.as_deref(),
    )
}

fn validate_compatibility(identity: &str) -> io::Result<()> {
    if identity.len() != 64
        || !identity
            .bytes()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
    {
        return Err(invalid(
            "compatibility identity must be 64 lowercase hex digits",
        ));
    }
    Ok(())
}

fn index_digest(path: &Path) -> io::Result<String> {
    let mut digest = kernal_api::hash::Blake3Hasher::new();
    digest.update(&fs::read(path)?);
    Ok(digest.finalize().to_hex().to_string())
}

fn invalid(message: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.to_string())
}

#[cfg(test)]
mod tests {
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
}
