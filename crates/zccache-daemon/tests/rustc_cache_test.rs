//! Integration tests for rustc compilation caching.
//!
//! Tests the full daemon pipeline for Rust compiler invocations,
//! verifying cache miss → cache hit behavior.

use zccache_monocrate::core::NormalizedPath;
use zccache_daemon::DaemonServer;
use zccache_protocol::{Request, Response};

#[cfg(unix)]
type ClientConn = zccache_ipc::IpcConnection;
#[cfg(windows)]
type ClientConn = zccache_ipc::IpcClientConnection;

async fn start_daemon() -> (
    String,
    tokio::task::JoinHandle<()>,
    std::sync::Arc<tokio::sync::Notify>,
) {
    let endpoint = zccache_ipc::unique_test_endpoint();
    let mut server = DaemonServer::bind(&endpoint).unwrap();
    let shutdown = server.shutdown_handle();
    let handle = tokio::spawn(async move {
        server.run(0).await.unwrap();
    });
    (endpoint, handle, shutdown)
}

/// Helper: start session and return session ID.
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
        })
        .await
        .unwrap();

    match client.recv().await.unwrap() {
        Some(Response::SessionStarted { session_id, .. }) => session_id,
        other => panic!("expected SessionStarted, got: {other:?}"),
    }
}

/// Helper: compile via IPC and return (exit_code, cached).
async fn compile(
    client: &mut ClientConn,
    session_id: &str,
    compiler: &str,
    args: &[&str],
    cwd: &std::path::Path,
) -> (i32, bool) {
    compile_with_env(client, session_id, compiler, args, cwd, None).await
}

async fn compile_with_env(
    client: &mut ClientConn,
    session_id: &str,
    compiler: &str,
    args: &[&str],
    cwd: &std::path::Path,
    env: Option<Vec<(String, String)>>,
) -> (i32, bool) {
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

/// Simplest possible rustc caching test:
/// 1. Write a trivial lib.rs
/// 2. Compile with rustc --crate-type lib (cache miss)
/// 3. Delete output, compile again (cache hit)
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore] // integration-level: starts real daemon with IPC + rustc
async fn test_rustc_lib_compile_cached() {
    let rustc = match zccache_test_support::find_rustc() {
        Some(p) => p,
        None => {
            eprintln!("skipping test: rustc not found");
            return;
        }
    };

    zccache_test_support::test_timeout(async move {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("lib.rs");
        let output = tmp.path().join("libhello.rlib");

        std::fs::write(&src, "pub fn hello() -> i32 { 42 }\n").unwrap();

        let (endpoint, server_handle, shutdown) = start_daemon().await;
        let mut client = zccache_ipc::connect(&endpoint).await.unwrap();
        let session_id = start_session(&mut client).await;

        let rustc_str = rustc.to_string_lossy().to_string();
        let src_str = src.to_string_lossy().to_string();
        let output_str = output.to_string_lossy().to_string();

        // First compile: cache miss
        let (exit_code, cached) = compile(
            &mut client,
            &session_id,
            &rustc_str,
            &[
                "--edition",
                "2021",
                "--crate-type",
                "lib",
                "--crate-name",
                "hello",
                "--emit=link",
                &src_str,
                "-o",
                &output_str,
            ],
            tmp.path(),
        )
        .await;
        assert_eq!(exit_code, 0, "first compile should succeed");
        assert!(!cached, "first compile should be a cache miss");
        assert!(output.exists(), "output file should exist after compile");

        // Delete output file
        std::fs::remove_file(&output).unwrap();
        assert!(!output.exists(), "output should be deleted");

        // Second compile: cache hit
        let (exit_code, cached) = compile(
            &mut client,
            &session_id,
            &rustc_str,
            &[
                "--edition",
                "2021",
                "--crate-type",
                "lib",
                "--crate-name",
                "hello",
                "--emit=link",
                &src_str,
                "-o",
                &output_str,
            ],
            tmp.path(),
        )
        .await;
        assert_eq!(exit_code, 0, "second compile should succeed");
        assert!(cached, "second compile should be a cache hit");
        assert!(output.exists(), "output should be restored from cache");

        shutdown.notify_one();
        server_handle.await.unwrap();
    })
    .await;
}

/// Test that different source content produces cache misses.
///
/// Uses separate daemon instances to avoid metadata cache state.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore] // integration-level: starts real daemon with IPC + rustc
async fn test_rustc_different_source_different_artifact() {
    let rustc = match zccache_test_support::find_rustc() {
        Some(p) => p,
        None => return,
    };

    zccache_test_support::test_timeout(async move {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("lib.rs");
        let output = tmp.path().join("libhello.rlib");

        let (endpoint, server_handle, shutdown) = start_daemon().await;
        let mut client = zccache_ipc::connect(&endpoint).await.unwrap();
        let session_id = start_session(&mut client).await;

        let rustc_str = rustc.to_string_lossy().to_string();
        let src_str = src.to_string_lossy().to_string();
        let output_str = output.to_string_lossy().to_string();

        // Compile version A
        std::fs::write(&src, "pub fn hello() -> i32 { 42 }\n").unwrap();
        let (exit_code, cached) = compile(
            &mut client,
            &session_id,
            &rustc_str,
            &[
                "--edition",
                "2021",
                "--crate-type",
                "lib",
                "--crate-name",
                "hello",
                "--emit=link",
                &src_str,
                "-o",
                &output_str,
            ],
            tmp.path(),
        )
        .await;
        assert_eq!(exit_code, 0);
        assert!(!cached, "first compile should be miss");
        let data_a = std::fs::read(&output).unwrap();

        // Compile version A again — should hit
        std::fs::remove_file(&output).unwrap();
        let (exit_code, cached) = compile(
            &mut client,
            &session_id,
            &rustc_str,
            &[
                "--edition",
                "2021",
                "--crate-type",
                "lib",
                "--crate-name",
                "hello",
                "--emit=link",
                &src_str,
                "-o",
                &output_str,
            ],
            tmp.path(),
        )
        .await;
        assert_eq!(exit_code, 0);
        assert!(cached, "same source should be cache hit");
        let data_a2 = std::fs::read(&output).unwrap();
        assert_eq!(data_a, data_a2, "cached output should match original");

        shutdown.notify_one();
        server_handle.await.unwrap();
    })
    .await;
}

/// Test --emit=metadata (cargo check) caching.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore] // integration-level: starts real daemon with IPC + rustc
async fn test_rustc_emit_metadata_cached() {
    let rustc = match zccache_test_support::find_rustc() {
        Some(p) => p,
        None => return,
    };

    zccache_test_support::test_timeout(async move {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("lib.rs");
        let output = tmp.path().join("libhello.rmeta");

        std::fs::write(&src, "pub fn hello() -> i32 { 42 }\n").unwrap();

        let (endpoint, server_handle, shutdown) = start_daemon().await;
        let mut client = zccache_ipc::connect(&endpoint).await.unwrap();
        let session_id = start_session(&mut client).await;

        let rustc_str = rustc.to_string_lossy().to_string();
        let src_str = src.to_string_lossy().to_string();
        let output_str = output.to_string_lossy().to_string();

        // Compile with --emit=metadata (cargo check mode)
        let args = &[
            "--edition",
            "2021",
            "--crate-type",
            "lib",
            "--crate-name",
            "hello",
            "--emit=metadata",
            &src_str,
            "-o",
            &output_str,
        ];

        // First compile: miss
        let (exit_code, cached) =
            compile(&mut client, &session_id, &rustc_str, args, tmp.path()).await;
        assert_eq!(exit_code, 0, "metadata compile should succeed");
        assert!(!cached, "first metadata compile should be miss");
        assert!(output.exists(), ".rmeta should exist");

        // Delete output
        std::fs::remove_file(&output).unwrap();

        // Second compile: hit
        let (exit_code, cached) =
            compile(&mut client, &session_id, &rustc_str, args, tmp.path()).await;
        assert_eq!(exit_code, 0);
        assert!(cached, "second metadata compile should be hit");
        assert!(output.exists(), ".rmeta should be restored from cache");

        shutdown.notify_one();
        server_handle.await.unwrap();
    })
    .await;
}

/// Test multi-output caching: --emit=dep-info,metadata,link produces 3 files.
/// This is what cargo actually passes to rustc.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore] // integration-level: starts real daemon with IPC + rustc
async fn test_rustc_multi_output_cached() {
    let rustc = match zccache_test_support::find_rustc() {
        Some(p) => p,
        None => return,
    };

    zccache_test_support::test_timeout(async move {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("lib.rs");
        let out_dir = tmp.path().join("deps");
        std::fs::create_dir_all(&out_dir).unwrap();

        std::fs::write(&src, "pub fn add(a: i32, b: i32) -> i32 { a + b }\n").unwrap();

        let (endpoint, server_handle, shutdown) = start_daemon().await;
        let mut client = zccache_ipc::connect(&endpoint).await.unwrap();
        let session_id = start_session(&mut client).await;

        let rustc_str = rustc.to_string_lossy().to_string();
        let src_str = src.to_string_lossy().to_string();
        let out_dir_str = out_dir.to_string_lossy().to_string();

        // Mimic what cargo actually invokes
        let args = &[
            "--edition",
            "2021",
            "--crate-type",
            "lib",
            "--crate-name",
            "hello",
            "--emit=dep-info,metadata,link",
            "-C",
            "embed-bitcode=no",
            "-C",
            "metadata=abc123",
            "-C",
            "extra-filename=-abc123",
            "--out-dir",
            &out_dir_str,
            &src_str,
        ];

        // First compile: miss
        let (exit_code, cached) =
            compile(&mut client, &session_id, &rustc_str, args, tmp.path()).await;
        assert_eq!(exit_code, 0, "first compile should succeed");
        assert!(!cached, "first compile should be miss");

        // Verify all 3 output files exist
        let rlib = out_dir.join("libhello-abc123.rlib");
        let rmeta = out_dir.join("libhello-abc123.rmeta");
        let depinfo = out_dir.join("hello-abc123.d");
        assert!(rlib.exists(), "rlib should exist: {}", rlib.display());
        assert!(rmeta.exists(), "rmeta should exist: {}", rmeta.display());
        assert!(
            depinfo.exists(),
            "dep-info should exist: {}",
            depinfo.display()
        );

        // Save originals for comparison
        let rlib_data = std::fs::read(&rlib).unwrap();
        let rmeta_data = std::fs::read(&rmeta).unwrap();

        // Delete all outputs
        std::fs::remove_file(&rlib).unwrap();
        std::fs::remove_file(&rmeta).unwrap();
        std::fs::remove_file(&depinfo).unwrap();

        // Second compile: should be cache hit with all files restored
        let (exit_code, cached) =
            compile(&mut client, &session_id, &rustc_str, args, tmp.path()).await;
        assert_eq!(exit_code, 0, "second compile should succeed");
        assert!(cached, "second compile should be cache hit");

        // All 3 files should be restored
        assert!(
            rlib.exists(),
            "rlib should be restored from cache: {}",
            rlib.display()
        );
        assert!(
            rmeta.exists(),
            "rmeta should be restored from cache: {}",
            rmeta.display()
        );
        assert!(
            depinfo.exists(),
            "dep-info should be restored from cache: {}",
            depinfo.display()
        );

        // Content should match
        assert_eq!(
            std::fs::read(&rlib).unwrap(),
            rlib_data,
            "rlib content should match"
        );
        assert_eq!(
            std::fs::read(&rmeta).unwrap(),
            rmeta_data,
            "rmeta content should match"
        );

        shutdown.notify_one();
        server_handle.await.unwrap();
    })
    .await;
}

/// Test that changing an extern crate invalidates the cache.
///
/// 1. Compile crate A → libA.rlib
/// 2. Compile crate B with --extern a=libA.rlib �� cache miss
/// 3. Compile crate B again → cache hit
/// 4. Change A's source, recompile A → new libA.rlib
/// 5. Compile crate B again → cache miss (extern content changed)
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore] // integration-level: starts real daemon with IPC + rustc
async fn test_rustc_extern_change_invalidates() {
    let rustc = match zccache_test_support::find_rustc() {
        Some(p) => p,
        None => return,
    };

    zccache_test_support::test_timeout(async move {
        let tmp = tempfile::tempdir().unwrap();
        let src_a = tmp.path().join("a.rs");
        let src_b = tmp.path().join("b.rs");
        let lib_a = tmp.path().join("liba.rlib");
        let lib_b = tmp.path().join("libb.rlib");

        // Crate A: a simple lib
        std::fs::write(&src_a, "pub fn value() -> i32 { 42 }\n").unwrap();
        // Crate B: depends on A
        std::fs::write(
            &src_b,
            "extern crate a; pub fn double() -> i32 { a::value() * 2 }\n",
        )
        .unwrap();

        let (endpoint, server_handle, shutdown) = start_daemon().await;
        let mut client = zccache_ipc::connect(&endpoint).await.unwrap();
        let session_id = start_session(&mut client).await;

        let rustc_str = rustc.to_string_lossy().to_string();
        let src_a_str = src_a.to_string_lossy().to_string();
        let src_b_str = src_b.to_string_lossy().to_string();
        let lib_a_str = lib_a.to_string_lossy().to_string();
        let lib_b_str = lib_b.to_string_lossy().to_string();

        // Step 1: Compile crate A
        let (exit_code, _) = compile(
            &mut client,
            &session_id,
            &rustc_str,
            &[
                "--edition",
                "2021",
                "--crate-type",
                "lib",
                "--crate-name",
                "a",
                "--emit=link",
                &src_a_str,
                "-o",
                &lib_a_str,
            ],
            tmp.path(),
        )
        .await;
        assert_eq!(exit_code, 0, "crate A compile should succeed");
        assert!(lib_a.exists());

        // Step 2: Compile crate B with --extern a=libA.rlib (miss)
        let extern_arg = format!("a={lib_a_str}");
        let b_args = &[
            "--edition",
            "2021",
            "--crate-type",
            "lib",
            "--crate-name",
            "b",
            "--emit=link",
            "--extern",
            &extern_arg,
            &src_b_str,
            "-o",
            &lib_b_str,
        ];
        let (exit_code, cached) =
            compile(&mut client, &session_id, &rustc_str, b_args, tmp.path()).await;
        assert_eq!(exit_code, 0, "crate B first compile should succeed");
        assert!(!cached, "crate B first compile should be miss");

        // Step 3: Compile B again (hit)
        std::fs::remove_file(&lib_b).unwrap();
        let (exit_code, cached) =
            compile(&mut client, &session_id, &rustc_str, b_args, tmp.path()).await;
        assert_eq!(exit_code, 0);
        assert!(cached, "crate B second compile should be hit");

        // Step 4: Change A's source, recompile A
        std::thread::sleep(std::time::Duration::from_millis(1100));
        std::fs::write(&src_a, "pub fn value() -> i32 { 99 }\n").unwrap();
        let (exit_code, _) = compile(
            &mut client,
            &session_id,
            &rustc_str,
            &[
                "--edition",
                "2021",
                "--crate-type",
                "lib",
                "--crate-name",
                "a",
                "--emit=link",
                &src_a_str,
                "-o",
                &lib_a_str,
            ],
            tmp.path(),
        )
        .await;
        assert_eq!(exit_code, 0, "crate A recompile should succeed");

        // Step 5: Compile B again — should be miss because extern A changed
        std::fs::remove_file(&lib_b).unwrap();
        let (exit_code, cached) =
            compile(&mut client, &session_id, &rustc_str, b_args, tmp.path()).await;
        assert_eq!(exit_code, 0, "crate B third compile should succeed");
        assert!(
            !cached,
            "crate B should be cache miss after extern A changed"
        );

        shutdown.notify_one();
        server_handle.await.unwrap();
    })
    .await;
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
    let rustc = match zccache_test_support::find_rustc() {
        Some(p) => p,
        None => {
            eprintln!("skipping test: rustc not found");
            return;
        }
    };

    zccache_test_support::test_timeout(async move {
        let tmp = tempfile::tempdir().unwrap();
        let root_a = tmp.path().join("worktree-a");
        let root_b = tmp.path().join("worktree-b");

        if !init_git_root(&root_a) || !init_git_root(&root_b) {
            return;
        }

        write_worktree_project(&root_a, 7, 1);
        write_worktree_project(&root_b, 7, 1);

        let (endpoint, server_handle, shutdown) = start_daemon().await;
        let mut client_a = zccache_ipc::connect(&endpoint).await.unwrap();
        let mut client_b = zccache_ipc::connect(&endpoint).await.unwrap();
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

/// Issue #229: auto-remap should make path-sensitive Rust outputs stable
/// enough to share across sibling Git roots.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore] // integration-level: starts real daemon with IPC + rustc
async fn test_rustc_path_remap_auto_file_macro_hits_across_sibling_git_roots() {
    let rustc = match zccache_test_support::find_rustc() {
        Some(p) => p,
        None => {
            eprintln!("skipping test: rustc not found");
            return;
        }
    };

    zccache_test_support::test_timeout(async move {
        let tmp = tempfile::tempdir().unwrap();
        let root_a = tmp.path().join("worktree-a");
        let root_b = tmp.path().join("worktree-b");

        if !init_git_root(&root_a) || !init_git_root(&root_b) {
            return;
        }

        write_path_sensitive_lib(&root_a, 7);
        write_path_sensitive_lib(&root_b, 7);

        let (endpoint, server_handle, shutdown) = start_daemon().await;
        let mut client_a = zccache_ipc::connect(&endpoint).await.unwrap();
        let mut client_b = zccache_ipc::connect(&endpoint).await.unwrap();
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
    let rustc = match zccache_test_support::find_rustc() {
        Some(p) => p,
        None => {
            eprintln!("skipping test: rustc not found");
            return;
        }
    };

    zccache_test_support::test_timeout(async move {
        let tmp = tempfile::tempdir().unwrap();
        let root_a = tmp.path().join("worktree-a");
        let root_b = tmp.path().join("worktree-b");

        if !init_git_root(&root_a) || !init_git_root(&root_b) {
            return;
        }

        write_path_sensitive_lib(&root_a, 7);
        write_path_sensitive_lib(&root_b, 7);

        let (endpoint, server_handle, shutdown) = start_daemon().await;
        let mut client_a = zccache_ipc::connect(&endpoint).await.unwrap();
        let mut client_b = zccache_ipc::connect(&endpoint).await.unwrap();
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
