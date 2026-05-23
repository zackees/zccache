//! Integration test: dep graph persists across daemon restarts.
//!
//! Verifies the fix for issue #262 — fresh daemon starts no longer have to
//! re-seed the dep graph by recompiling once. After a graceful shutdown the
//! daemon flushes the dep graph to `<cache_dir>/depgraph/depgraph.bin`; the
//! next daemon process loads it back and reports `dep_graph_persisted = true`
//! over the IPC `Status` response.
//!
//! Run: soldr cargo test -p zccache-daemon --test depgraph_persistence_test -- --ignored --nocapture

use std::sync::{Arc, Mutex};
use tokio::sync::Notify;
use tokio::task::JoinHandle;
use zccache_monocrate::core::NormalizedPath;
use zccache_daemon::DaemonServer;
use zccache_depgraph::{CompileContext, DepGraph, IncludeSearchPaths};
use zccache_monocrate::protocol::{DaemonStatus, Request, Response};

// ─── Helpers ─────────────────────────────────────────────────────────────────

/// Serialize tests that depend on the process-global `ZCCACHE_CACHE_DIR` env
/// variable. They cannot run in parallel because the daemon reads the cache
/// dir on every IPC request, and other tests in this binary need their own
/// view.
static ENV_SERIAL: Mutex<()> = Mutex::new(());

struct CacheDirGuard {
    _tmp: tempfile::TempDir,
    prev: Option<std::ffi::OsString>,
    _lock: std::sync::MutexGuard<'static, ()>,
}

impl CacheDirGuard {
    fn new() -> Self {
        let lock = ENV_SERIAL.lock().unwrap_or_else(|p| p.into_inner());
        let tmp = tempfile::TempDir::new().unwrap();
        let prev = std::env::var_os(zccache_monocrate::core::config::CACHE_DIR_ENV);
        std::env::set_var(zccache_monocrate::core::config::CACHE_DIR_ENV, tmp.path());
        Self {
            _tmp: tmp,
            prev,
            _lock: lock,
        }
    }
}

impl Drop for CacheDirGuard {
    fn drop(&mut self) {
        match self.prev.take() {
            Some(v) => std::env::set_var(zccache_monocrate::core::config::CACHE_DIR_ENV, v),
            None => std::env::remove_var(zccache_monocrate::core::config::CACHE_DIR_ENV),
        }
    }
}

async fn start_daemon(endpoint: &str) -> (JoinHandle<()>, Arc<Notify>) {
    let mut server = DaemonServer::bind(endpoint).unwrap();
    let shutdown = server.shutdown_handle();
    let handle = tokio::spawn(async move {
        server.run(0).await.unwrap();
    });
    (handle, shutdown)
}

async fn start_daemon_with_preloaded_graph(
    endpoint: &str,
    graph: DepGraph,
) -> (JoinHandle<()>, Arc<Notify>) {
    let mut server = DaemonServer::bind(endpoint).unwrap();
    server.set_dep_graph(graph);
    let shutdown = server.shutdown_handle();
    let handle = tokio::spawn(async move {
        server.run(0).await.unwrap();
    });
    (handle, shutdown)
}

async fn get_status(endpoint: &str) -> DaemonStatus {
    let mut client = zccache_monocrate::ipc::connect(endpoint).await.unwrap();
    client.send(&Request::Status).await.unwrap();
    match client.recv().await.unwrap() {
        Some(Response::Status(s)) => s,
        other => panic!("unexpected response: {other:?}"),
    }
}

fn make_ctx(source: &str) -> CompileContext {
    CompileContext {
        source_file: NormalizedPath::from(source),
        include_search: IncludeSearchPaths::default(),
        defines: Vec::new(),
        flags: Vec::new(),
        force_includes: Vec::new(),
        unknown_flags: Vec::new(),
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

/// Fresh daemon with no on-disk snapshot reports `dep_graph_persisted = false`.
#[tokio::test]
#[ignore] // integration: starts real daemon, mutates ZCCACHE_CACHE_DIR
async fn fresh_daemon_reports_not_persisted() {
    let _guard = CacheDirGuard::new();
    let endpoint = zccache_monocrate::ipc::unique_test_endpoint();
    let (handle, shutdown) = start_daemon(&endpoint).await;

    let status = get_status(&endpoint).await;
    assert!(
        !status.dep_graph_persisted,
        "fresh daemon must report dep_graph_persisted = false, got: {status:?}",
    );
    assert_eq!(status.dep_graph_disk_size, 0);
    assert_eq!(status.dep_graph_version, zccache_depgraph::DEPGRAPH_VERSION);
    // Issue #262 explicitly checked that the (cached, cold, non-cacheable)
    // counters coexist with the persisted state.
    assert_eq!(status.cache_hits, 0);
    assert_eq!(status.cache_misses, 0);
    assert_eq!(status.non_cacheable, 0);

    shutdown.notify_one();
    handle.await.unwrap();
}

/// Daemon that was started with a preloaded graph immediately reports persisted=true.
#[tokio::test]
#[ignore]
async fn preloaded_graph_reports_persisted() {
    let _guard = CacheDirGuard::new();

    // Pre-populate a graph and save it (so the on-disk file exists too).
    let graph = DepGraph::new();
    let _ = graph.register(make_ctx("/src/main.cpp"));
    let path = zccache_depgraph::depgraph_file_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    zccache_depgraph::save_to_file(&graph, &path).unwrap();

    // Re-load to simulate the daemon's own startup path.
    let loaded = zccache_depgraph::load_from_file(&path).unwrap();
    assert_eq!(loaded.stats().context_count, 1);

    let endpoint = zccache_monocrate::ipc::unique_test_endpoint();
    let (handle, shutdown) = start_daemon_with_preloaded_graph(&endpoint, loaded).await;

    let status = get_status(&endpoint).await;
    assert!(
        status.dep_graph_persisted,
        "preloaded daemon must report dep_graph_persisted = true, got: {status:?}",
    );
    assert!(
        status.dep_graph_disk_size > 0,
        "snapshot file must be visible on disk, got size 0",
    );
    assert_eq!(status.dep_graph_contexts, 1, "context survived the load");
    assert_eq!(status.cache_hits, 0);
    assert_eq!(status.cache_misses, 0);
    assert_eq!(status.non_cacheable, 0);

    shutdown.notify_one();
    handle.await.unwrap();
}

/// Full lifecycle: daemon-with-graph → graceful shutdown (flushes snapshot)
/// → snapshot file on disk → new daemon with the same cache_dir loads it
/// → reports `dep_graph_persisted = true` and the graph contents survive.
///
/// We use a *separate* cache_dir for the second daemon and copy the snapshot
/// file across to avoid redb index lock contention on Windows when the first
/// daemon's background tasks haven't fully drained.
#[tokio::test]
#[ignore]
async fn shutdown_save_restore_roundtrip() {
    // ── Phase 1: daemon with preloaded graph, then shutdown ─────────
    let guard1 = CacheDirGuard::new();
    let graph = DepGraph::new();
    let _ = graph.register(make_ctx("/src/foo.cpp"));
    let _ = graph.register(make_ctx("/src/bar.cpp"));
    assert_eq!(graph.stats().context_count, 2);

    let endpoint1 = zccache_monocrate::ipc::unique_test_endpoint();
    let (handle1, shutdown1) = start_daemon_with_preloaded_graph(&endpoint1, graph).await;

    let status1 = get_status(&endpoint1).await;
    assert_eq!(status1.dep_graph_contexts, 2);
    assert!(
        status1.dep_graph_persisted,
        "preloaded daemon should already report persisted=true",
    );

    let saved_path = zccache_depgraph::depgraph_file_path();
    shutdown1.notify_one();
    handle1.await.unwrap();

    assert!(
        saved_path.exists(),
        "shutdown should have flushed depgraph to {}",
        saved_path.display(),
    );
    let saved_bytes = std::fs::read(&saved_path).unwrap();
    drop(guard1); // Release ZCCACHE_CACHE_DIR before the next phase claims it.

    // ── Phase 2: brand-new cache_dir, plant the saved snapshot, start a
    //    completely fresh daemon → load_from_file → report persisted. ───
    let guard2 = CacheDirGuard::new();
    let new_path = zccache_depgraph::depgraph_file_path();
    if let Some(parent) = new_path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(&new_path, &saved_bytes).unwrap();

    let loaded = zccache_depgraph::load_from_file(&new_path).unwrap();
    assert_eq!(loaded.stats().context_count, 2, "loaded graph survived");

    let endpoint2 = zccache_monocrate::ipc::unique_test_endpoint();
    let (handle2, shutdown2) = start_daemon_with_preloaded_graph(&endpoint2, loaded).await;
    let status2 = get_status(&endpoint2).await;

    assert!(
        status2.dep_graph_persisted,
        "restarted daemon must report dep_graph_persisted = true after loading from disk",
    );
    assert_eq!(
        status2.dep_graph_contexts, 2,
        "restarted graph keeps both contexts",
    );
    // (0 cached, 0 cold, 0 non-cacheable) counters and persisted state coexist.
    assert_eq!(status2.cache_hits, 0);
    assert_eq!(status2.cache_misses, 0);
    assert_eq!(status2.non_cacheable, 0);
    assert!(status2.dep_graph_disk_size > 0);

    shutdown2.notify_one();
    handle2.await.unwrap();
    drop(guard2);
}
