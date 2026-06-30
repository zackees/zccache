//! Integration tests for rustc caching across sibling Git worktrees.
//!
//! Covers issue #215 (automatic worktree-root detection sharing cache hits
//! across sibling roots) and issue #396 (explicit `ZCCACHE_WORKTREE_ROOT`
//! sharing across worktrees with different `CARGO_TARGET_DIR` shapes).
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

fn path_remap_auto_env_for_root(
    root: &std::path::Path,
    cargo_target_dir: &std::path::Path,
) -> Vec<(String, String)> {
    vec![
        ("ZCCACHE_PATH_REMAP".to_string(), "auto".to_string()),
        (
            "ZCCACHE_WORKTREE_ROOT".to_string(),
            root.to_string_lossy().into_owned(),
        ),
        (
            "CARGO_TARGET_DIR".to_string(),
            cargo_target_dir.to_string_lossy().into_owned(),
        ),
    ]
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

fn run_git(root: &std::path::Path, args: &[&str]) -> bool {
    match std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .status()
    {
        Ok(status) if status.success() => true,
        Ok(status) => {
            eprintln!(
                "skipping test: git -C {} {} failed with status {status}",
                root.display(),
                args.join(" ")
            );
            false
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            eprintln!("skipping test: git not found");
            false
        }
        Err(err) => panic!("failed to run git: {err}"),
    }
}

fn init_git_worktree_pair(
    root_a: &std::path::Path,
    root_b: &std::path::Path,
) -> Option<(std::path::PathBuf, std::path::PathBuf)> {
    if !init_git_root(root_a) {
        return None;
    }
    let (dep_src, app_src) = write_worktree_project(root_a, 7, 1);
    let root_b_arg = root_b.to_string_lossy().into_owned();
    if !run_git(root_a, &["config", "user.email", "zccache@example.invalid"])
        || !run_git(root_a, &["config", "user.name", "zccache test"])
        || !run_git(root_a, &["add", "src/dep.rs", "src/lib.rs"])
        || !run_git(root_a, &["commit", "--quiet", "-m", "initial test project"])
        || !run_git(root_a, &["worktree", "add", "--quiet", &root_b_arg, "HEAD"])
    {
        return None;
    }
    Some((dep_src, app_src))
}

fn write_worktree_project(
    root: &std::path::Path,
    dep_value: i32,
    app_increment: i32,
) -> (std::path::PathBuf, std::path::PathBuf) {
    let src_dir = root.join("src");
    let target_dir = root.join("target");
    std::fs::create_dir_all(&src_dir).unwrap();
    std::fs::create_dir_all(&target_dir).unwrap();

    let dep_src = src_dir.join("dep.rs");
    let app_src = src_dir.join("lib.rs");
    std::fs::write(
        &dep_src,
        format!("pub fn value() -> i32 {{ {dep_value} }}\n"),
    )
    .unwrap();
    std::fs::write(
        &app_src,
        format!("extern crate dep;\npub fn answer() -> i32 {{ dep::value() + {app_increment} }}\n"),
    )
    .unwrap();
    (dep_src, app_src)
}

fn remove_file_if_exists(path: &std::path::Path) {
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => panic!("failed to remove {}: {err}", path.display()),
    }
}

async fn compile_worktree_dep_with_env(
    client: &mut ClientConn,
    session_id: &str,
    compiler: &str,
    root: &std::path::Path,
    env: Option<Vec<(String, String)>>,
) -> (i32, bool) {
    compile_with_env(
        client,
        session_id,
        compiler,
        &[
            "--edition",
            "2021",
            "--crate-type",
            "lib",
            "--crate-name",
            "dep",
            "--emit=link",
            "src/dep.rs",
            "-o",
            "target/libdep.rlib",
        ],
        root,
        env,
    )
    .await
}

async fn compile_worktree_app_with_env(
    client: &mut ClientConn,
    session_id: &str,
    compiler: &str,
    root: &std::path::Path,
    env: Option<Vec<(String, String)>>,
) -> (i32, bool) {
    compile_with_env(
        client,
        session_id,
        compiler,
        &[
            "--edition",
            "2021",
            "--crate-type",
            "lib",
            "--crate-name",
            "app",
            "--emit=link",
            "--extern",
            "dep=target/libdep.rlib",
            "src/lib.rs",
            "-o",
            "target/libapp.rlib",
        ],
        root,
        env,
    )
    .await
}

async fn compile_worktree_dep_to_target_with_env(
    client: &mut ClientConn,
    session_id: &str,
    compiler: &str,
    root: &std::path::Path,
    target_dir: &std::path::Path,
    env: Option<Vec<(String, String)>>,
) -> (i32, bool) {
    std::fs::create_dir_all(target_dir).unwrap();
    let target_dir_arg = target_dir.to_string_lossy().to_string();
    compile_with_env(
        client,
        session_id,
        compiler,
        &[
            "--edition",
            "2021",
            "--crate-type",
            "lib",
            "--crate-name",
            "dep",
            "--emit=link",
            "--out-dir",
            &target_dir_arg,
            "src/dep.rs",
        ],
        root,
        env,
    )
    .await
}

async fn compile_worktree_app_to_target_with_env(
    client: &mut ClientConn,
    session_id: &str,
    compiler: &str,
    root: &std::path::Path,
    target_dir: &std::path::Path,
    env: Option<Vec<(String, String)>>,
) -> (i32, bool) {
    std::fs::create_dir_all(target_dir).unwrap();
    let target_dir_arg = target_dir.to_string_lossy().to_string();
    let dep_arg = format!("dep={}", target_dir.join("libdep.rlib").display());
    compile_with_env(
        client,
        session_id,
        compiler,
        &[
            "--edition",
            "2021",
            "--crate-type",
            "lib",
            "--crate-name",
            "app",
            "--emit=link",
            "--out-dir",
            &target_dir_arg,
            "--extern",
            &dep_arg,
            "src/lib.rs",
        ],
        root,
        env,
    )
    .await
}

/// Acceptance test for issue #215.
///
/// The daemon should auto-detect each sibling Git root as the worktree
/// normalization root, share equivalent Rust compile artifacts across roots,
/// and fall back to a miss when B's project-local source or dependency content
/// changes.
///
/// This intentionally leaves `ZCCACHE_WORKTREE_ROOT` unset so the automatic Git
/// root path is covered.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore] // integration-level: starts real daemon with IPC + rustc
async fn test_rustc_sibling_git_worktree_equivalent_cache_sharing() {
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

        write_worktree_project(&root_a, 7, 1);
        write_worktree_project(&root_b, 7, 1);

        let (endpoint, server_handle, shutdown) = start_daemon().await;
        let mut client_a = zccache::ipc::connect(&endpoint).await.unwrap();
        let mut client_b = zccache::ipc::connect(&endpoint).await.unwrap();
        let session_a = start_session_in(&mut client_a, &root_a).await;
        let session_b = start_session_in(&mut client_b, &root_b).await;

        let rustc_str = rustc.to_string_lossy().to_string();
        let dep_a = root_a.join("target/libdep.rlib");
        let dep_b = root_b.join("target/libdep.rlib");
        let app_a = root_a.join("target/libapp.rlib");
        let app_b = root_b.join("target/libapp.rlib");

        let (exit_code, cached) = compile_worktree_dep_with_env(
            &mut client_a,
            &session_a,
            &rustc_str,
            &root_a,
            Some(path_remap_auto_env()),
        )
        .await;
        assert_eq!(exit_code, 0, "A dependency compile should succeed");
        assert!(!cached, "A dependency compile should be a cold miss");
        assert!(dep_a.exists(), "A dependency output should exist");

        let (exit_code, cached) = compile_worktree_app_with_env(
            &mut client_a,
            &session_a,
            &rustc_str,
            &root_a,
            Some(path_remap_auto_env()),
        )
        .await;
        assert_eq!(exit_code, 0, "A app compile should succeed");
        assert!(!cached, "A app compile should be a cold miss");
        let app_a_original = std::fs::read(&app_a).unwrap();

        let (exit_code, cached) = compile_worktree_dep_with_env(
            &mut client_b,
            &session_b,
            &rustc_str,
            &root_b,
            Some(path_remap_auto_env()),
        )
        .await;
        assert_eq!(
            exit_code, 0,
            "B equivalent dependency compile should succeed"
        );
        assert!(
            cached,
            "B equivalent dependency compile should hit A's worktree-equivalent entry"
        );
        assert!(dep_b.exists(), "B dependency output should be restored");

        let (exit_code, cached) = compile_worktree_app_with_env(
            &mut client_b,
            &session_b,
            &rustc_str,
            &root_b,
            Some(path_remap_auto_env()),
        )
        .await;
        assert_eq!(exit_code, 0, "B equivalent app compile should succeed");
        assert!(
            cached,
            "B equivalent app compile should hit A's worktree-equivalent entry"
        );
        assert!(app_b.exists(), "B app output should be restored");

        tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
        write_worktree_project(&root_b, 7, 2);
        remove_file_if_exists(&app_b);
        let (exit_code, cached) = compile_worktree_app_with_env(
            &mut client_b,
            &session_b,
            &rustc_str,
            &root_b,
            Some(path_remap_auto_env()),
        )
        .await;
        assert_eq!(
            exit_code, 0,
            "B app compile after source edit should succeed"
        );
        assert!(!cached, "B source edit should force a conservative miss");

        tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
        write_worktree_project(&root_b, 99, 1);
        remove_file_if_exists(&dep_b);
        remove_file_if_exists(&app_b);

        let (exit_code, cached) = compile_worktree_dep_with_env(
            &mut client_b,
            &session_b,
            &rustc_str,
            &root_b,
            Some(path_remap_auto_env()),
        )
        .await;
        assert_eq!(
            exit_code, 0,
            "B dependency compile after dependency edit should succeed"
        );
        assert!(!cached, "B dependency edit should miss");

        let (exit_code, cached) = compile_worktree_app_with_env(
            &mut client_b,
            &session_b,
            &rustc_str,
            &root_b,
            Some(path_remap_auto_env()),
        )
        .await;
        assert_eq!(
            exit_code, 0,
            "B app compile after dependency edit should succeed"
        );
        assert!(
            !cached,
            "B app should miss when its root-relative dependency content changes"
        );

        remove_file_if_exists(&app_a);
        let (exit_code, cached) = compile_worktree_app_with_env(
            &mut client_a,
            &session_a,
            &rustc_str,
            &root_a,
            Some(path_remap_auto_env()),
        )
        .await;
        assert_eq!(exit_code, 0, "A original app compile should still succeed");
        assert!(
            cached,
            "B edits must not poison A's original worktree-equivalent cache entry"
        );
        assert_eq!(
            std::fs::read(&app_a).unwrap(),
            app_a_original,
            "A cached output should remain byte-identical after B misses"
        );

        shutdown.notify_one();
        server_handle.await.unwrap();
    })
    .await;
}

/// Issue #396: a real `git worktree` pair should share rustc artifacts even
/// when each worktree uses a different relative `CARGO_TARGET_DIR` leaf name.
///
/// The explicit `ZCCACHE_WORKTREE_ROOT` entries mirror soldr's managed-build
/// contract. Before filtering `CARGO_TARGET_DIR`, these equivalent compiles
/// missed because the target-dir shape leaked into rustc cache identity.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore] // integration-level: starts real daemon with IPC + rustc + git worktree
async fn test_rustc_git_worktrees_share_with_different_cargo_target_dir_shapes() {
    let rustc = match zccache::test_support::find_rustc() {
        Some(p) => p,
        None => {
            eprintln!("skipping test: rustc not found");
            return;
        }
    };

    zccache::test_support::test_timeout(async move {
        let tmp = tempfile::tempdir().unwrap();
        let root_a = tmp.path().join("repo-main");
        let root_b = tmp.path().join("repo-subagent");

        let Some((_dep_src, _app_src)) = init_git_worktree_pair(&root_a, &root_b) else {
            return;
        };

        let target_a = root_a.join(".claude/worktrees/parent-cache-main-target");
        let target_b = root_b.join(".claude/worktrees/parent-cache-sub-target");

        let (endpoint, server_handle, shutdown) = start_daemon().await;
        let mut client_a = zccache::ipc::connect(&endpoint).await.unwrap();
        let mut client_b = zccache::ipc::connect(&endpoint).await.unwrap();
        let session_a = start_session_in(&mut client_a, &root_a).await;
        let session_b = start_session_in(&mut client_b, &root_b).await;
        let rustc_str = rustc.to_string_lossy().to_string();

        let env_a = path_remap_auto_env_for_root(&root_a, &target_a);
        let env_b = path_remap_auto_env_for_root(&root_b, &target_b);

        let (exit_code, cached) = compile_worktree_dep_to_target_with_env(
            &mut client_a,
            &session_a,
            &rustc_str,
            &root_a,
            &target_a,
            Some(env_a.clone()),
        )
        .await;
        assert_eq!(exit_code, 0, "A dependency compile should succeed");
        assert!(!cached, "A dependency compile should be a cold miss");

        let (exit_code, cached) = compile_worktree_app_to_target_with_env(
            &mut client_a,
            &session_a,
            &rustc_str,
            &root_a,
            &target_a,
            Some(env_a),
        )
        .await;
        assert_eq!(exit_code, 0, "A app compile should succeed");
        assert!(!cached, "A app compile should be a cold miss");

        let (exit_code, dep_cached) = compile_worktree_dep_to_target_with_env(
            &mut client_b,
            &session_b,
            &rustc_str,
            &root_b,
            &target_b,
            Some(env_b.clone()),
        )
        .await;
        assert_eq!(exit_code, 0, "B dependency compile should succeed");

        let (exit_code, app_cached) = compile_worktree_app_to_target_with_env(
            &mut client_b,
            &session_b,
            &rustc_str,
            &root_b,
            &target_b,
            Some(env_b),
        )
        .await;
        assert_eq!(exit_code, 0, "B app compile should succeed");

        let warm_hits = usize::from(dep_cached) + usize::from(app_cached);
        let warm_cacheable = 2usize;
        assert!(
            warm_hits * 100 >= warm_cacheable * 95,
            "expected >=95% warm hit rate across target-dir shapes, got \
             {warm_hits}/{warm_cacheable}; dep_cached={dep_cached}, \
             app_cached={app_cached}"
        );
        assert!(
            target_b.join("libdep.rlib").exists(),
            "B dependency output should be restored into B's target dir"
        );
        assert!(
            target_b.join("libapp.rlib").exists(),
            "B app output should be restored into B's target dir"
        );

        shutdown.notify_one();
        server_handle.await.unwrap();
    })
    .await;
}
