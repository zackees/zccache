//! Integration tests for issue #320 - Option 2.
//!
//! A fresh daemon pointed at a populated cache dir must auto-classify the
//! session as warm by loading the persisted `depgraph.bin` written by a prior
//! session. Version-mismatch and corrupt files must fall back to cold AND
//! emit a clear warning visible in the per-session `last-session.log`.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::panic_in_result_fn, clippy::unwrap_in_result)]

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tokio::sync::Notify;
use tokio::task::JoinHandle;
use zccache::core::NormalizedPath;
use zccache::daemon::DaemonServer;
use zccache::depgraph::{
    classify_load, depgraph_file_path, save_to_file, CompileContext, ContextState, DepGraph,
    DepGraphLoadOutcome, IncludeSearchPaths, DEPGRAPH_VERSION,
};
use zccache::protocol::{Request, Response};

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
        let prev = std::env::var_os(zccache::core::config::CACHE_DIR_ENV);
        std::env::set_var(zccache::core::config::CACHE_DIR_ENV, tmp.path());
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
            Some(v) => std::env::set_var(zccache::core::config::CACHE_DIR_ENV, v),
            None => std::env::remove_var(zccache::core::config::CACHE_DIR_ENV),
        }
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

async fn start_daemon_with_warning(
    endpoint: &str,
    warning: Option<String>,
    preloaded_graph: Option<DepGraph>,
) -> (JoinHandle<()>, Arc<Notify>) {
    let mut server = DaemonServer::bind(endpoint).unwrap();
    if let Some(graph) = preloaded_graph {
        server.set_dep_graph(graph);
    }
    if let Some(w) = warning {
        server.set_depgraph_load_warning(w);
    }
    let shutdown = server.shutdown_handle();
    let handle = tokio::spawn(async move {
        server.run(0).await.unwrap();
    });
    (handle, shutdown)
}

async fn start_session_and_capture_log(endpoint: &str, log_file: &std::path::Path) -> String {
    let mut client = zccache::ipc::connect(endpoint).await.unwrap();
    client
        .send(&Request::SessionStart {
            client_pid: std::process::id(),
            working_dir: std::env::temp_dir().to_string_lossy().into_owned().into(),
            log_file: Some(log_file.to_string_lossy().into_owned().into()),
            track_stats: false,
            journal_path: None,
            profile: false,
            private_daemon: None,
        })
        .await
        .unwrap();
    match client.recv().await.unwrap() {
        Some(Response::SessionStarted { .. }) => {}
        other => panic!("unexpected: {other:?}"),
    }
    if log_file.exists() {
        std::fs::read_to_string(log_file).unwrap_or_default()
    } else {
        String::new()
    }
}

#[tokio::test]
#[ignore]
async fn valid_depgraph_makes_session_warm() {
    let _guard = CacheDirGuard::new();

    let graph = DepGraph::new();
    let key = graph.register(make_ctx("/src/main.cpp"));
    graph.update(
        &key,
        zccache::depgraph::ScanResult {
            resolved: vec![NormalizedPath::from("/inc/a.h")],
            unresolved: Vec::new(),
            has_computed: false,
        },
        |_| Some(zccache::hash::hash_bytes(b"x")),
    );
    assert_eq!(graph.get_state(&key), Some(ContextState::Warm));

    let path = depgraph_file_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    save_to_file(&graph, &path).unwrap();

    let outcome = classify_load(&path);
    let loaded = match outcome {
        DepGraphLoadOutcome::Loaded { graph } => graph,
        other => panic!("expected Loaded, got {other:?}"),
    };
    assert_eq!(loaded.stats().context_count, 1);
    assert_eq!(loaded.get_state(&key), Some(ContextState::Warm));
    assert!(!loaded.is_cold(&key), "reloaded context must be warm");
}

#[tokio::test]
#[ignore]
async fn corrupt_depgraph_emits_warning_in_session_log() {
    let _guard = CacheDirGuard::new();

    let path = depgraph_file_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(&path, [0xFFu8, 0xFF, 0xFF, 0xFF, b'n', b'o', b't']).unwrap();

    let outcome = classify_load(&path);
    assert!(matches!(outcome, DepGraphLoadOutcome::Corrupt { .. }));
    let warning = outcome.warning(&path).expect("corrupt must warn");

    let endpoint = zccache::ipc::unique_test_endpoint();
    let (handle, shutdown) = start_daemon_with_warning(&endpoint, Some(warning), None).await;

    let log_path: PathBuf =
        std::env::temp_dir().join(format!("zccache_320_corrupt_{}.log", std::process::id()));
    let _ = std::fs::remove_file(&log_path);

    let log_contents = start_session_and_capture_log(&endpoint, &log_path).await;

    assert!(
        log_contents.contains("corrupt"),
        "session log must mention corruption: {log_contents:?}"
    );
    assert!(
        log_contents.contains("treating session as cold"),
        "session log must mention cold fallback: {log_contents:?}"
    );

    shutdown.notify_one();
    handle.await.unwrap();
    let _ = std::fs::remove_file(&log_path);
}

#[tokio::test]
#[ignore]
async fn version_mismatch_depgraph_emits_warning_in_session_log() {
    let _guard = CacheDirGuard::new();

    let path = depgraph_file_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    let mut data = Vec::new();
    data.extend_from_slice(&[0x5A, 0x43, 0x44, 0x47]);
    data.extend_from_slice(&99u32.to_le_bytes());
    data.extend_from_slice(&0u64.to_le_bytes());
    std::fs::write(&path, &data).unwrap();

    let outcome = classify_load(&path);
    match &outcome {
        DepGraphLoadOutcome::VersionMismatch {
            file_version: 99,
            expected_version,
        } => assert_eq!(*expected_version, DEPGRAPH_VERSION),
        other => panic!("expected VersionMismatch, got {other:?}"),
    }
    let warning = outcome.warning(&path).expect("must warn");

    let endpoint = zccache::ipc::unique_test_endpoint();
    let (handle, shutdown) = start_daemon_with_warning(&endpoint, Some(warning), None).await;

    let log_path: PathBuf =
        std::env::temp_dir().join(format!("zccache_320_vmm_{}.log", std::process::id()));
    let _ = std::fs::remove_file(&log_path);

    let log_contents = start_session_and_capture_log(&endpoint, &log_path).await;

    assert!(
        log_contents.contains("version 99"),
        "session log must mention file version: {log_contents:?}"
    );
    assert!(
        log_contents.contains("treating session as cold"),
        "session log must mention cold fallback: {log_contents:?}"
    );

    shutdown.notify_one();
    handle.await.unwrap();
    let _ = std::fs::remove_file(&log_path);
}

#[tokio::test]
#[ignore]
async fn missing_depgraph_emits_no_warning() {
    let _guard = CacheDirGuard::new();

    let path = depgraph_file_path();
    assert!(!path.exists(), "precondition: no depgraph on disk");

    let outcome = classify_load(&path);
    assert!(matches!(outcome, DepGraphLoadOutcome::Missing));
    assert!(outcome.warning(&path).is_none());

    let endpoint = zccache::ipc::unique_test_endpoint();
    let (handle, shutdown) = start_daemon_with_warning(&endpoint, None, None).await;

    let log_path: PathBuf =
        std::env::temp_dir().join(format!("zccache_320_missing_{}.log", std::process::id()));
    let _ = std::fs::remove_file(&log_path);

    let log_contents = start_session_and_capture_log(&endpoint, &log_path).await;

    assert!(
        !log_contents.contains("depgraph"),
        "missing depgraph must produce no session-log warning: {log_contents:?}"
    );

    shutdown.notify_one();
    handle.await.unwrap();
    let _ = std::fs::remove_file(&log_path);
}

#[tokio::test]
#[ignore]
async fn loaded_graph_with_missing_artifact_is_still_warm() {
    let _guard = CacheDirGuard::new();

    let graph = DepGraph::new();
    let key = graph.register(make_ctx("/src/orphan.cpp"));
    graph.update(
        &key,
        zccache::depgraph::ScanResult {
            resolved: Vec::new(),
            unresolved: Vec::new(),
            has_computed: false,
        },
        |_| Some(zccache::hash::hash_bytes(b"orphan")),
    );

    let path = depgraph_file_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    save_to_file(&graph, &path).unwrap();

    let outcome = classify_load(&path);
    let loaded = match outcome {
        DepGraphLoadOutcome::Loaded { graph } => graph,
        other => panic!("expected Loaded, got {other:?}"),
    };

    assert!(
        !loaded.is_cold(&key),
        "context must be warm-classified even when artifact bytes are missing",
    );
}
