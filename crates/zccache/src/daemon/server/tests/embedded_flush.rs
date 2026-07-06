//! Regression tests for embedded-service flush durability.

use super::super::*;

#[tokio::test(start_paused = true)]
async fn embedded_flush_persists_queued_index_rows_before_returning() {
    let tmp = tempfile::tempdir().unwrap();
    let cache_dir = crate::core::NormalizedPath::new(tmp.path());
    let endpoint = crate::ipc::unique_test_endpoint();
    let daemon = EmbeddedDaemon::start(endpoint, cache_dir.clone(), None)
        .await
        .unwrap();
    let state = Arc::clone(&daemon.state);

    let expected = 37usize;
    for i in 0..expected {
        let key = format!("{i:064x}");
        let meta = synthetic_index_entry(i as u64 + 1);
        state
            .index_writer_tx
            .send(IndexWriterCommand::Insert(key, meta))
            .unwrap();
    }

    let report = daemon.flush().await;

    assert!(report.pending_writes_drained);
    assert_eq!(report.artifact_entries, expected as u64);
    assert_eq!(state.artifact_store.len(), expected);

    let index_path = crate::core::config::index_path_from_cache_dir(&cache_dir);
    let reopened = crate::artifact::ArtifactStore::open(index_path.as_path()).unwrap();
    assert_eq!(reopened.len(), state.artifact_store.len());
    assert_eq!(reopened.len(), expected);

    let _ = daemon.shutdown().await;
}

fn synthetic_index_entry(total_size: u64) -> crate::artifact::ArtifactIndex {
    crate::artifact::ArtifactIndex::new(
        vec!["foo.o".to_string()],
        vec![total_size],
        Vec::new(),
        Vec::new(),
        0,
    )
}
