//! Integration tests for rustc compilation caching: basic single-crate paths.
//!
//! Covers single-output link caching, source-content differentiation,
//! `--emit=metadata` (cargo check), multi-output `--emit=dep-info,metadata,link`,
//! and extern-crate invalidation. Split out from
//! `daemon_rustc_cache_test.rs` so each integration-test binary stays small.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::panic_in_result_fn,
    clippy::unwrap_in_result
)]

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
            private_daemon: None,
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
    client
        .send(&Request::Compile {
            session_id: session_id.to_string(),
            args: args.iter().map(|s| s.to_string()).collect(),
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
        other => panic!("unexpected response: {other:?}"),
    }
}

/// Simplest possible rustc caching test:
/// 1. Write a trivial lib.rs
/// 2. Compile with rustc --crate-type lib (cache miss)
/// 3. Delete output, compile again (cache hit)
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore] // integration-level: starts real daemon with IPC + rustc
async fn test_rustc_lib_compile_cached() {
    let rustc = match zccache::test_support::find_rustc() {
        Some(p) => p,
        None => {
            eprintln!("skipping test: rustc not found");
            return;
        }
    };

    zccache::test_support::test_timeout(async move {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("lib.rs");
        let output = tmp.path().join("libhello.rlib");

        std::fs::write(&src, "pub fn hello() -> i32 { 42 }\n").unwrap();

        let (endpoint, server_handle, shutdown) = start_daemon().await;
        let mut client = zccache::ipc::connect(&endpoint).await.unwrap();
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
    let rustc = match zccache::test_support::find_rustc() {
        Some(p) => p,
        None => return,
    };

    zccache::test_support::test_timeout(async move {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("lib.rs");
        let output = tmp.path().join("libhello.rlib");

        let (endpoint, server_handle, shutdown) = start_daemon().await;
        let mut client = zccache::ipc::connect(&endpoint).await.unwrap();
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
    let rustc = match zccache::test_support::find_rustc() {
        Some(p) => p,
        None => return,
    };

    zccache::test_support::test_timeout(async move {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("lib.rs");
        let output = tmp.path().join("libhello.rmeta");

        std::fs::write(&src, "pub fn hello() -> i32 { 42 }\n").unwrap();

        let (endpoint, server_handle, shutdown) = start_daemon().await;
        let mut client = zccache::ipc::connect(&endpoint).await.unwrap();
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
    let rustc = match zccache::test_support::find_rustc() {
        Some(p) => p,
        None => return,
    };

    zccache::test_support::test_timeout(async move {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("lib.rs");
        let out_dir = tmp.path().join("deps");
        std::fs::create_dir_all(&out_dir).unwrap();

        std::fs::write(&src, "pub fn add(a: i32, b: i32) -> i32 { a + b }\n").unwrap();

        let (endpoint, server_handle, shutdown) = start_daemon().await;
        let mut client = zccache::ipc::connect(&endpoint).await.unwrap();
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

/// Test check-style metadata can reuse a prior build-style metadata+link artifact.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore] // integration-level: starts real daemon with IPC + rustc
async fn test_rustc_check_metadata_hits_build_metadata_link() {
    let rustc = match zccache::test_support::find_rustc() {
        Some(p) => p,
        None => return,
    };

    zccache::test_support::test_timeout(async move {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("lib.rs");
        let out_dir = tmp.path().join("deps");
        std::fs::create_dir_all(&out_dir).unwrap();
        std::fs::write(&src, "pub fn add(a: i32, b: i32) -> i32 { a + b }\n").unwrap();

        let (endpoint, server_handle, shutdown) = start_daemon().await;
        let mut client = zccache::ipc::connect(&endpoint).await.unwrap();
        let session_id = start_session(&mut client).await;

        let rustc_str = rustc.to_string_lossy().to_string();
        let src_str = src.to_string_lossy().to_string();
        let out_dir_str = out_dir.to_string_lossy().to_string();

        let build_args = &[
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
            "metadata=build123",
            "-C",
            "extra-filename=-build123",
            "--out-dir",
            &out_dir_str,
            &src_str,
        ];
        let check_args = &[
            "--edition",
            "2021",
            "--crate-type",
            "lib",
            "--crate-name",
            "hello",
            "--emit=dep-info,metadata",
            "-C",
            "embed-bitcode=no",
            "-C",
            "metadata=check456",
            "-C",
            "extra-filename=-check456",
            "--out-dir",
            &out_dir_str,
            &src_str,
        ];

        let (exit_code, cached) =
            compile(&mut client, &session_id, &rustc_str, build_args, tmp.path()).await;
        assert_eq!(exit_code, 0, "build-style compile should succeed");
        assert!(!cached, "build-style compile should be a miss");

        let build_rlib = out_dir.join("libhello-build123.rlib");
        let build_rmeta = out_dir.join("libhello-build123.rmeta");
        let build_depinfo = out_dir.join("hello-build123.d");
        assert!(build_rlib.exists());
        assert!(build_rmeta.exists());
        assert!(build_depinfo.exists());

        let check_rlib = out_dir.join("libhello-check456.rlib");
        let check_rmeta = out_dir.join("libhello-check456.rmeta");
        let check_depinfo = out_dir.join("hello-check456.d");
        let _ = std::fs::remove_file(&check_rlib);
        let _ = std::fs::remove_file(&check_rmeta);
        let _ = std::fs::remove_file(&check_depinfo);

        let (exit_code, cached) =
            compile(&mut client, &session_id, &rustc_str, check_args, tmp.path()).await;
        assert_eq!(exit_code, 0, "check-style compile should succeed");
        assert!(
            cached,
            "check-style compile should hit build-style artifact"
        );
        assert!(check_rmeta.exists(), "check .rmeta should be materialized");
        assert!(
            check_depinfo.exists(),
            "check dep-info should be materialized"
        );
        assert!(
            !check_rlib.exists(),
            "compatibility hit must not materialize link output for check"
        );

        shutdown.notify_one();
        server_handle.await.unwrap();
    })
    .await;
}

/// Test that changing an extern crate invalidates the cache.
///
/// 1. Compile crate A → libA.rlib
/// 2. Compile crate B with --extern a=libA.rlib → cache miss
/// 3. Compile crate B again → cache hit
/// 4. Change A's source, recompile A → new libA.rlib
/// 5. Compile crate B again → cache miss (extern content changed)
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore] // integration-level: starts real daemon with IPC + rustc
async fn test_rustc_extern_change_invalidates() {
    let rustc = match zccache::test_support::find_rustc() {
        Some(p) => p,
        None => return,
    };

    zccache::test_support::test_timeout(async move {
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
        let mut client = zccache::ipc::connect(&endpoint).await.unwrap();
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
