//! Thin CLI adapter for the artifact backend's immutable snapshot API.

use super::args::CacheSnapshotArgs;
use crate::artifact::snapshot::{export_store_snapshot, import_snapshot, SnapshotReceipt};
use serde::Serialize;
use std::{io, process::ExitCode};

#[derive(Serialize)]
struct SnapshotOutput {
    schema: u32,
    #[serde(flatten)]
    receipt: SnapshotReceipt,
}

pub(crate) fn export(args: CacheSnapshotArgs) -> ExitCode {
    emit(export_store_snapshot(
        &args.root,
        &args.compatibility,
        &args.snapshot,
    ))
}

pub(crate) fn import(args: CacheSnapshotArgs) -> ExitCode {
    emit(import_snapshot(
        &args.snapshot,
        &args.compatibility,
        &args.root,
    ))
}

fn emit(result: io::Result<SnapshotReceipt>) -> ExitCode {
    match result.and_then(|receipt| {
        serde_json::to_string(&SnapshotOutput { schema: 1, receipt }).map_err(io::Error::other)
    }) {
        Ok(json) => {
            println!("{json}");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("zccache cache: {error}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::artifact::{resolve_staged_artifact_files, ArtifactIndex, ArtifactStore};
    use std::{fs, path::Path};

    fn args(operation: &str, root: &Path, snapshot: &Path, compatibility: &str) -> Vec<String> {
        vec![
            "zccache".into(),
            "cache".into(),
            operation.into(),
            "--root".into(),
            root.to_string_lossy().into_owned(),
            "--snapshot".into(),
            snapshot.to_string_lossy().into_owned(),
            "--compatibility".into(),
            compatibility.into(),
        ]
    }

    #[test]
    fn public_cache_commands_round_trip_and_reject_wrong_compatibility() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("source");
        fs::create_dir_all(root.join("artifacts")).unwrap();
        let key = "a".repeat(64);
        fs::write(root.join("artifacts").join(format!("{key}_0")), b"object").unwrap();
        let store = ArtifactStore::open_empty(&root.join("index.bin"));
        store.insert(
            &key,
            &ArtifactIndex::new(
                vec!["unit.o".into()],
                vec![6],
                vec![],
                b"diagnostic".to_vec(),
                0,
            ),
        );
        store.flush().unwrap();
        let compatibility = "b".repeat(64);
        let snapshot = temp.path().join("snapshot");
        assert_eq!(
            super::super::run_with_args(&args("export", &root, &snapshot, &compatibility)),
            ExitCode::SUCCESS
        );
        let restored = temp.path().join("restored");
        assert_eq!(
            super::super::run_with_args(&args("import", &restored, &snapshot, &compatibility)),
            ExitCode::SUCCESS
        );
        let index = ArtifactStore::open(&restored.join("index.bin")).unwrap();
        let row = index.get(&key).unwrap();
        assert_eq!(&*row.stderr, b"diagnostic");
        assert_eq!(&*row.output_names, &["unit.o"]);
        let files = resolve_staged_artifact_files(&restored.join("artifacts"), &key, &[6])
            .unwrap()
            .unwrap();
        assert_eq!(fs::read(&files[0]).unwrap(), b"object");
        let incompatible = temp.path().join("incompatible");
        assert_eq!(
            super::super::run_with_args(&args("import", &incompatible, &snapshot, &"c".repeat(64))),
            ExitCode::FAILURE
        );
        assert!(!incompatible.exists());
    }
}
