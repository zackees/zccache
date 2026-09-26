//! Daemon-side depgraph save-failure behaviour (zccache#1660 / #1661).
//!
//! The acceptance list in #1661's "What a save failure does today" comment:
//! a failed snapshot save must be an `Err` the daemon logs and survives —
//! never a panic that ends the save loop, never a torn or deleted prior
//! snapshot, never an aborted shutdown flush.
//!
//! Failures are induced through the depgraph crate's own seam,
//! `inject_save_failures_for_tests`, so the real `save_to_file` runs up to the
//! point of failure (tmp file created) and then returns `Err`. The seam is a
//! process-global (path-scoped) counter, so every test here holds [`INJECTION_LOCK`] to keep
//! one test's injected failures from being consumed by another's save.

use super::super::*;

use std::sync::atomic::Ordering;

const TICK: Duration = Duration::from_millis(20);
const DEADLINE: Duration = Duration::from_secs(10);
const POLL: Duration = Duration::from_millis(10);

static INJECTION_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Sync tests take the lock with `blocking_lock` (they are not on a runtime);
/// async tests `.await` it so the guard may be held across awaits.
fn injection_lock() -> tokio::sync::MutexGuard<'static, ()> {
    INJECTION_LOCK.blocking_lock()
}

fn fast_intervals() -> MaintenanceIntervals {
    MaintenanceIntervals {
        memory_eviction: TICK,
        depgraph_save: TICK,
    }
}

fn test_state(cache_dir: &crate::core::NormalizedPath) -> Arc<SharedState> {
    let endpoint = crate::ipc::unique_test_endpoint();
    let identity = crate::ipc::current_backend_identity(&endpoint).expect("backend identity");
    let (state, _index_writer_rx) =
        new_shared_state(&endpoint, cache_dir, None, identity, None, None).expect("shared state");
    state
}

fn stop(state: &Arc<SharedState>) {
    state.shutdown_requested.store(true, Ordering::Release);
    state.shutdown.notify_waiters();
}

async fn wait_until(mut ready: impl FnMut() -> bool) -> bool {
    tokio::time::timeout(DEADLINE, async {
        while !ready() {
            tokio::time::sleep(POLL).await;
        }
    })
    .await
    .is_ok()
}

fn context(root: &std::path::Path, name: &str) -> CompileContext {
    let source = root.join(name);
    std::fs::write(&source, "int main(void) { return 0; }\n").unwrap();
    CompileContext {
        source_file: source.into(),
        include_search: crate::depgraph::IncludeSearchPaths::default(),
        defines: Vec::new(),
        flags: Vec::new(),
        force_includes: Vec::new(),
        unknown_flags: Vec::new(),
        compiler_hash: crate::hash::hash_bytes(b"fixture"),
    }
}

/// Context count of the snapshot on disk, or `None` if it cannot be loaded.
fn persisted_contexts(path: &std::path::Path) -> Option<usize> {
    crate::depgraph::load_from_file(path)
        .ok()
        .map(|graph| graph.stats().context_count)
}

fn tmp_files(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .map(|entry| entry.path())
                .filter(|path| path.to_string_lossy().ends_with(".tmp"))
                .collect()
        })
        .unwrap_or_default()
}

/// (A) Injected failures are logged `Err`s: the save loop keeps running, the
/// supervisor never has to bring it back, and a later tick retries and
/// succeeds.
#[tokio::test]
async fn a_failed_periodic_save_keeps_the_loop_running_and_retries() {
    let _guard = INJECTION_LOCK.lock().await;
    let root = tempfile::tempdir().expect("cache root");
    let cache_dir = crate::core::NormalizedPath::new(root.path());
    let state = test_state(&cache_dir);
    let depgraph_path = depgraph_file_path_for_cache_dir(&cache_dir);
    std::fs::remove_file(&depgraph_path).ok();
    state.dep_graph_persisted.store(false, Ordering::Release);

    crate::depgraph::inject_save_failures_for_tests(root.path(), 3);
    let started = MaintenanceSchedule::new(
        Arc::clone(&state),
        MaintenancePolicy::default(),
        ServiceMode::Embedded,
    )
    .with_intervals(fast_intervals())
    .start();
    assert!(started.started.contains(&TASK_DEPGRAPH_SAVE));

    // Restart backoff is 1 s; with a 20 ms tick the three failures and the
    // retry land well inside it, so a save here is the *same* loop retrying
    // rather than a supervisor-restarted replacement.
    let saved = wait_until(|| depgraph_path.exists()).await;
    assert!(
        saved,
        "the tick after the injected failures must retry and persist"
    );
    assert!(state.dep_graph_persisted.load(Ordering::Acquire));

    // Still ticking: remove the snapshot and the same loop writes it again.
    std::fs::remove_file(&depgraph_path).unwrap();
    let saved_again = wait_until(|| depgraph_path.exists()).await;
    stop(&state);
    crate::depgraph::inject_save_failures_for_tests(root.path(), 0);
    assert!(
        saved_again,
        "the save loop must still be alive after failing"
    );
}

/// (B) After N failures, a later successful save persists state registered
/// after those failures — the loop does not get stuck on a stale graph.
#[tokio::test]
async fn a_save_after_failures_persists_later_registrations() {
    let _guard = INJECTION_LOCK.lock().await;
    let root = tempfile::tempdir().expect("cache root");
    let cache_dir = crate::core::NormalizedPath::new(root.path());
    let state = test_state(&cache_dir);
    let depgraph_path = depgraph_file_path_for_cache_dir(&cache_dir);
    std::fs::remove_file(&depgraph_path).ok();

    state
        .dep_graph
        .load()
        .register(context(root.path(), "first.c"));

    crate::depgraph::inject_save_failures_for_tests(root.path(), 4);
    // Held: dropping the started schedule cancels the save loop it owns.
    let _started = MaintenanceSchedule::new(
        Arc::clone(&state),
        MaintenancePolicy::default(),
        ServiceMode::Embedded,
    )
    .with_intervals(fast_intervals())
    .start();

    let first = wait_until(|| persisted_contexts(&depgraph_path) == Some(1)).await;
    assert!(
        first,
        "the first successful save after the failures must land"
    );

    state
        .dep_graph
        .load()
        .register(context(root.path(), "second.c"));
    let second = wait_until(|| persisted_contexts(&depgraph_path) == Some(2)).await;
    stop(&state);
    crate::depgraph::inject_save_failures_for_tests(root.path(), 0);
    assert!(
        second,
        "a context registered after the failed saves must reach the snapshot"
    );
}

/// (C) A failed save leaves the prior snapshot byte-identical and no tmp file
/// behind.
#[test]
fn a_failed_save_leaves_the_prior_snapshot_intact() {
    let _guard = injection_lock();
    let root = tempfile::tempdir().expect("cache root");
    let dir = root.path().join("depgraph");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("depgraph.bin");

    let graph = crate::depgraph::DepGraph::new();
    graph.register(context(root.path(), "a.c"));
    crate::depgraph::save_to_file(&graph, &path).expect("baseline save");
    let before = std::fs::read(&path).unwrap();

    graph.register(context(root.path(), "b.c"));
    crate::depgraph::inject_save_failures_for_tests(root.path(), 1);
    let result = crate::depgraph::save_to_file(&graph, &path);
    crate::depgraph::inject_save_failures_for_tests(root.path(), 0);

    assert!(result.is_err(), "the injected failure must surface as Err");
    assert_eq!(
        std::fs::read(&path).unwrap(),
        before,
        "a failed save must not touch the previous snapshot"
    );
    assert!(
        tmp_files(&dir).is_empty(),
        "a failed save must clean up its tmp file: {:?}",
        tmp_files(&dir)
    );
}

/// (D) A daemon starting on a stale-but-valid snapshot (older than a restart,
/// younger than the TTL) loads it warm and saves it again without error.
#[test]
fn a_stale_but_valid_snapshot_loads_and_saves() {
    let _guard = injection_lock();
    let root = tempfile::tempdir().expect("cache root");
    let path = root.path().join("depgraph.bin");

    let graph = crate::depgraph::DepGraph::new();
    graph.register(context(root.path(), "stale.c"));
    let two_days_ms = 2 * 86_400 * 1_000;
    let opts = crate::depgraph::SaveOptions {
        now_unix_ms: crate::depgraph::snapshot::now_unix_ms().saturating_sub(two_days_ms),
        ..crate::depgraph::SaveOptions::default()
    };
    crate::depgraph::save_to_file_with(&graph, &path, &opts).expect("stale save");

    let load = crate::daemon::depgraph_load::load_for_startup(&path);
    let loaded = load
        .graph
        .expect("a snapshot inside the TTL must load warm");
    assert!(load.warning.is_none(), "{:?}", load.warning);
    assert_eq!(loaded.stats().context_count, 1);

    crate::depgraph::save_to_file(&loaded, &path).expect("re-saving a loaded graph");
    assert_eq!(persisted_contexts(&path), Some(1));
}

/// (G) An injected depgraph failure during the embedded shutdown flush is a
/// reported step failure, not an aborted shutdown: the flush completes and
/// the artifact index is still written.
#[tokio::test(start_paused = true)]
async fn a_depgraph_failure_does_not_abort_embedded_shutdown() {
    let _guard = INJECTION_LOCK.lock().await;
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

    let expected = 5usize;
    for i in 0..expected {
        let key = format!("{i:064x}");
        let meta = crate::artifact::ArtifactIndex::new(
            vec!["foo.o".to_string()],
            vec![i as u64 + 1],
            Vec::new(),
            Vec::new(),
            0,
        );
        assert!(state
            .index_writer_tx
            .send(IndexWriterCommand::Insert(key, meta))
            .is_ok());
    }

    crate::depgraph::inject_save_failures_for_tests(tmp.path(), 1);
    let report = daemon.shutdown().await;
    crate::depgraph::inject_save_failures_for_tests(tmp.path(), 0);

    let outcome = |name: &str| {
        report
            .steps
            .iter()
            .find(|step| step.step == name)
            .map(|step| step.outcome.clone())
    };
    assert!(
        matches!(outcome("depgraph"), Some(FlushStepOutcome::Failed(_))),
        "the injected save failure must be reported as a failed step: {report:?}"
    );
    assert!(
        matches!(outcome("artifact_store"), Some(FlushStepOutcome::Completed)),
        "the artifact index step must still complete: {report:?}"
    );
    if let Some(metadata) = outcome("metadata") {
        assert!(
            matches!(metadata, FlushStepOutcome::Completed),
            "steps after the depgraph must still run: {report:?}"
        );
    }

    let index_path = crate::core::config::index_path_from_cache_dir(&cache_dir);
    let reopened = crate::artifact::ArtifactStore::open(index_path.as_path()).unwrap();
    assert_eq!(
        reopened.len(),
        expected,
        "the artifact index must be written"
    );
}
