//! Regression coverage for zackees/zccache#398.
//!
//! A setup-soldr restore starts a fresh daemon against a restored cache while
//! `target/` is absent. The fresh daemon must load both the artifact index and
//! persisted depgraph so unchanged rustc multi-output args are served from
//! cache and recreate the missing output tree.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, OnceLock};
use tokio::task::JoinHandle;

use zccache::core::NormalizedPath;
use zccache::daemon::DaemonServer;
use zccache::depgraph::{classify_load, depgraph_file_path, DepGraphLoadOutcome};
use zccache::protocol::{DaemonStatus, Request, Response};

#[cfg(unix)]
type ClientConn = zccache::ipc::IpcConnection;
#[cfg(windows)]
type ClientConn = zccache::ipc::IpcClientConnection;

fn env_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

struct CacheEnvGuard {
    _lock: MutexGuard<'static, ()>,
    previous_cache_dir: Option<OsString>,
}

impl CacheEnvGuard {
    fn new(cache_dir: &Path) -> Self {
        let lock = env_lock().lock().unwrap_or_else(|err| err.into_inner());
        let previous_cache_dir = std::env::var_os(zccache::core::config::CACHE_DIR_ENV);
        std::env::set_var(zccache::core::config::CACHE_DIR_ENV, cache_dir);
        Self {
            _lock: lock,
            previous_cache_dir,
        }
    }
}

impl Drop for CacheEnvGuard {
    fn drop(&mut self) {
        match self.previous_cache_dir.take() {
            Some(value) => std::env::set_var(zccache::core::config::CACHE_DIR_ENV, value),
            None => std::env::remove_var(zccache::core::config::CACHE_DIR_ENV),
        }
    }
}

async fn start_daemon_like_zccache_daemon() -> (String, JoinHandle<()>) {
    let endpoint = zccache::ipc::unique_test_endpoint();
    let depgraph_path = depgraph_file_path();
    let depgraph_load = classify_load(&depgraph_path);
    let depgraph_warning = depgraph_load.warning(&depgraph_path);

    let mut server = DaemonServer::bind(&endpoint).expect("bind daemon");
    if let DepGraphLoadOutcome::Loaded { graph } = depgraph_load {
        server.set_dep_graph(graph);
    }
    if let Some(warning) = depgraph_warning {
        server.set_depgraph_load_warning(warning);
    }

    let handle = tokio::spawn(async move {
        server.run(0).await.expect("daemon run");
    });

    (endpoint, handle)
}

async fn connect(endpoint: &str) -> ClientConn {
    zccache::ipc::connect(endpoint)
        .await
        .expect("connect daemon")
}

async fn start_session(client: &mut ClientConn, working_dir: &Path) -> String {
    client
        .send(&Request::SessionStart {
            client_pid: std::process::id(),
            working_dir: NormalizedPath::from(working_dir),
            log_file: None,
            track_stats: false,
            journal_path: None,
            profile: false,
            private_daemon: None,
        })
        .await
        .expect("send SessionStart");

    match client.recv().await.expect("recv SessionStarted") {
        Some(Response::SessionStarted { session_id, .. }) => session_id,
        other => panic!("expected SessionStarted, got {other:?}"),
    }
}

async fn end_session(client: &mut ClientConn, session_id: String) {
    client
        .send(&Request::SessionEnd { session_id })
        .await
        .expect("send SessionEnd");
    match client.recv().await.expect("recv SessionEnded") {
        Some(Response::SessionEnded { .. }) => {}
        other => panic!("expected SessionEnded, got {other:?}"),
    }
}

struct CompileOutcome {
    exit_code: i32,
    stderr: Vec<u8>,
    cached: bool,
}

async fn compile_rustc(
    client: &mut ClientConn,
    session_id: &str,
    compiler: &Path,
    args: &[String],
    cwd: &Path,
) -> CompileOutcome {
    client
        .send(&Request::Compile {
            session_id: session_id.to_string(),
            args: args.to_vec(),
            cwd: NormalizedPath::from(cwd),
            compiler: NormalizedPath::new(compiler),
            env: None,
            stdin: Vec::new(),
        })
        .await
        .expect("send Compile");

    match client.recv().await.expect("recv CompileResult") {
        Some(Response::CompileResult {
            exit_code,
            stderr,
            cached,
            ..
        }) => CompileOutcome {
            exit_code,
            stderr: (*stderr).clone(),
            cached,
        },
        Some(Response::Error { message }) => panic!("compile error: {message}"),
        other => panic!("expected CompileResult, got {other:?}"),
    }
}

async fn get_status(client: &mut ClientConn) -> DaemonStatus {
    client.send(&Request::Status).await.expect("send Status");
    match client.recv().await.expect("recv Status") {
        Some(Response::Status(status)) => status,
        other => panic!("expected Status, got {other:?}"),
    }
}

async fn shutdown_daemon(mut client: ClientConn, handle: JoinHandle<()>) {
    client
        .send(&Request::Shutdown)
        .await
        .expect("send Shutdown");
    match client.recv().await.expect("recv ShuttingDown") {
        Some(Response::ShuttingDown) => {}
        other => panic!("expected ShuttingDown, got {other:?}"),
    }
    drop(client);
    handle.await.expect("daemon task join");
}

fn create_tiny_project(project_dir: &Path) {
    std::fs::create_dir_all(project_dir.join("src")).expect("create src");
    std::fs::create_dir_all(project_dir.join("target/debug/deps")).expect("create deps");
    std::fs::write(
        project_dir.join("Cargo.toml"),
        "[package]\nname = \"restore-hit\"\nversion = \"0.0.0\"\nedition = \"2021\"\n",
    )
    .expect("write Cargo.toml");
    std::fs::write(
        project_dir.join("src/lib.rs"),
        "pub fn answer() -> i32 { 398 }\n",
    )
    .expect("write lib.rs");
}

fn rustc_multi_output_args() -> Vec<String> {
    const SUFFIX: &str = "z398";
    vec![
        "--edition".into(),
        "2021".into(),
        "--crate-type".into(),
        "lib".into(),
        "--crate-name".into(),
        "restore_hit".into(),
        "--emit=dep-info,metadata,link".into(),
        "-C".into(),
        "embed-bitcode=no".into(),
        "-C".into(),
        format!("metadata={SUFFIX}"),
        "-C".into(),
        format!("extra-filename=-{SUFFIX}"),
        "--out-dir".into(),
        "target/debug/deps".into(),
        "src/lib.rs".into(),
    ]
}

fn expected_outputs(project_dir: &Path) -> Vec<PathBuf> {
    let deps = project_dir.join("target/debug/deps");
    vec![
        deps.join("librestore_hit-z398.rlib"),
        deps.join("librestore_hit-z398.rmeta"),
        deps.join("restore_hit-z398.d"),
    ]
}

fn assert_outputs_exist(outputs: &[PathBuf], context: &str) {
    for output in outputs {
        assert!(
            output.is_file(),
            "{context}: expected output {} to exist",
            output.display(),
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rustc_multi_output_hit_survives_cache_restore_without_target_dir() {
    let rustc = match zccache::test_support::find_rustc() {
        Some(path) => path,
        None => {
            eprintln!("skipping test: rustc not found");
            return;
        }
    };

    zccache::test_support::test_timeout(async move {
        let tmp = tempfile::tempdir().expect("tempdir");
        let cache_dir = tmp.path().join("zccache-cache");
        let project_dir = tmp.path().join("workspace");
        std::fs::create_dir_all(&cache_dir).expect("create cache dir");
        create_tiny_project(&project_dir);
        let _cache_env = CacheEnvGuard::new(&cache_dir);

        let args = rustc_multi_output_args();
        let outputs = expected_outputs(&project_dir);

        let (endpoint1, handle1) = start_daemon_like_zccache_daemon().await;
        let mut client1 = connect(&endpoint1).await;
        let session1 = start_session(&mut client1, &project_dir).await;
        let first = compile_rustc(
            &mut client1,
            &session1,
            rustc.as_path(),
            &args,
            &project_dir,
        )
        .await;
        assert_eq!(
            first.exit_code,
            0,
            "first rustc compile failed: {}",
            String::from_utf8_lossy(&first.stderr),
        );
        assert!(!first.cached, "first compile should populate the cache");
        assert_outputs_exist(&outputs, "first compile");
        end_session(&mut client1, session1).await;
        shutdown_daemon(client1, handle1).await;

        let depgraph_path = depgraph_file_path();
        assert!(
            depgraph_path.is_file(),
            "graceful shutdown should flush depgraph to {}",
            depgraph_path.display(),
        );

        // Issue #761 / #762 Phase 0: `index.bin` lives under
        // `<cache>/v<VERSION>/index.bin` so `index_path_from_cache_dir`
        // needs the versioned subdir prepended.
        let cache_dir_norm =
            NormalizedPath::new(&cache_dir).join(zccache::core::config::versioned_subdir());
        let index_path = zccache::core::config::index_path_from_cache_dir(&cache_dir_norm);
        let index_len = std::fs::metadata(&index_path)
            .map(|meta| meta.len())
            .unwrap_or(0);
        assert!(
            index_len > 0,
            "graceful shutdown should flush non-empty artifact index at {}",
            index_path.display(),
        );

        std::fs::remove_dir_all(project_dir.join("target")).expect("remove target");
        for output in &outputs {
            assert!(
                !output.exists(),
                "test setup should remove output {}",
                output.display(),
            );
        }

        let (endpoint2, handle2) = start_daemon_like_zccache_daemon().await;
        let mut client2 = connect(&endpoint2).await;
        let status = get_status(&mut client2).await;
        assert!(
            status.dep_graph_persisted,
            "fresh daemon should load the persisted depgraph: {status:?}",
        );
        assert!(
            status.dep_graph_contexts > 0,
            "loaded depgraph should contain the first compile context: {status:?}",
        );

        let session2 = start_session(&mut client2, &project_dir).await;
        let second = compile_rustc(
            &mut client2,
            &session2,
            rustc.as_path(),
            &args,
            &project_dir,
        )
        .await;
        assert_eq!(
            second.exit_code,
            0,
            "second rustc compile failed: {}",
            String::from_utf8_lossy(&second.stderr),
        );
        assert!(
            second.cached,
            "second compile should be restored from the cache after target/ deletion",
        );
        assert_outputs_exist(&outputs, "cached restore");
        end_session(&mut client2, session2).await;
        shutdown_daemon(client2, handle2).await;
    })
    .await;
}
