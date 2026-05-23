//! Regression coverage for issue #210: Rust cold misses must populate the
//! daemon cache before observable warm-cache behavior is reported.
//!
//! Run all:
//!   cargo test -p zccache-daemon --test rustc_issue_210_async_populate_test -- --ignored --nocapture

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use tokio::sync::Notify;
use tokio::task::JoinHandle;
use zccache_monocrate::core::NormalizedPath;
use zccache_daemon::DaemonServer;
use zccache_protocol::{DaemonStatus, Request, Response, RustArtifactInfo};

#[cfg(unix)]
type ClientConn = zccache_ipc::IpcConnection;
#[cfg(windows)]
type ClientConn = zccache_ipc::IpcClientConnection;

fn env_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

struct CacheEnvGuard {
    _lock: MutexGuard<'static, ()>,
    old_cache_dir: Option<String>,
    old_profile_rust_miss: Option<String>,
    old_compile_priority: Option<String>,
}

impl CacheEnvGuard {
    fn new(cache_dir: &Path) -> Self {
        Self::new_inner(cache_dir, false)
    }

    fn new_profiled(cache_dir: &Path) -> Self {
        Self::new_inner(cache_dir, true)
    }

    fn new_inner(cache_dir: &Path, profiled: bool) -> Self {
        let lock = env_lock().lock().unwrap_or_else(|e| e.into_inner());
        let old_cache_dir = std::env::var(zccache_monocrate::core::config::CACHE_DIR_ENV).ok();
        let old_profile_rust_miss = std::env::var("ZCCACHE_PROFILE_RUST_MISS").ok();
        let old_compile_priority = std::env::var("ZCCACHE_COMPILE_PRIORITY").ok();
        unsafe {
            std::env::set_var(zccache_monocrate::core::config::CACHE_DIR_ENV, cache_dir);
            if profiled {
                std::env::set_var("ZCCACHE_PROFILE_RUST_MISS", "1");
                std::env::set_var("ZCCACHE_COMPILE_PRIORITY", "auto");
            }
        }
        Self {
            _lock: lock,
            old_cache_dir,
            old_profile_rust_miss,
            old_compile_priority,
        }
    }
}

impl Drop for CacheEnvGuard {
    fn drop(&mut self) {
        unsafe {
            match &self.old_cache_dir {
                Some(value) => std::env::set_var(zccache_monocrate::core::config::CACHE_DIR_ENV, value),
                None => std::env::remove_var(zccache_monocrate::core::config::CACHE_DIR_ENV),
            }
            match &self.old_profile_rust_miss {
                Some(value) => std::env::set_var("ZCCACHE_PROFILE_RUST_MISS", value),
                None => std::env::remove_var("ZCCACHE_PROFILE_RUST_MISS"),
            }
            match &self.old_compile_priority {
                Some(value) => std::env::set_var("ZCCACHE_COMPILE_PRIORITY", value),
                None => std::env::remove_var("ZCCACHE_COMPILE_PRIORITY"),
            }
        }
    }
}

async fn start_daemon() -> (String, JoinHandle<()>, Arc<Notify>) {
    let endpoint = zccache_ipc::unique_test_endpoint();
    start_daemon_at(&endpoint).await
}

async fn start_daemon_at(endpoint: &str) -> (String, JoinHandle<()>, Arc<Notify>) {
    let mut server = DaemonServer::bind(endpoint).unwrap();
    let shutdown = server.shutdown_handle();
    let handle = tokio::spawn(async move {
        server.run(0).await.unwrap();
    });
    (endpoint.to_string(), handle, shutdown)
}

async fn stop_daemon(handle: JoinHandle<()>, shutdown: Arc<Notify>) {
    shutdown.notify_one();
    handle.await.unwrap();
}

async fn start_session(client: &mut ClientConn) -> String {
    client
        .send(&Request::SessionStart {
            client_pid: std::process::id(),
            working_dir: std::env::current_dir().unwrap().into(),
            log_file: None,
            track_stats: false,
            journal_path: None,
            profile: false,
        })
        .await
        .unwrap();

    match client.recv().await.unwrap() {
        Some(Response::SessionStarted { session_id, .. }) => session_id,
        other => panic!("expected SessionStarted, got: {other:?}"),
    }
}

async fn compile(
    client: &mut ClientConn,
    session_id: &str,
    compiler: &Path,
    args: &[String],
    cwd: &Path,
) -> (i32, bool) {
    client
        .send(&Request::Compile {
            session_id: session_id.to_string(),
            args: args.to_vec(),
            cwd: cwd.to_path_buf().into(),
            compiler: NormalizedPath::new(compiler),
            env: None,
            stdin: Vec::new(),
        })
        .await
        .unwrap();

    match client.recv().await.unwrap() {
        Some(Response::CompileResult {
            exit_code, cached, ..
        }) => (exit_code, cached),
        Some(Response::Error { message }) => panic!("compile error: {message}"),
        other => panic!("expected CompileResult, got: {other:?}"),
    }
}

async fn get_status(client: &mut ClientConn) -> DaemonStatus {
    client.send(&Request::Status).await.unwrap();
    match client.recv().await.unwrap() {
        Some(Response::Status(status)) => status,
        other => panic!("expected Status, got: {other:?}"),
    }
}

async fn list_rust_artifacts(client: &mut ClientConn) -> Vec<RustArtifactInfo> {
    client.send(&Request::ListRustArtifacts).await.unwrap();
    match client.recv().await.unwrap() {
        Some(Response::RustArtifactList { artifacts }) => artifacts,
        other => panic!("expected RustArtifactList, got: {other:?}"),
    }
}

async fn clear_cache(client: &mut ClientConn) {
    client.send(&Request::Clear).await.unwrap();
    match client.recv().await.unwrap() {
        Some(Response::Cleared { .. }) => {}
        other => panic!("expected Cleared, got: {other:?}"),
    }
}

fn write_lib(path: &Path, value: i32) {
    std::fs::write(path, format!("pub fn value() -> i32 {{ {value} }}\n")).unwrap();
}

fn remove_if_exists(path: &Path) {
    if path.exists() {
        std::fs::remove_file(path).unwrap();
    }
}

fn remove_outputs(paths: &[PathBuf]) {
    for path in paths {
        remove_if_exists(path);
    }
}

fn simple_lib_args(src: &Path, output: &Path, crate_name: &str) -> Vec<String> {
    vec![
        "--edition".into(),
        "2021".into(),
        "--crate-type".into(),
        "lib".into(),
        "--crate-name".into(),
        crate_name.into(),
        "--emit=link".into(),
        src.to_string_lossy().into_owned(),
        "-o".into(),
        output.to_string_lossy().into_owned(),
    ]
}

fn cargo_build_args(src: &Path, out_dir: &Path, crate_name: &str, suffix: &str) -> Vec<String> {
    vec![
        "--edition".into(),
        "2021".into(),
        "--crate-type".into(),
        "lib".into(),
        "--crate-name".into(),
        crate_name.into(),
        "--emit=dep-info,metadata,link".into(),
        "-C".into(),
        "embed-bitcode=no".into(),
        "-C".into(),
        format!("metadata={suffix}"),
        "-C".into(),
        format!("extra-filename=-{suffix}"),
        "--out-dir".into(),
        out_dir.to_string_lossy().into_owned(),
        src.to_string_lossy().into_owned(),
    ]
}

fn cargo_check_args(src: &Path, out_dir: &Path, crate_name: &str, suffix: &str) -> Vec<String> {
    vec![
        "--edition".into(),
        "2021".into(),
        "--crate-type".into(),
        "lib".into(),
        "--crate-name".into(),
        crate_name.into(),
        "--emit=dep-info,metadata".into(),
        "-C".into(),
        format!("metadata={suffix}"),
        "-C".into(),
        format!("extra-filename=-{suffix}"),
        "--out-dir".into(),
        out_dir.to_string_lossy().into_owned(),
        src.to_string_lossy().into_owned(),
    ]
}

fn artifact_names(artifacts: &[RustArtifactInfo]) -> Vec<String> {
    artifacts
        .iter()
        .flat_map(|artifact| artifact.output_names.iter().cloned())
        .collect()
}

async fn wait_for_rust_artifacts(client: &mut ClientConn) -> Vec<RustArtifactInfo> {
    for _ in 0..20 {
        let artifacts = list_rust_artifacts(client).await;
        if !artifacts.is_empty() {
            return artifacts;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    list_rust_artifacts(client).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn profiled_rust_build_miss_populates_artifact() {
    let rustc = match zccache_test_support::find_rustc() {
        Some(path) => path,
        None => return,
    };

    zccache_test_support::test_timeout(async move {
        let tmp = tempfile::tempdir().unwrap();
        let _cache_env = CacheEnvGuard::new_profiled(&tmp.path().join("cache"));
        let src = tmp.path().join("lib.rs");
        let out_dir = tmp.path().join("deps");
        std::fs::create_dir_all(&out_dir).unwrap();
        write_lib(&src, 232);

        let (endpoint, server_handle, shutdown) = start_daemon().await;
        let mut client = zccache_ipc::connect(&endpoint).await.unwrap();
        let session_id = start_session(&mut client).await;
        let args = cargo_build_args(&src, &out_dir, "profile232", "profile232");

        let (exit_code, cached) =
            compile(&mut client, &session_id, &rustc, &args, tmp.path()).await;
        assert_eq!(exit_code, 0);
        assert!(!cached, "profiled build-mode cold compile should miss");

        let artifacts = list_rust_artifacts(&mut client).await;
        let names = artifact_names(&artifacts);
        assert!(
            names
                .iter()
                .any(|name| name.ends_with("libprofile232-profile232.rlib")),
            "profiled build miss should publish the rlib artifact; names={names:?}"
        );

        stop_daemon(server_handle, shutdown).await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore] // integration-level: starts real daemon with IPC + rustc
async fn issue_210_same_context_immediate_recompile_hits_after_miss() {
    let rustc = match zccache_test_support::find_rustc() {
        Some(path) => path,
        None => return,
    };

    zccache_test_support::test_timeout(async move {
        let tmp = tempfile::tempdir().unwrap();
        let _cache_env = CacheEnvGuard::new(&tmp.path().join("cache"));
        let src = tmp.path().join("lib.rs");
        let output = tmp.path().join("libissue210.rlib");
        write_lib(&src, 210);

        let (endpoint, server_handle, shutdown) = start_daemon().await;
        let mut client = zccache_ipc::connect(&endpoint).await.unwrap();
        let session_id = start_session(&mut client).await;
        let args = simple_lib_args(&src, &output, "issue210");

        let (exit_code, cached) =
            compile(&mut client, &session_id, &rustc, &args, tmp.path()).await;
        assert_eq!(exit_code, 0);
        assert!(!cached, "cold compile should miss");

        remove_if_exists(&output);
        let (exit_code, cached) =
            compile(&mut client, &session_id, &rustc, &args, tmp.path()).await;
        assert_eq!(exit_code, 0);
        assert!(
            cached,
            "same-session immediate recompile should hit after a cold miss"
        );
        assert!(output.exists(), "hit should restore the compiled output");

        stop_daemon(server_handle, shutdown).await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore] // integration-level: starts real daemon with IPC + rustc
async fn issue_210_status_and_list_reflect_populated_rust_artifacts() {
    let rustc = match zccache_test_support::find_rustc() {
        Some(path) => path,
        None => return,
    };

    zccache_test_support::test_timeout(async move {
        let tmp = tempfile::tempdir().unwrap();
        let _cache_env = CacheEnvGuard::new(&tmp.path().join("cache"));
        let src = tmp.path().join("lib.rs");
        let out_dir = tmp.path().join("deps");
        std::fs::create_dir_all(&out_dir).unwrap();
        write_lib(&src, 211);

        let (endpoint, server_handle, shutdown) = start_daemon().await;
        let mut client = zccache_ipc::connect(&endpoint).await.unwrap();
        let session_id = start_session(&mut client).await;
        let args = cargo_build_args(&src, &out_dir, "issue210_status", "status210");

        let (exit_code, cached) =
            compile(&mut client, &session_id, &rustc, &args, tmp.path()).await;
        assert_eq!(exit_code, 0);
        assert!(!cached, "cold cargo-build style compile should miss");

        let status = get_status(&mut client).await;
        assert_eq!(status.cache_misses, 1, "status should record the cold miss");
        assert!(
            status.artifact_count > 0,
            "status should include the Rust artifact populated by the miss"
        );

        let artifacts = list_rust_artifacts(&mut client).await;
        let names = artifact_names(&artifacts);
        assert!(
            names
                .iter()
                .any(|name| name.ends_with("libissue210_status-status210.rlib")),
            "ListRustArtifacts should include the populated rlib; names={names:?}"
        );
        assert!(
            names
                .iter()
                .any(|name| name.ends_with("libissue210_status-status210.rmeta")),
            "ListRustArtifacts should include the populated rmeta; names={names:?}"
        );
        assert!(
            names
                .iter()
                .any(|name| name.ends_with("issue210_status-status210.d")),
            "ListRustArtifacts should include the populated dep-info file; names={names:?}"
        );

        stop_daemon(server_handle, shutdown).await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore] // integration-level: starts real daemon with IPC + rustc
async fn issue_210_clear_leaves_no_stale_rust_artifacts() {
    let rustc = match zccache_test_support::find_rustc() {
        Some(path) => path,
        None => return,
    };

    zccache_test_support::test_timeout(async move {
        let tmp = tempfile::tempdir().unwrap();
        let _cache_env = CacheEnvGuard::new(&tmp.path().join("cache"));
        let src = tmp.path().join("lib.rs");
        let output = tmp.path().join("libclear210.rlib");
        write_lib(&src, 212);

        let (endpoint, server_handle, shutdown) = start_daemon().await;
        let mut client = zccache_ipc::connect(&endpoint).await.unwrap();
        let session_id = start_session(&mut client).await;
        let args = simple_lib_args(&src, &output, "clear210");

        let (exit_code, cached) =
            compile(&mut client, &session_id, &rustc, &args, tmp.path()).await;
        assert_eq!(exit_code, 0);
        assert!(!cached, "initial compile should miss");
        assert!(
            !list_rust_artifacts(&mut client).await.is_empty(),
            "miss should populate a Rust artifact before Clear"
        );

        clear_cache(&mut client).await;
        let status = get_status(&mut client).await;
        assert_eq!(
            status.artifact_count, 0,
            "Clear should remove in-memory artifacts"
        );
        assert!(
            list_rust_artifacts(&mut client).await.is_empty(),
            "Clear should remove Rust artifacts from ListRustArtifacts"
        );

        remove_if_exists(&output);
        let (exit_code, cached) =
            compile(&mut client, &session_id, &rustc, &args, tmp.path()).await;
        assert_eq!(exit_code, 0);
        assert!(
            !cached,
            "recompile after Clear should miss instead of serving a stale artifact"
        );

        stop_daemon(server_handle, shutdown).await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore] // integration-level: starts real daemon with IPC + rustc
async fn issue_210_shutdown_after_just_populated_rust_artifact() {
    let rustc = match zccache_test_support::find_rustc() {
        Some(path) => path,
        None => return,
    };

    zccache_test_support::test_timeout(async move {
        let tmp = tempfile::tempdir().unwrap();
        let _cache_env = CacheEnvGuard::new(&tmp.path().join("cache"));
        let src = tmp.path().join("lib.rs");
        let output = tmp.path().join("librestart210.rlib");
        write_lib(&src, 213);

        let (endpoint, server_handle, shutdown) = start_daemon().await;
        let mut client = zccache_ipc::connect(&endpoint).await.unwrap();
        let session_id = start_session(&mut client).await;
        let args = simple_lib_args(&src, &output, "restart210");

        let (exit_code, cached) =
            compile(&mut client, &session_id, &rustc, &args, tmp.path()).await;
        assert_eq!(exit_code, 0);
        assert!(!cached, "initial compile should miss");
        let artifacts = wait_for_rust_artifacts(&mut client).await;
        assert!(
            !artifacts.is_empty(),
            "Rust artifact should be visible before daemon shutdown"
        );
        drop(client);
        stop_daemon(server_handle, shutdown).await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore] // integration-level: starts real daemon with IPC + rustc
async fn issue_210_build_and_check_warm_hits_are_preserved() {
    let rustc = match zccache_test_support::find_rustc() {
        Some(path) => path,
        None => return,
    };

    zccache_test_support::test_timeout(async move {
        let tmp = tempfile::tempdir().unwrap();
        let _cache_env = CacheEnvGuard::new(&tmp.path().join("cache"));
        let src = tmp.path().join("lib.rs");
        let out_dir = tmp.path().join("deps");
        std::fs::create_dir_all(&out_dir).unwrap();
        write_lib(&src, 214);

        let (endpoint, server_handle, shutdown) = start_daemon().await;
        let mut client = zccache_ipc::connect(&endpoint).await.unwrap();
        let session_id = start_session(&mut client).await;

        let build_args = cargo_build_args(&src, &out_dir, "build210", "build210");
        let build_outputs = vec![
            out_dir.join("libbuild210-build210.rlib"),
            out_dir.join("libbuild210-build210.rmeta"),
            out_dir.join("build210-build210.d"),
        ];
        let (exit_code, cached) =
            compile(&mut client, &session_id, &rustc, &build_args, tmp.path()).await;
        assert_eq!(exit_code, 0);
        assert!(!cached, "build-mode cold compile should miss");
        remove_outputs(&build_outputs);
        let (exit_code, cached) =
            compile(&mut client, &session_id, &rustc, &build_args, tmp.path()).await;
        assert_eq!(exit_code, 0);
        assert!(cached, "build-mode warm compile should hit");

        let check_args = cargo_check_args(&src, &out_dir, "check210", "check210");
        let check_outputs = vec![
            out_dir.join("libcheck210-check210.rmeta"),
            out_dir.join("check210-check210.d"),
        ];
        let (exit_code, cached) =
            compile(&mut client, &session_id, &rustc, &check_args, tmp.path()).await;
        assert_eq!(exit_code, 0);
        assert!(!cached, "check-mode cold compile should miss");
        remove_outputs(&check_outputs);
        let (exit_code, cached) =
            compile(&mut client, &session_id, &rustc, &check_args, tmp.path()).await;
        assert_eq!(exit_code, 0);
        assert!(cached, "check-mode warm compile should hit");

        stop_daemon(server_handle, shutdown).await;
    })
    .await;
}
