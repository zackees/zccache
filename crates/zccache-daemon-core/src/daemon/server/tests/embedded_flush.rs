//! Regression tests for embedded-service flush durability.

use super::super::*;

async fn schedule_blocked_exec_publisher(
    state: &Arc<SharedState>,
    key: &str,
) -> tokio::sync::OwnedSemaphorePermit {
    let permit_count = state.persist_semaphore.available_permits() as u32;
    let blocked = Arc::clone(&state.persist_semaphore)
        .acquire_many_owned(permit_count)
        .await
        .unwrap();
    let artifact = ArtifactData {
        outputs: vec![ArtifactOutput {
            name: "result.bin".to_string(),
            payload: ArtifactPayload::Bytes(Arc::new(b"durable before lifecycle return".to_vec())),
        }],
        stdout: Arc::new(Vec::new()),
        stderr: Arc::new(Vec::new()),
        exit_code: 0,
    };
    super::super::handle_exec::store_exec_artifact(state, key.to_string(), artifact, None)
        .await
        .unwrap();
    blocked
}

#[tokio::test(start_paused = true)]
async fn embedded_flush_persists_queued_index_rows_before_returning() {
    let tmp = tempfile::tempdir().unwrap();
    let cache_dir = crate::core::NormalizedPath::new(tmp.path());
    let endpoint = crate::ipc::unique_test_endpoint();
    let daemon = EmbeddedDaemon::start(
        endpoint,
        cache_dir.clone(),
        None,
        MaintenancePolicy::default(),
    )
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
    assert!(report.index_writer_drained);
    assert!(
        report.is_complete(),
        "every durable flush step must complete in the healthy path: {report:?}"
    );
    assert!(report
        .steps
        .iter()
        .all(|step| matches!(step.outcome, FlushStepOutcome::Completed)));
    assert_eq!(report.artifact_entries, expected as u64);
    assert_eq!(state.artifact_store.len(), expected);

    let index_path = crate::core::config::index_path_from_cache_dir(&cache_dir);
    let reopened = crate::artifact::ArtifactStore::open(index_path.as_path()).unwrap();
    assert_eq!(reopened.len(), state.artifact_store.len());
    assert_eq!(reopened.len(), expected);

    let _ = daemon.shutdown().await;
}

#[tokio::test]
async fn embedded_flush_waits_for_detached_publisher_handoff() {
    let tmp = tempfile::tempdir().unwrap();
    let cache_dir = crate::core::NormalizedPath::new(tmp.path());
    let daemon = Arc::new(
        EmbeddedDaemon::start(
            crate::ipc::unique_test_endpoint(),
            cache_dir.clone(),
            None,
            MaintenancePolicy::default(),
        )
        .await
        .unwrap(),
    );
    let key = "7".repeat(64);
    let blocked = schedule_blocked_exec_publisher(&daemon.state, &key).await;

    let flush_daemon = Arc::clone(&daemon);
    let mut flush = tokio::spawn(async move { flush_daemon.flush().await });
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(25), &mut flush)
            .await
            .is_err(),
        "flush must wait for a detached publisher scheduled before it"
    );
    drop(blocked);
    let report = tokio::time::timeout(std::time::Duration::from_secs(5), flush)
        .await
        .unwrap()
        .unwrap();
    assert!(report.pending_writes_drained);
    assert!(daemon.state.artifact_store.get(&key).is_some());
    let reopened = crate::artifact::ArtifactStore::open(
        crate::core::config::index_path_from_cache_dir(&cache_dir).as_path(),
    )
    .unwrap();
    assert!(reopened.get(&key).is_some());
    let _ = daemon.shutdown().await;
}

#[tokio::test]
async fn embedded_shutdown_waits_for_detached_publisher_before_stopping_writer() {
    let tmp = tempfile::tempdir().unwrap();
    let cache_dir = crate::core::NormalizedPath::new(tmp.path());
    let daemon = Arc::new(
        EmbeddedDaemon::start(
            crate::ipc::unique_test_endpoint(),
            cache_dir.clone(),
            None,
            MaintenancePolicy::default(),
        )
        .await
        .unwrap(),
    );
    let key = "8".repeat(64);
    let blocked = schedule_blocked_exec_publisher(&daemon.state, &key).await;

    let shutdown_daemon = Arc::clone(&daemon);
    let mut shutdown = tokio::spawn(async move { shutdown_daemon.shutdown().await });
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(25), &mut shutdown)
            .await
            .is_err(),
        "shutdown must wait before stopping the index writer"
    );
    drop(blocked);
    let report = tokio::time::timeout(std::time::Duration::from_secs(5), shutdown)
        .await
        .unwrap()
        .unwrap();
    assert!(report.pending_writes_drained);
    let reopened = crate::artifact::ArtifactStore::open(
        crate::core::config::index_path_from_cache_dir(&cache_dir).as_path(),
    )
    .unwrap();
    assert!(reopened.get(&key).is_some());
}

#[tokio::test]
async fn publisher_arriving_after_embedded_shutdown_cannot_reopen_cache() {
    let tmp = tempfile::tempdir().unwrap();
    let cache_dir = crate::core::NormalizedPath::new(tmp.path());
    let daemon = Arc::new(
        EmbeddedDaemon::start(
            crate::ipc::unique_test_endpoint(),
            cache_dir,
            None,
            MaintenancePolicy::default(),
        )
        .await
        .unwrap(),
    );
    let key = "6".repeat(64);
    let report = daemon.shutdown().await;
    assert!(report.pending_writes_drained);

    let artifact = ArtifactData {
        outputs: vec![ArtifactOutput {
            name: "late.bin".to_string(),
            payload: ArtifactPayload::Bytes(Arc::new(b"must not publish".to_vec())),
        }],
        stdout: Arc::new(Vec::new()),
        stderr: Arc::new(Vec::new()),
        exit_code: 0,
    };
    super::super::handle_exec::store_exec_artifact(&daemon.state, key.clone(), artifact, None)
        .await
        .unwrap();

    assert!(!daemon.state.artifacts.contains_key(&key));
    assert!(daemon.state.artifact_store.get(&key).is_none());
    assert!(!daemon.state.artifact_dir.join(format!("{key}_0")).exists());
}

#[tokio::test]
async fn publisher_queued_behind_shutdown_writer_rechecks_latched_flag() {
    let tmp = tempfile::tempdir().unwrap();
    let cache_dir = crate::core::NormalizedPath::new(tmp.path());
    let daemon = Arc::new(
        EmbeddedDaemon::start(
            crate::ipc::unique_test_endpoint(),
            cache_dir,
            None,
            MaintenancePolicy::default(),
        )
        .await
        .unwrap(),
    );
    let shutdown_writer = Arc::clone(&daemon.state.artifact_publication)
        .write_owned()
        .await;
    let publisher_state = Arc::clone(&daemon.state);
    let mut publisher =
        tokio::spawn(async move { begin_artifact_publication(&publisher_state).await });
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(25), &mut publisher)
            .await
            .is_err(),
        "publisher must queue behind the shutdown writer"
    );

    daemon
        .state
        .shutdown_requested
        .store(true, Ordering::Release);
    drop(shutdown_writer);
    let guard = tokio::time::timeout(std::time::Duration::from_secs(5), publisher)
        .await
        .unwrap()
        .unwrap();
    assert!(guard.is_none(), "queued publisher must re-check shutdown");
    let _ = daemon.shutdown().await;
}

/// #1162 finding 3: persistence used to live only in the explicit
/// `flush()`/`shutdown()` paths, so a host error, early return, or
/// panic-unwind that dropped the handle discarded the index outright — the
/// same end state as SIGKILL, on the primary embedded compile path.
///
/// Dropping without `shutdown()` must still leave a checkpoint on disk.
#[tokio::test]
async fn dropping_without_shutdown_still_checkpoints_the_index() {
    let tmp = tempfile::tempdir().unwrap();
    let cache_dir = crate::core::NormalizedPath::new(tmp.path());
    let index_path = crate::core::config::index_path_from_cache_dir(&cache_dir);
    let expected = 12usize;

    {
        let daemon = EmbeddedDaemon::start(
            crate::ipc::unique_test_endpoint(),
            cache_dir.clone(),
            None,
            MaintenancePolicy::default(),
        )
        .await
        .unwrap();

        for i in 0..expected {
            daemon
                .state
                .artifact_store
                .insert(&format!("{i:064x}"), &synthetic_index_entry(i as u64 + 1));
        }
        assert_eq!(daemon.state.artifact_store.len(), expected);

        // The whole point: no `shutdown()`, no `flush()` — just drop it, the
        // way a `?` on an error path in the host would.
    }

    let reopened = crate::artifact::ArtifactStore::open(index_path.as_path())
        .expect("drop must leave a readable index behind");
    assert_eq!(
        reopened.len(),
        expected,
        "dropping the service without shutdown() must still checkpoint the index"
    );
}

/// The drop-time checkpoint must never publish a snapshot it has not finished
/// loading.
///
/// `ArtifactStore::flush` serializes the *entire* in-memory map over
/// `index.bin`. If the on-disk entries have not been merged in yet, flushing
/// replaces a populated index with an empty one — total cache loss, strictly
/// worse than the unshut drop the checkpoint exists to survive.
///
/// `start()` currently awaits every load, so the production path cannot reach
/// this state and no other test can either; that is exactly why the guard
/// needs its own test. The invariant it protects is non-local — the standalone
/// path already opens the index empty and merges it in the background (#784
/// phase 2d), so the day that reaches the embedded service, an ungated flush
/// here would silently destroy the accumulated index.
#[tokio::test]
async fn an_unloaded_checkpoint_leaves_on_disk_state_untouched() {
    let tmp = tempfile::tempdir().unwrap();
    let cache_dir = crate::core::NormalizedPath::new(tmp.path());
    let index_path = crate::core::config::index_path_from_cache_dir(&cache_dir);
    let survivors = 7usize;

    // A populated index from a previous, healthy run.
    {
        let daemon = EmbeddedDaemon::start(
            crate::ipc::unique_test_endpoint(),
            cache_dir.clone(),
            None,
            MaintenancePolicy::default(),
        )
        .await
        .unwrap();
        for i in 0..survivors {
            daemon
                .state
                .artifact_store
                .insert(&format!("{i:064x}"), &synthetic_index_entry(i as u64 + 1));
        }
        let _ = daemon.shutdown().await;
    }
    assert_eq!(
        crate::artifact::ArtifactStore::open(index_path.as_path())
            .expect("seeded index")
            .len(),
        survivors
    );

    // A service whose loads have not landed: its in-memory store is empty and
    // must not be published over the good on-disk one.
    {
        let daemon = EmbeddedDaemon::start(
            crate::ipc::unique_test_endpoint(),
            cache_dir.clone(),
            None,
            MaintenancePolicy::default(),
        )
        .await
        .unwrap();
        daemon
            .state
            .artifact_store_loaded
            .store(false, Ordering::Release);
        daemon
            .state
            .dep_graph_load_complete
            .store(false, Ordering::Release);
        daemon
            .state
            .metadata_cache_loaded
            .store(false, Ordering::Release);
        daemon.state.artifact_store.clear();
        assert_eq!(daemon.state.artifact_store.len(), 0);
        // Dropped unshut, so the checkpoint runs — and must decline every write.
    }

    assert_eq!(
        crate::artifact::ArtifactStore::open(index_path.as_path())
            .expect("index must survive an unloaded checkpoint")
            .len(),
        survivors,
        "an unloaded checkpoint must not publish an empty index over a populated one"
    );
}

/// The graceful path must not pay for the backstop: `shutdown()` latches
/// `shutdown_requested` before its final flush, so the drop-time checkpoint
/// skips rather than writing everything a second time.
#[tokio::test]
async fn shutdown_suppresses_the_drop_time_checkpoint() {
    let tmp = tempfile::tempdir().unwrap();
    let cache_dir = crate::core::NormalizedPath::new(tmp.path());
    let daemon = EmbeddedDaemon::start(
        crate::ipc::unique_test_endpoint(),
        cache_dir.clone(),
        None,
        MaintenancePolicy::default(),
    )
    .await
    .unwrap();
    daemon
        .state
        .artifact_store
        .insert(&format!("{:064x}", 1), &synthetic_index_entry(1));

    let _ = daemon.shutdown().await;
    let state = Arc::clone(&daemon.state);
    drop(daemon);

    assert!(
        state.shutdown_requested.load(Ordering::Acquire),
        "shutdown must latch the flag the drop-time checkpoint gates on"
    );
    let index_path = crate::core::config::index_path_from_cache_dir(&cache_dir);
    let reopened = crate::artifact::ArtifactStore::open(index_path.as_path()).unwrap();
    assert_eq!(reopened.len(), 1, "the graceful flush must still be intact");
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
