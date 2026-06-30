//! Integration tests for rustc path-remap caching behavior.
//!
//! Covers issue #229: `ZCCACHE_PATH_REMAP=auto` makes path-sensitive Rust
//! outputs stable enough to share across sibling Git roots, while leaving
//! remap unset (or pointing at different destination prefixes) must
//! conservatively miss instead of corrupting outputs.
//! Split out from `daemon_rustc_cache_test.rs` so each integration-test
//! binary stays small.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::panic_in_result_fn, clippy::unwrap_in_result)]

use zccache::core::NormalizedPath;
use zccache::daemon::DaemonServer;
use zccache::protocol::{Request, Response};

#[cfg(unix)]
type ClientConn = zccache::ipc::IpcConnection;
#[cfg(windows)]
type ClientConn = zccache::ipc::IpcClientConnection;

async fn start_daemon() -> (
    String,
    tokio::task::JoinHandle<()>,
    std::sync::Arc<tokio::sync::Notify>,
) {
    let endpoint = zccache::ipc::unique_test_endpoint();
    let mut server = DaemonServer::bind(&endpoint).unwrap();
    let shutdown = server.shutdown_handle();
    let handle = tokio::spawn(async move {
        server.run(0).await.unwrap();
    });
    (endpoint, handle, shutdown)
}

/// Helper: start session with an explicit working directory.
async fn start_session_in(client: &mut ClientConn, working_dir: &std::path::Path) -> String {
    client
        .send(&Request::SessionStart {
            client_pid: std::process::id(),
            working_dir: working_dir.to_path_buf().into(),
            log_file: None,
            track_stats: false,
            journal_path: None,
            profile: false,
            private_daemon: None,
        })
        .await
        .unwrap();

    match client.recv().await.unwrap() {
        Some(Response::SessionStarted { session_id, .. }) => session_id,
        other => panic!("expected SessionStarted, got: {other:?}"),
    }
}

async fn compile_with_env(
    client: &mut ClientConn,
    session_id: &str,
    compiler: &str,
    args: &[&str],
    cwd: &std::path::Path,
    env: Option<Vec<(String, String)>>,
) -> (i32, bool) {
    let env = env.map(full_env_with_overrides);
    client
        .send(&Request::Compile {
            session_id: session_id.to_string(),
            args: args.iter().map(|s| s.to_string()).collect(),
            cwd: cwd.to_path_buf().into(),
            compiler: NormalizedPath::new(compiler),
            env,
            stdin: Vec::new(),
        })
        .await
        .unwrap();

    match client.recv().await.unwrap() {
        Some(Response::CompileResult {
            exit_code, cached, ..
        }) => (exit_code, cached),
        Some(Response::Error { message }) => panic!("compile error: {message}"),
        other => panic!("unexpected response: {other:?}"),
    }
}

fn full_env_with_overrides(overrides: Vec<(String, String)>) -> Vec<(String, String)> {
    let mut env: std::collections::BTreeMap<String, String> = std::env::vars().collect();
    env.remove("ZCCACHE_WORKTREE_ROOT");
    env.remove("ZCCACHE_PATH_REMAP");
    for (key, value) in overrides {
        env.insert(key, value);
    }
    env.into_iter().collect()
}

fn path_remap_auto_env() -> Vec<(String, String)> {
    vec![("ZCCACHE_PATH_REMAP".to_string(), "auto".to_string())]
}

fn init_git_root(root: &std::path::Path) -> bool {
    std::fs::create_dir_all(root).unwrap();
    match std::process::Command::new("git")
        .arg("init")
        .arg("--quiet")
        .arg(root)
        .status()
    {
        Ok(status) if status.success() => true,
        Ok(status) => {
            eprintln!("skipping test: git init failed with status {status}");
            false
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            eprintln!("skipping test: git not found");
            false
        }
        Err(err) => panic!("failed to run git init: {err}"),
    }
}

fn remove_file_if_exists(path: &std::path::Path) {
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => panic!("failed to remove {}: {err}", path.display()),
    }
}

fn write_path_sensitive_lib(root: &std::path::Path, value: i32) -> std::path::PathBuf {
    let src_dir = root.join("src");
    let target_dir = root.join("target");
    std::fs::create_dir_all(&src_dir).unwrap();
    std::fs::create_dir_all(&target_dir).unwrap();
    let src = src_dir.join("lib.rs");
    std::fs::write(
        &src,
        format!(
            "#[used]\npub static SOURCE_FILE: &str = file!();\npub fn value() -> i32 {{ {value} }}\n"
        ),
    )
    .unwrap();
    src
}

fn bytes_contain_path(bytes: &[u8], path: &std::path::Path) -> bool {
    let haystack = String::from_utf8_lossy(bytes);
    let path = path.to_string_lossy();
    haystack.contains(path.as_ref()) || haystack.contains(&path.replace('\\', "/"))
}

async fn compile_path_sensitive_lib(
    client: &mut ClientConn,
    session_id: &str,
    compiler: &str,
    root: &std::path::Path,
    extra_args: &[String],
    env: Option<Vec<(String, String)>>,
) -> (i32, bool) {
    let src = root.join("src/lib.rs");
    let output = root.join("target/libpathremap.rlib");
    let src = src.to_string_lossy().to_string();
    let output = output.to_string_lossy().to_string();
    let mut args = vec![
        "--edition".to_string(),
        "2021".to_string(),
        "--crate-type".to_string(),
        "lib".to_string(),
        "--crate-name".to_string(),
        "pathremap".to_string(),
        "--emit=link".to_string(),
    ];
    args.extend(extra_args.iter().cloned());
    args.extend([src, "-o".to_string(), output]);
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    compile_with_env(client, session_id, compiler, &arg_refs, root, env).await
}

/// Issue #229: auto-remap should make path-sensitive Rust outputs stable
/// enough to share across sibling Git roots.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore] // integration-level: starts real daemon with IPC + rustc
async fn test_rustc_path_remap_auto_file_macro_hits_across_sibling_git_roots() {
    let rustc = match zccache::test_support::find_rustc() {
        Some(p) => p,
        None => {
            eprintln!("skipping test: rustc not found");
            return;
        }
    };

    zccache::test_support::test_timeout(async move {
        let tmp = tempfile::tempdir().unwrap();
        let root_a = tmp.path().join("worktree-a");
        let root_b = tmp.path().join("worktree-b");

        if !init_git_root(&root_a) || !init_git_root(&root_b) {
            return;
        }

        write_path_sensitive_lib(&root_a, 7);
        write_path_sensitive_lib(&root_b, 7);

        let (endpoint, server_handle, shutdown) = start_daemon().await;
        let mut client_a = zccache::ipc::connect(&endpoint).await.unwrap();
        let mut client_b = zccache::ipc::connect(&endpoint).await.unwrap();
        let session_a = start_session_in(&mut client_a, &root_a).await;
        let session_b = start_session_in(&mut client_b, &root_b).await;

        let rustc_str = rustc.to_string_lossy().to_string();
        let output_a = root_a.join("target/libpathremap.rlib");
        let output_b = root_b.join("target/libpathremap.rlib");

        let (exit_code, cached) = compile_path_sensitive_lib(
            &mut client_a,
            &session_a,
            &rustc_str,
            &root_a,
            &[],
            Some(path_remap_auto_env()),
        )
        .await;
        assert_eq!(exit_code, 0, "A path-sensitive compile should succeed");
        assert!(!cached, "A path-sensitive compile should be a cold miss");
        let bytes_a = std::fs::read(&output_a).unwrap();
        assert!(
            !bytes_contain_path(&bytes_a, &root_a),
            "auto-remap output should not embed A's physical root"
        );

        let (exit_code, cached) = compile_path_sensitive_lib(
            &mut client_b,
            &session_b,
            &rustc_str,
            &root_b,
            &[],
            Some(path_remap_auto_env()),
        )
        .await;
        assert_eq!(exit_code, 0, "B equivalent compile should succeed");
        assert!(
            cached,
            "B equivalent compile should hit A's root-remapped cache entry"
        );
        let bytes_b = std::fs::read(&output_b).unwrap();
        assert_eq!(bytes_a, bytes_b, "restored output should be byte-identical");
        assert!(
            !bytes_contain_path(&bytes_b, &root_a) && !bytes_contain_path(&bytes_b, &root_b),
            "restored output should contain the mapped path, not either physical root"
        );

        shutdown.notify_one();
        server_handle.await.unwrap();
    })
    .await;
}

/// Issue #229: without a root-covering Rust remap, path-sensitive outputs must
/// not share across sibling roots. Different remap destination prefixes must
/// also remain cache-significant.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore] // integration-level: starts real daemon with IPC + rustc
async fn test_rustc_path_remap_conservative_cross_root_misses() {
    let rustc = match zccache::test_support::find_rustc() {
        Some(p) => p,
        None => {
            eprintln!("skipping test: rustc not found");
            return;
        }
    };

    zccache::test_support::test_timeout(async move {
        let tmp = tempfile::tempdir().unwrap();
        let root_a = tmp.path().join("worktree-a");
        let root_b = tmp.path().join("worktree-b");

        if !init_git_root(&root_a) || !init_git_root(&root_b) {
            return;
        }

        write_path_sensitive_lib(&root_a, 7);
        write_path_sensitive_lib(&root_b, 7);

        let (endpoint, server_handle, shutdown) = start_daemon().await;
        let mut client_a = zccache::ipc::connect(&endpoint).await.unwrap();
        let mut client_b = zccache::ipc::connect(&endpoint).await.unwrap();
        let session_a = start_session_in(&mut client_a, &root_a).await;
        let session_b = start_session_in(&mut client_b, &root_b).await;
        let rustc_str = rustc.to_string_lossy().to_string();
        let output_b = root_b.join("target/libpathremap.rlib");

        let (exit_code, cached) =
            compile_path_sensitive_lib(&mut client_a, &session_a, &rustc_str, &root_a, &[], None)
                .await;
        assert_eq!(exit_code, 0, "A no-remap compile should succeed");
        assert!(!cached, "A no-remap compile should be a cold miss");

        let (exit_code, cached) =
            compile_path_sensitive_lib(&mut client_b, &session_b, &rustc_str, &root_b, &[], None)
                .await;
        assert_eq!(exit_code, 0, "B no-remap compile should succeed");
        assert!(
            !cached,
            "B no-remap compile should miss instead of sharing path-sensitive output"
        );
        let no_remap_b = std::fs::read(&output_b).unwrap();
        assert!(
            bytes_contain_path(&no_remap_b, &root_b),
            "no-remap B output should be compiled for B's physical root"
        );

        remove_file_if_exists(&root_a.join("target/libpathremap.rlib"));
        remove_file_if_exists(&output_b);
        let remap_a = vec![format!(
            "--remap-path-prefix={}=/stable-a",
            root_a.display()
        )];
        let remap_b = vec![format!(
            "--remap-path-prefix={}=/stable-b",
            root_b.display()
        )];

        let (exit_code, cached) = compile_path_sensitive_lib(
            &mut client_a,
            &session_a,
            &rustc_str,
            &root_a,
            &remap_a,
            None,
        )
        .await;
        assert_eq!(exit_code, 0, "A manual-remap compile should succeed");
        assert!(!cached, "A manual-remap compile should miss");

        let (exit_code, cached) = compile_path_sensitive_lib(
            &mut client_b,
            &session_b,
            &rustc_str,
            &root_b,
            &remap_b,
            None,
        )
        .await;
        assert_eq!(exit_code, 0, "B manual-remap compile should succeed");
        assert!(
            !cached,
            "different --remap-path-prefix destinations must not share"
        );

        shutdown.notify_one();
        server_handle.await.unwrap();
    })
    .await;
}
