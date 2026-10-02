//! Output-mode persistence and migration from pre-mode index snapshots.

use super::*;

#[test]
fn output_modes_survive_index_roundtrip() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("index.bin");
    let store = ArtifactStore::open(&path).unwrap();
    let mut meta = ArtifactIndex::new(
        vec!["app".into(), "app.debug".into()],
        vec![10, 20],
        Vec::new(),
        Vec::new(),
        0,
    );
    meta.output_modes = vec![0o751, 0o640];
    store.insert("link", &meta);
    store.flush().unwrap();
    let reopened = ArtifactStore::open(&path).unwrap();
    assert_eq!(
        reopened.get("link").unwrap().output_modes,
        vec![0o751, 0o640]
    );
}

#[test]
fn pre_mode_index_preserves_verdicts_and_multiple_rows() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("index.bin");
    let rows: Vec<_> = ["first", "second"]
        .into_iter()
        .map(|key| {
            (
                key.to_string(),
                LegacyVerdictArtifactIndex {
                    output_names: Arc::from(vec!["foo.o".to_string()]),
                    output_sizes: vec![4],
                    stdout: Arc::new(Vec::new()),
                    stderr: Arc::new(Vec::new()),
                    exit_code: 0,
                    total_size: 4,
                    stored_at_secs: 42,
                    rustc_verdicts: BTreeMap::from([(
                        "lint".to_string(),
                        ArtifactVerdict {
                            stdout: Arc::new(Vec::new()),
                            stderr: Arc::new(b"lint diagnostic".to_vec()),
                            exit_code: 1,
                        },
                    )]),
                },
            )
        })
        .collect();
    std::fs::write(&path, bincode::serialize(&rows).unwrap()).unwrap();
    let store = ArtifactStore::open(&path).unwrap();
    for key in ["first", "second"] {
        let meta = store.get(key).unwrap();
        assert!(meta.output_modes.is_empty());
        assert_eq!(meta.rustc_verdicts["lint"].exit_code, 1);
        assert_eq!(meta.total_size, 4);
    }
}
