//! Mode-parity and embedded-behaviour tests for the shared maintenance
//! schedule (issue #1160).
//!
//! The parity test is the recurrence guard: it compares the task names each
//! mode *actually started* against the declaration in `MAINTENANCE_TASKS`, so
//! a future task hand-spawned into one mode alone — the failure mode that
//! produced #1148, #1160 and #1165 finding 6 — fails here rather than shipping.
//!
//! The behaviour tests inject short intervals instead of sleeping through the
//! production 30 s / 300 s periods.

use super::*;

use std::collections::BTreeSet;

const TICK: Duration = Duration::from_millis(20);
const DEADLINE: Duration = Duration::from_secs(10);
const POLL: Duration = Duration::from_millis(10);

fn fast_intervals() -> MaintenanceIntervals {
    MaintenanceIntervals {
        memory_eviction: TICK,
        depgraph_save: TICK,
    }
}

/// Build a real `SharedState` over a throwaway cache root.
///
/// The schedule is state-driven, so a test double would only assert that the
/// double was called. Building the real thing also proves the tasks can start
/// against a freshly created root, which is what both services do.
fn test_state(cache_dir: &crate::core::NormalizedPath) -> Arc<SharedState> {
    let endpoint = crate::ipc::unique_test_endpoint();
    let identity = crate::ipc::current_backend_identity(&endpoint).expect("backend identity");
    let (state, _index_writer_rx) =
        new_shared_state(&endpoint, cache_dir, None, identity, None, None).expect("shared state");
    state
}

/// Let every started loop observe shutdown so the temp root can be removed.
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

fn declared(mode: ServiceMode) -> BTreeSet<&'static str> {
    MAINTENANCE_TASKS
        .iter()
        .filter(|task| task.runs_in(mode))
        .map(|task| task.name)
        .collect()
}

/// Embedded must start every member that is not explicitly standalone-only,
/// and standalone must start everything. Both sides are compared against what
/// `start()` reported spawning, not against the table alone.
#[tokio::test]
async fn embedded_starts_every_task_that_is_not_standalone_only() {
    let standalone_root = tempfile::tempdir().expect("standalone root");
    let embedded_root = tempfile::tempdir().expect("embedded root");
    let standalone_state = test_state(&crate::core::NormalizedPath::new(standalone_root.path()));
    let embedded_state = test_state(&crate::core::NormalizedPath::new(embedded_root.path()));

    let standalone = MaintenanceSchedule::new(
        Arc::clone(&standalone_state),
        MaintenancePolicy::default(),
        ServiceMode::Standalone,
    )
    .with_intervals(fast_intervals())
    // Non-zero so the idle watchdog is actually started; with 0 the standalone
    // daemon deliberately omits it and the comparison below would be vacuous.
    .with_idle_timeout_secs(3600)
    .start();
    let embedded = MaintenanceSchedule::new(
        Arc::clone(&embedded_state),
        MaintenancePolicy::default(),
        ServiceMode::Embedded,
    )
    .with_intervals(fast_intervals())
    .start();

    let standalone_started: BTreeSet<&'static str> = standalone.started.iter().copied().collect();
    let embedded_started: BTreeSet<&'static str> = embedded.started.iter().copied().collect();

    assert_eq!(
        standalone_started,
        declared(ServiceMode::Standalone),
        "standalone must start exactly the declared schedule"
    );
    assert_eq!(
        embedded_started,
        declared(ServiceMode::Embedded),
        "embedded must start exactly the declared schedule"
    );

    let missing_in_embedded: BTreeSet<&'static str> = standalone_started
        .difference(&embedded_started)
        .copied()
        .collect();
    let standalone_only: BTreeSet<&'static str> = MAINTENANCE_TASKS
        .iter()
        .filter(|task| task.standalone_only.is_some())
        .map(|task| task.name)
        .collect();
    assert_eq!(
        missing_in_embedded, standalone_only,
        "a task may only be absent from embedded mode when it is flagged \
         standalone-only with a rationale in MAINTENANCE_TASKS"
    );

    stop(&standalone_state);
    stop(&embedded_state);
}

/// The parity test above drives the schedule directly, which proves the
/// schedule is right but not that the embedded service still calls it. This
/// closes that loop against a real `EmbeddedDaemon`: before #1160 it started no
/// periodic task at all.
#[tokio::test]
async fn a_real_embedded_service_starts_the_whole_embedded_schedule() {
    let root = tempfile::tempdir().expect("cache root");
    let daemon = EmbeddedDaemon::start(
        crate::ipc::unique_test_endpoint(),
        crate::core::NormalizedPath::new(root.path()),
        None,
        MaintenancePolicy::default(),
    )
    .await
    .expect("embedded daemon");

    let started: BTreeSet<&'static str> = daemon.maintenance_tasks.iter().copied().collect();
    assert_eq!(
        started,
        declared(ServiceMode::Embedded),
        "the embedded service must start every non-standalone-only member"
    );

    let report = daemon.shutdown().await;
    assert!(report.is_complete());
}

/// The flag is only honest if it carries an argument. An empty rationale would
/// let a future omission pass the parity test above by simply flagging itself.
#[test]
fn every_standalone_only_task_states_why() {
    for task in MAINTENANCE_TASKS {
        if let Some(rationale) = task.standalone_only {
            assert!(
                rationale.len() > 40,
                "standalone-only task {} needs a real rationale, got {rationale:?}",
                task.name
            );
        }
    }
}

/// #1160(b): embedded persisted the depgraph only inside a host-driven
/// `flush()`, so a host crash lost the whole delta rather than one interval of
/// it. A tick must now write the snapshot with no host call at all.
#[tokio::test]
async fn embedded_saves_the_depgraph_on_a_tick() {
    let root = tempfile::tempdir().expect("cache root");
    let cache_dir = crate::core::NormalizedPath::new(root.path());
    let state = test_state(&cache_dir);
    let depgraph_path = depgraph_file_path_for_cache_dir(&cache_dir);
    std::fs::remove_file(&depgraph_path).ok();
    state.dep_graph_persisted.store(false, Ordering::Release);

    let started = MaintenanceSchedule::new(
        Arc::clone(&state),
        MaintenancePolicy::default(),
        ServiceMode::Embedded,
    )
    .with_intervals(fast_intervals())
    .start();
    assert!(started.started.contains(&TASK_DEPGRAPH_SAVE));

    let saved =
        wait_until(|| depgraph_path.exists() && state.dep_graph_persisted.load(Ordering::Acquire))
            .await;
    stop(&state);
    assert!(
        saved,
        "embedded mode must persist the depgraph on its own tick, without a host flush()"
    );
    assert!(
        state.dep_graph_persisted.load(Ordering::Acquire),
        "a periodic save must mark the graph persisted"
    );
}

#[tokio::test]
async fn depgraph_saves_are_exclusive_without_blocking_async_progress() {
    let root = tempfile::tempdir().expect("cache root");
    let state = test_state(&crate::core::NormalizedPath::new(root.path()));
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let first_done = Arc::new(AtomicBool::new(false));
    let first_flag = Arc::clone(&first_done);
    let first_state = Arc::clone(&state);
    let first = tokio::spawn(async move {
        run_depgraph_save_with(first_state, None, move |_| {
            entered_tx.send(()).expect("test observes first save");
            release_rx.recv().expect("test releases first save");
            first_flag.store(true, Ordering::Release);
        })
        .await
        .expect("first blocking save")
    });
    // On this single-threaded runtime, observing the held save at all proves
    // the save blocks outside the async runtime.
    entered_rx.await.expect("first save entered");
    tokio::spawn(async {})
        .await
        .expect("the runtime schedules other tasks while a save blocks");
    assert!(
        state.depgraph_persistence.try_lock().is_err(),
        "a running save holds the cache root's persistence guard"
    );

    let second_state = Arc::clone(&state);
    let second = tokio::spawn(async move {
        run_depgraph_save_with(second_state, None, move |_| {
            first_done.load(Ordering::Acquire)
        })
        .await
        .expect("second blocking save")
    });

    release_tx.send(()).expect("release first save");
    first.await.expect("first task");
    assert!(
        second.await.expect("second task"),
        "same-state depgraph saves must not overlap"
    );
}

/// #1684: a save queued behind another must write the graph that is current
/// when it gets the lock. The startup loader replaces the whole graph object
/// (`DepGraphSetter::install`), so a snapshot taken before the lock could
/// otherwise publish the graph the loader just replaced.
#[tokio::test]
async fn a_queued_depgraph_save_writes_the_graph_current_at_lock_time() {
    let root = tempfile::tempdir().expect("cache root");
    let state = test_state(&crate::core::NormalizedPath::new(root.path()));
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let first_state = Arc::clone(&state);
    let first = tokio::spawn(async move {
        run_depgraph_save_with(first_state, None, move |_| {
            entered_tx.send(()).expect("test observes first save");
            release_rx.recv().expect("test releases first save");
        })
        .await
        .expect("first blocking save")
    });
    entered_rx.await.expect("first save entered");

    let second_state = Arc::clone(&state);
    let second = tokio::spawn(async move {
        run_depgraph_save_with(second_state, None, |graph| {
            std::ptr::from_ref(graph) as usize
        })
        .await
        .expect("second blocking save")
    });

    let installed = Arc::new(crate::depgraph::DepGraph::new());
    let installed_addr = Arc::as_ptr(&installed) as usize;
    state.dep_graph.store(installed);
    release_tx.send(()).expect("release first save");
    first.await.expect("first task");
    assert_eq!(
        second.await.expect("second task"),
        installed_addr,
        "the queued save must serialize the graph installed while it waited"
    );
}

/// #1684: a panic inside the blocking save must reach the supervisor, which
/// logs its payload, writes the durable `background-task-died` event and
/// bounds restarts. Swallowing it as a `warn!` lost all three.
#[tokio::test]
async fn a_panicking_depgraph_save_is_reraised_for_the_supervisor() {
    let error = kernal_api::async_engine::launch_blocking(|| panic!("serializer exploded"))
        .await
        .expect_err("the blocking save panicked");
    let reraised = kernal_api::async_engine::launch(async move {
        reraise_depgraph_save_panic(&error);
    })
    .await
    .expect_err("a save panic must end the loop task");
    assert_eq!(
        supervise::panic_message(&reraised).as_deref(),
        Some("periodic depgraph save panicked: serializer exploded"),
        "the supervisor must see the original payload"
    );

    let cancelled = kernal_api::async_engine::launch(std::future::pending::<()>());
    cancelled.cancel();
    let cancelled = cancelled.await.expect_err("cancelled");
    reraise_depgraph_save_panic(&cancelled);
}

#[tokio::test]
async fn periodic_depgraph_task_is_owned_and_joins_after_shutdown() {
    let root = tempfile::tempdir().expect("cache root");
    let state = test_state(&crate::core::NormalizedPath::new(root.path()));
    let mut started = MaintenanceSchedule::new(
        Arc::clone(&state),
        MaintenancePolicy::default(),
        ServiceMode::Embedded,
    )
    .with_intervals(fast_intervals())
    .start();

    stop(&state);
    let handle = started
        .depgraph_save
        .take()
        .expect("periodic depgraph task must remain owned");
    handle
        .await
        .expect("periodic depgraph task joins cleanly after shutdown");
}

/// #1160(c): the staged-temp sweep was startup-only and standalone-only, so an
/// embedded host that never restarts its process never reclaimed an
/// interrupted publish's leftovers.
#[tokio::test]
async fn embedded_sweeps_staged_artifact_temps() {
    let root = tempfile::tempdir().expect("cache root");
    let cache_dir = crate::core::NormalizedPath::new(root.path());
    let state = test_state(&cache_dir);

    let staged_root = state.artifact_dir.join(".staged-v2");
    std::fs::create_dir_all(&staged_root).expect("staged root");
    let orphan = staged_root.join(".interrupted-write.tmp");
    std::fs::write(&orphan, b"partial").expect("orphan temp");

    let started = MaintenanceSchedule::new(
        Arc::clone(&state),
        MaintenancePolicy::default(),
        ServiceMode::Embedded,
    )
    .with_intervals(fast_intervals())
    .start();
    assert!(started.started.contains(&TASK_STAGED_TEMP_SWEEP));

    let swept = wait_until(|| !orphan.exists()).await;
    stop(&state);
    assert!(
        swept,
        "embedded mode must reclaim interrupted-write staged temp files"
    );
}

/// Host-owned disk maintenance suppresses only the disk loop. Suppressing the
/// in-memory members with it would restore the #1160(a) leak, because no host
/// API drives eviction or depgraph persistence.
#[tokio::test]
async fn host_owned_disk_maintenance_keeps_the_in_memory_members() {
    let root = tempfile::tempdir().expect("cache root");
    let state = test_state(&crate::core::NormalizedPath::new(root.path()));

    let started = MaintenanceSchedule::new(
        Arc::clone(&state),
        MaintenancePolicy::default(),
        ServiceMode::Embedded,
    )
    .with_intervals(fast_intervals())
    .with_disk_maintenance(false)
    .start();

    assert!(started.disk_maintenance.is_none());
    assert!(!started.started.contains(&TASK_DISK_MAINTENANCE));
    assert!(started.started.contains(&TASK_MEMORY_EVICTION));
    assert!(started.started.contains(&TASK_DEPGRAPH_SAVE));
    assert!(started.started.contains(&TASK_STAGED_TEMP_SWEEP));

    stop(&state);
}

// ─── #1659: retired version-store sweep ownership ────────────────────────

/// Zccache-owned disk maintenance (`disk_maintenance_enabled == true`, i.e.
/// `MaintenanceOwnership::Embedded`) reclaims a retired sibling `v<VERSION>`
/// store on its very first pass -- the loop body runs immediately with no
/// leading sleep, so no interval needs to elapse for this to happen.
#[tokio::test]
async fn zccache_owned_maintenance_reclaims_a_retired_version_store() {
    let top = tempfile::tempdir().expect("top-level cache root");
    let current = crate::core::config::versioned_subdir();
    let cache_dir = crate::core::NormalizedPath::new(top.path().join(&current));
    std::fs::create_dir_all(cache_dir.as_path()).expect("versioned cache dir");
    let state = test_state(&cache_dir);

    let retired = top.path().join("v0.0.1");
    std::fs::create_dir_all(&retired).expect("retired store");
    let victim = retired.join("blob.bin");
    std::fs::write(&victim, b"stale retired-store artifact").expect("victim artifact");
    // #1673: the periodic sweep only reclaims files older than the grace
    // period, so the stale artifact must actually be stale.
    let old = kernal_api::platform::fs::FileTime::from_system_time(
        std::time::SystemTime::now() - std::time::Duration::from_secs(30 * 24 * 60 * 60),
    );
    kernal_api::platform::fs::set_file_mtime(&victim, old).expect("backdate victim");
    // The disk loop waits for the startup artifact/depgraph loads, which the
    // real daemon runs; this test has no loader, so mark them complete.
    state.artifacts_loaded.store(true, Ordering::Release);
    state.dep_graph_load_complete.store(true, Ordering::Release);

    let started = MaintenanceSchedule::new(
        Arc::clone(&state),
        MaintenancePolicy::default(),
        ServiceMode::Embedded,
    )
    .with_intervals(fast_intervals())
    .start();
    assert!(started.started.contains(&TASK_DISK_MAINTENANCE));

    let swept = wait_until(|| !victim.exists()).await;
    stop(&state);
    assert!(
        swept,
        "zccache-owned maintenance must reclaim a retired version store"
    );
    assert!(
        !retired.exists(),
        "the fully-reclaimed retired store directory must itself be removed"
    );
}

/// Host-owned disk maintenance (`with_disk_maintenance(false)`, i.e.
/// `MaintenanceOwnership::Host`) never spawns the disk loop at all, so it
/// must not touch a retired version store either -- under host ownership the
/// host is expected to call the public sweep API from its own scheduler.
#[tokio::test]
async fn host_owned_maintenance_does_not_touch_retired_version_stores() {
    let top = tempfile::tempdir().expect("top-level cache root");
    let current = crate::core::config::versioned_subdir();
    let cache_dir = crate::core::NormalizedPath::new(top.path().join(&current));
    std::fs::create_dir_all(cache_dir.as_path()).expect("versioned cache dir");
    let state = test_state(&cache_dir);

    let retired = top.path().join("v0.0.1");
    std::fs::create_dir_all(&retired).expect("retired store");
    let victim = retired.join("blob.bin");
    std::fs::write(&victim, b"stale retired-store artifact").expect("victim artifact");
    // #1673: the periodic sweep only reclaims files older than the grace
    // period, so the stale artifact must actually be stale.
    let old = kernal_api::platform::fs::FileTime::from_system_time(
        std::time::SystemTime::now() - std::time::Duration::from_secs(30 * 24 * 60 * 60),
    );
    kernal_api::platform::fs::set_file_mtime(&victim, old).expect("backdate victim");
    // The disk loop waits for the startup artifact/depgraph loads, which the
    // real daemon runs; this test has no loader, so mark them complete.
    state.artifacts_loaded.store(true, Ordering::Release);
    state.dep_graph_load_complete.store(true, Ordering::Release);

    let started = MaintenanceSchedule::new(
        Arc::clone(&state),
        MaintenancePolicy::default(),
        ServiceMode::Embedded,
    )
    .with_intervals(fast_intervals())
    .with_disk_maintenance(false)
    .start();
    assert!(started.disk_maintenance.is_none());

    // No disk loop exists to run the sweep at all; give the (absent) loop a
    // beat it does not have before asserting nothing happened.
    tokio::time::sleep(Duration::from_millis(200)).await;
    stop(&state);
    assert!(
        victim.exists(),
        "host-owned maintenance must not run the retired-store sweep itself"
    );
}

// ─── zackees/soldr#2436 D5: batch-save decision core ─────────────────────

#[test]
fn a_batch_of_registrations_triggers_a_save_before_the_timer() {
    use super::super::maintenance_schedule::{depgraph_save_due, DEPGRAPH_SAVE_BATCH};
    use std::time::Duration;
    // Timer nowhere near due, but a full batch registered since last save.
    assert_eq!(
        depgraph_save_due(
            Duration::from_secs(5),
            Duration::from_secs(300),
            DEPGRAPH_SAVE_BATCH,
            0
        ),
        Some("batch")
    );
    // Growth measured against the last SAVED count, not zero.
    assert_eq!(
        depgraph_save_due(
            Duration::from_secs(5),
            Duration::from_secs(300),
            100 + DEPGRAPH_SAVE_BATCH,
            100
        ),
        Some("batch")
    );
}

#[test]
fn below_batch_growth_waits_for_the_interval() {
    use super::super::maintenance_schedule::{depgraph_save_due, DEPGRAPH_SAVE_BATCH};
    use std::time::Duration;
    assert_eq!(
        depgraph_save_due(
            Duration::from_secs(5),
            Duration::from_secs(300),
            DEPGRAPH_SAVE_BATCH - 1,
            0
        ),
        None
    );
    assert_eq!(
        depgraph_save_due(Duration::from_secs(300), Duration::from_secs(300), 1, 0),
        Some("interval")
    );
}
