//! Adversarial stress tests for the compilation cache — compiler override.
//!
//! Verifies the per-request `compiler:` field overrides the session compiler
//! (e.g. dispatch a C source through `clang` while the session was started
//! with `clang++`).
//!
//! See `daemon_stress_correctness_test.rs` for correctness + concurrency tests
//! and `daemon_stress_edges_test.rs` for path/output/empty-source edge cases.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::panic_in_result_fn,
    clippy::unwrap_in_result
)]

use zccache::daemon::DaemonServer;
use zccache::protocol::{Request, Response};

/// Platform-correct client connection type.
#[cfg(unix)]
type ClientConn = zccache::ipc::IpcConnection;
#[cfg(windows)]
type ClientConn = zccache::ipc::IpcClientConnection;

/// Helper: start a daemon server on a unique endpoint.
async fn start_daemon() -> (
    String,
    tokio::task::JoinHandle<()>,
    std::sync::Arc<tokio::sync::Notify>,
) {
    let endpoint = zccache::ipc::unique_test_endpoint();
    let mut server = DaemonServer::bind(&endpoint).unwrap();
    let shutdown = server.shutdown_handle();
    let handle = tokio::spawn(async move { server.run(0).await.unwrap() });
    (endpoint, handle, shutdown)
}

/// Helper: start a session. Returns (session_id, compiler_string).
async fn start_session(
    client: &mut ClientConn,
    clang: &std::path::Path,
    cwd: &str,
    log_file: &str,
) -> (String, String) {
    let compiler_str = clang.to_string_lossy().into_owned();
    client
        .send(&Request::SessionStart {
            client_pid: std::process::id(),
            working_dir: cwd.to_string().into(),
            log_file: Some(log_file.to_string().into()),
            track_stats: false,
            journal_path: None,
            profile: false,
            private_daemon: None,
        })
        .await
        .unwrap();
    let session_id = match client.recv().await.unwrap() {
        Some(Response::SessionStarted { session_id, .. }) => session_id,
        other => panic!("expected SessionStarted, got: {other:?}"),
    };
    (session_id, compiler_str)
}

// ═══════════════════════════════════════════════════════════════════════
// COMPILER OVERRIDE
// ═══════════════════════════════════════════════════════════════════════

#[tokio::test]
#[ignore] // integration: spawns clang, run with --full
async fn compiler_override_uses_wrapped_compiler() {
    let clangpp = match zccache::test_support::find_clang() {
        Some(p) => p,
        None => return,
    };
    let clang = clangpp
        .parent()
        .unwrap()
        .join(if cfg!(windows) { "clang.exe" } else { "clang" });
    if !clang.exists() {
        eprintln!("SKIP: clang not found at {}", clang.display());
        return;
    }

    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("test.c");
    let obj = tmp.path().join("test.o");
    let log = tmp.path().join("log.txt");
    let cwd = tmp.path().to_string_lossy().into_owned();
    std::fs::write(&src, "struct Point { int x; int y; };\nint main(void) {\n\tstruct Point p = { .x = 1, .y = 2 };\n\treturn p.x + p.y - 3;\n}\n").unwrap();

    let (endpoint, server_handle, shutdown) = start_daemon().await;
    let mut client = zccache::ipc::connect(&endpoint).await.unwrap();
    let (sid, _clangpp_compiler) =
        start_session(&mut client, &clangpp, &cwd, &log.to_string_lossy()).await;

    client
        .send(&Request::Compile {
            session_id: sid.clone(),
            args: vec![
                "-c".into(),
                "-std=c11".into(),
                src.to_string_lossy().into_owned(),
                "-o".into(),
                obj.to_string_lossy().into_owned(),
            ],
            cwd: cwd.clone().into(),
            compiler: clang.to_string_lossy().into_owned().into(),
            env: None,
            stdin: Vec::new(),
        })
        .await
        .unwrap();

    match client.recv().await.unwrap() {
        Some(Response::CompileResult {
            exit_code,
            cached,
            stderr,
            ..
        }) => {
            let stderr_str = String::from_utf8_lossy(&stderr);
            assert_eq!(
                exit_code, 0,
                "C file with -std=c11 should compile with clang override. stderr: {stderr_str}"
            );
            assert!(!cached, "first compile should be a miss");
            assert!(
                !stderr_str.contains("not valid for C++"),
                "compiler override should use clang, not clang++. stderr: {stderr_str}"
            );
        }
        Some(Response::Error { message }) => {
            panic!("compile error (compiler override not working?): {message}")
        }
        other => panic!("expected CompileResult, got: {other:?}"),
    }
    assert!(obj.exists(), "object file should be produced");
    shutdown.notify_one();
    server_handle.await.unwrap();
}
