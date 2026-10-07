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
}

/// Completed backend publication, separate from test-result evidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SnapshotReceipt {
    pub entries: usize,
    pub outputs: usize,
}

/// Export a complete staged store to a new immutable snapshot directory.
/// The caller must flush or supply the live authoritative index before export.
/// A failed export must leave every previously committed snapshot intact.
pub fn export_snapshot(
    store: &ArtifactStore,
    artifact_dir: &Path,
    compatibility: &str,
    destination: &Path,
) -> io::Result<SnapshotReceipt> {
    validate_compatibility(compatibility)?;
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
    let _guard = StagedReadGuard::acquire_if_present(artifact_dir)?;
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
        if !crate::layout::copy_staged_generation(artifact_dir, key, &meta.output_sizes, &target)? {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "snapshot export requires staged payloads; legacy layouts are not yet supported",
            ));
        }
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
    };
    let manifest_path = pending.path().join("snapshot.json");
    let mut manifest_file = fs::File::create(&manifest_path)?;
    std::io::Write::write_all(
        &mut manifest_file,
        &serde_json::to_vec(&manifest).map_err(invalid)?,
    )?;
    manifest_file.sync_all()?;
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
    if index_digest(&index)? != manifest.index_digest {
        return Err(invalid("snapshot index digest mismatch"));
    }
    let store = ArtifactStore::open(&index)?;
    export_snapshot(
        &store,
        &source.join("artifacts"),
        compatibility,
        destination,
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
