//! #1652: embedded bring-up is attributable and does not wait on the depgraph.

use super::super::*;

fn lifecycle_events(cache_dir: &Path, event: &str) -> Vec<serde_json::Value> {
    fn walk(dir: &Path, out: &mut Vec<std::path::PathBuf>) {
        for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, out);
            } else if path
                .file_name()
                .is_some_and(|name| name.to_string_lossy().contains("daemon-lifecycle"))
            {
                out.push(path);
            }
        }
    }
    let mut logs = Vec::new();
    walk(cache_dir, &mut logs);
    logs.iter()
        .flat_map(|log| {
            std::fs::read_to_string(log)
                .unwrap_or_default()
                .lines()
                .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
                .collect::<Vec<_>>()
        })
        .filter(|record| record["event"] == event)
        .collect()
}

async fn start(cache_dir: &crate::core::NormalizedPath) -> EmbeddedDaemon {
    EmbeddedDaemon::start(
        crate::ipc::unique_test_endpoint(),
        cache_dir.clone(),
        None,
        MaintenancePolicy::default(),
    )
    .await
    .unwrap()
}

fn register_context(state: &SharedState, dir: &Path) {
    let source = dir.join("unit.c");
    std::fs::write(&source, "int unit(void) { return 0; }\n").unwrap();
    state.dep_graph.load().register(CompileContext {
        source_file: source.into(),
        include_search: crate::depgraph::IncludeSearchPaths::default(),
        defines: Vec::new(),
        flags: Vec::new(),
        force_includes: Vec::new(),
        unknown_flags: Vec::new(),
        compiler_hash: crate::hash::hash_bytes(b"bringup"),
    });
}

#[tokio::test]
async fn bringup_logs_every_phase_and_the_background_depgraph_load() {
    let tmp = tempfile::tempdir().unwrap();
    let cache_dir = crate::core::NormalizedPath::new(tmp.path());
    let daemon = start(&cache_dir).await;
    let loaded = super::super::embedded_bringup::await_depgraph_load(&daemon.state).await;
    daemon.shutdown().await;
    assert!(loaded);

    let bringup = lifecycle_events(tmp.path(), crate::core::lifecycle::EVENT_EMBEDDED_BRINGUP);
    assert_eq!(bringup.len(), 1, "one bring-up record per start");
    let record = &bringup[0];
    assert!(record["ready_ns"].is_u64());
    for phase in [
        "artifact_index",
        "metadata",
        "compiler_hash",
        "system_includes",
    ] {
        assert!(
            record["phases_ns"][phase].is_u64(),
            "bring-up phase {phase} must be timed: {record}"
        );
    }
    assert_eq!(record["depgraph_load"], "background");
    let loaded = lifecycle_events(
        tmp.path(),
        crate::core::lifecycle::EVENT_EMBEDDED_DEPGRAPH_LOADED,
    );
    assert_eq!(loaded.len(), 1);
    assert!(loaded[0]["elapsed_ns"].is_u64());
}

/// Resets the process-global load delay even if the test panics.
struct LoadDelay;

impl LoadDelay {
    fn set(ms: u64) -> Self {
        super::super::embedded_bringup::TEST_DEPGRAPH_LOAD_DELAY_MS.store(ms, Ordering::Release);
        Self
    }
}

impl Drop for LoadDelay {
    fn drop(&mut self) {
        super::super::embedded_bringup::TEST_DEPGRAPH_LOAD_DELAY_MS.store(0, Ordering::Release);
    }
}

#[tokio::test]
async fn readiness_does_not_wait_for_a_slow_depgraph_load() {
    let tmp = tempfile::tempdir().unwrap();
    let cache_dir = crate::core::NormalizedPath::new(tmp.path());
    let work = tempfile::tempdir().unwrap();

    // Persist a graph with one context.
    let first = start(&cache_dir).await;
    assert!(super::super::embedded_bringup::await_depgraph_load(&first.state).await);
    register_context(&first.state, work.path());
    first.shutdown().await;

    let delay = std::time::Duration::from_millis(1500);
    let _delay = LoadDelay::set(delay.as_millis() as u64);
    let started = std::time::Instant::now();
    let second = start(&cache_dir).await;
    let ready = started.elapsed();
    assert!(
        ready < delay,
        "readiness took {ready:?}; it must not wait for the {delay:?} depgraph load"
    );
    assert!(
        !second.state.dep_graph_load_complete.load(Ordering::Acquire),
        "compiles must still see the load as pending"
    );

    // Shutdown joins the load, so the restored graph is what gets saved.
    assert!(super::super::embedded_bringup::await_depgraph_load(&second.state).await);
    assert_eq!(second.state.dep_graph.load().stats().context_count, 1);
    second.shutdown().await;

    let third = start(&cache_dir).await;
    assert!(super::super::embedded_bringup::await_depgraph_load(&third.state).await);
    assert_eq!(
        third.state.dep_graph.load().stats().context_count,
        1,
        "a restart must still restore the persisted graph"
    );
    third.shutdown().await;
}

/// A flush issued before the background load installs must not replace the
/// on-disk graph with the empty default.
#[tokio::test]
async fn early_flush_keeps_the_graph_the_load_is_restoring() {
    let tmp = tempfile::tempdir().unwrap();
    let cache_dir = crate::core::NormalizedPath::new(tmp.path());
    let work = tempfile::tempdir().unwrap();
    let first = start(&cache_dir).await;
    assert!(super::super::embedded_bringup::await_depgraph_load(&first.state).await);
    register_context(&first.state, work.path());
    first.shutdown().await;

    let _delay = LoadDelay::set(500);
    let second = start(&cache_dir).await;
    let report = second.flush().await;
    assert!(report.is_complete(), "{report:?}");
    assert_eq!(second.state.dep_graph.load().stats().context_count, 1);
    second.shutdown().await;

    let third = start(&cache_dir).await;
    assert!(super::super::embedded_bringup::await_depgraph_load(&third.state).await);
    assert_eq!(third.state.dep_graph.load().stats().context_count, 1);
    third.shutdown().await;
}
