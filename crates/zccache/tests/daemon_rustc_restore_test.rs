//! Regression coverage for zackees/zccache#398.
//!
//! A setup-soldr restore starts a fresh daemon against a restored cache while
//! `target/` is absent. The fresh daemon must load both the artifact index and
//! persisted depgraph so unchanged rustc multi-output args are served from
//! cache and recreate the missing output tree.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::panic_in_result_fn,
    clippy::unwrap_in_result
)]

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, OnceLock};
use tokio::task::JoinHandle;

use zccache::core::NormalizedPath;
use zccache::daemon::DaemonServer;
use zccache::depgraph::{classify_load, depgraph_file_path, DepGraphLoadOutcome};
use zccache::protocol::{DaemonStatus, Request, Response, SessionStats};

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

async fn start_daemon_with_delayed_background_depgraph_load(
    delay: std::time::Duration,
) -> (String, JoinHandle<()>, JoinHandle<()>) {
    let endpoint = zccache::ipc::unique_test_endpoint();
    let depgraph_path = depgraph_file_path();
    let mut server = DaemonServer::bind(&endpoint).expect("bind daemon");
    server.mark_dep_graph_load_pending();
    let setter = server.dep_graph_setter();

    let load_handle = tokio::spawn(async move {
        tokio::time::sleep(delay).await;
        let depgraph_load = classify_load(&depgraph_path);
        let depgraph_warning = depgraph_load.warning(&depgraph_path);
        match depgraph_load {
            DepGraphLoadOutcome::Loaded { graph } => {
                setter.install(Some(graph), depgraph_warning);
            }
            DepGraphLoadOutcome::Missing
            | DepGraphLoadOutcome::VersionMismatch { .. }
            | DepGraphLoadOutcome::Corrupt { .. }
            | DepGraphLoadOutcome::IoError { .. } => {
                setter.install(None, depgraph_warning);
            }
        }
    });

    let handle = tokio::spawn(async move {
        server.run(0).await.expect("daemon run");
    });

    (endpoint, handle, load_handle)
}

async fn connect(endpoint: &str) -> ClientConn {
    zccache::ipc::connect(endpoint)
        .await
        .expect("connect daemon")
}

async fn start_session(client: &mut ClientConn, working_dir: &Path) -> String {
    start_session_with_options(client, working_dir, None, false).await
}

async fn start_session_with_log(
    client: &mut ClientConn,
    working_dir: &Path,
    log_file: Option<NormalizedPath>,
) -> String {
    start_session_with_options(client, working_dir, log_file, false).await
}

async fn start_session_with_options(
    client: &mut ClientConn,
    working_dir: &Path,
    log_file: Option<NormalizedPath>,
    track_stats: bool,
) -> String {
    client
        .send(&Request::SessionStart {
            client_pid: std::process::id(),
            working_dir: NormalizedPath::from(working_dir),
            log_file,
            track_stats,
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

async fn query_session_stats(client: &mut ClientConn, session_id: &str) -> Option<SessionStats> {
    client
        .send(&Request::SessionStats {
            session_id: session_id.to_string(),
        })
        .await
        .expect("send SessionStats");

    match client.recv().await.expect("recv SessionStatsResult") {
        Some(Response::SessionStatsResult { stats }) => stats,
        other => panic!("expected SessionStatsResult, got {other:?}"),
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

fn single_artifact_key_in(cache_dir: &Path) -> String {
    let cache_dir = zccache::core::config::effective_cache_root_from_top_level(
        &NormalizedPath::from(cache_dir),
    );
    let artifact_dir = zccache::core::config::artifacts_dir_from_cache_dir(&cache_dir);
    let mut keys = std::collections::BTreeSet::new();
    for entry in std::fs::read_dir(&artifact_dir).expect("read artifact dir") {
        let entry = entry.expect("artifact dir entry");
        if !entry.file_type().expect("artifact file type").is_file() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        if let Some(key) = name.strip_suffix(".pack") {
            keys.insert(key.to_string());
        } else if let Some((key, _index)) = name.split_once('_') {
            keys.insert(key.to_string());
        }
    }
    assert_eq!(
        keys.len(),
        1,
        "expected one artifact key in isolated cache, found {keys:?}"
    );
    keys.into_iter().next().expect("one artifact key")
}

fn remove_artifact_payloads(cache_dir: &Path, artifact_key_hex: &str) {
    let cache_dir = zccache::core::config::effective_cache_root_from_top_level(
        &NormalizedPath::from(cache_dir),
    );
    let artifact_dir = zccache::core::config::artifacts_dir_from_cache_dir(&cache_dir);
    let mut removed = 0;
    for entry in std::fs::read_dir(&artifact_dir).expect("read artifact dir") {
        let entry = entry.expect("artifact dir entry");
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if name == format!("{artifact_key_hex}.pack")
            || name.starts_with(&format!("{artifact_key_hex}_"))
        {
            std::fs::remove_file(path).expect("remove artifact payload");
            removed += 1;
        }
    }
    assert!(
        removed > 0,
        "expected to remove artifact payloads for {artifact_key_hex}"
    );
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn depgraph_hit_artifact_not_found_invalidates_stale_entry() {
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

        let artifact_key_hex = single_artifact_key_in(&cache_dir);
        remove_artifact_payloads(&cache_dir, &artifact_key_hex);
        for output in &outputs {
            let _ = std::fs::remove_file(output);
        }

        let (endpoint2, handle2) = start_daemon_like_zccache_daemon().await;
        let mut client2 = connect(&endpoint2).await;
        let log_path = tmp.path().join("artifact-miss-session.log");
        let session2 = start_session_with_options(
            &mut client2,
            &project_dir,
            Some(NormalizedPath::from(log_path.as_path())),
            true,
        )
        .await;
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
            "second rustc compile failed: {}\nsession log:\n{}",
            String::from_utf8_lossy(&second.stderr),
            std::fs::read_to_string(&log_path).unwrap_or_default(),
        );
        assert!(
            !second.cached,
            "missing artifact payload should force a miss and repopulate"
        );
        let stats = query_session_stats(&mut client2, &session2)
            .await
            .expect("stats tracking enabled");
        assert_eq!(stats.lookup_outcomes.depgraph_hit_artifact_miss, 1);
        assert_eq!(stats.lookup_outcomes.depgraph_hit_artifact_hit, 0);
        let session_log = std::fs::read_to_string(&log_path).unwrap_or_default();
        assert!(
            session_log.contains(&format!(
                "[DIAG] artifact_not_found: key={artifact_key_hex}"
            )),
            "expected artifact_not_found diagnostic; session log:\n{session_log}",
        );
        assert!(
            session_log.contains(&format!(
                "[DIAG] depgraph_invalidate_artifact: key={artifact_key_hex} cleared=1"
            )),
            "expected stale depgraph artifact invalidation; session log:\n{session_log}",
        );
        assert_outputs_exist(&outputs, "miss repopulates outputs");

        for output in &outputs {
            let _ = std::fs::remove_file(output);
        }
        let third = compile_rustc(
            &mut client2,
            &session2,
            rustc.as_path(),
            &args,
            &project_dir,
        )
        .await;
        assert_eq!(
            third.exit_code,
            0,
            "third rustc compile failed: {}\nsession log:\n{}",
            String::from_utf8_lossy(&third.stderr),
            std::fs::read_to_string(&log_path).unwrap_or_default(),
        );
        assert!(
            third.cached,
            "repopulated artifact should be restorable after stale entry invalidation"
        );
        let stats = query_session_stats(&mut client2, &session2)
            .await
            .expect("stats tracking enabled");
        assert_eq!(stats.lookup_outcomes.depgraph_hit_artifact_miss, 1);
        assert_eq!(stats.lookup_outcomes.depgraph_hit_artifact_hit, 1);
        assert_outputs_exist(&outputs, "third compile restores outputs");

        end_session(&mut client2, session2).await;
        shutdown_daemon(client2, handle2).await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn first_compile_waits_for_background_depgraph_load_before_cold_skip() {
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

        let (endpoint2, handle2, load_handle) =
            start_daemon_with_delayed_background_depgraph_load(std::time::Duration::from_secs(3))
                .await;
        let mut client2 = connect(&endpoint2).await;
        let log_path = tmp.path().join("delayed-load-session.log");
        let session2 =
            start_session_with_log(
                &mut client2,
                &project_dir,
                Some(NormalizedPath::from(log_path.as_path())),
            )
            .await;
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
            "second rustc compile failed: {}\nsession log:\n{}",
            String::from_utf8_lossy(&second.stderr),
            std::fs::read_to_string(&log_path).unwrap_or_default(),
        );
        let session_log = std::fs::read_to_string(&log_path).unwrap_or_default();
        assert!(
            session_log.contains("depgraph_load_pending: waiting"),
            "first compile should observe the pending startup depgraph load; session log:\n{session_log}",
        );
        assert!(
            !session_log.contains("reason=cold_skip"),
            "first compile after daemon readiness must wait for the persisted depgraph instead of racing into cold_skip; session log:\n{session_log}",
        );
        assert!(
            session_log.contains("verdict=Hit") || session_log.contains("verdict=SourceChanged"),
            "first compile should consult the loaded depgraph after the wait; session log:\n{session_log}",
        );
        assert_outputs_exist(&outputs, "cached restore");
        load_handle.await.expect("depgraph load task join");
        end_session(&mut client2, session2).await;
        shutdown_daemon(client2, handle2).await;
    })
    .await;
}
