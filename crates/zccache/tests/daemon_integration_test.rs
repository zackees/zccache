//! End-to-end integration tests for the daemon.
//!
//! Tests the full client → daemon → clang toolchain discovery pipeline.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::panic_in_result_fn,
    clippy::unwrap_in_result
)]

use zccache::daemon::DaemonServer;
use zccache::protocol::{Request, Response};

/// Helper: start a daemon server on a unique endpoint and return the endpoint + shutdown handle.
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

#[tokio::test]
#[ignore] // integration-level: starts real daemon with IPC
async fn test_client_connects_and_pings_daemon() {
    zccache::test_support::test_timeout(async {
        let (endpoint, server_handle, shutdown) = start_daemon().await;

        let mut client = zccache::ipc::connect(&endpoint).await.unwrap();
        client.send(&Request::Ping).await.unwrap();
        let resp: Option<Response> = client.recv().await.unwrap();
        assert_eq!(resp, Some(Response::Pong));

        shutdown.notify_one();
        server_handle.await.unwrap();
    })
    .await;
}

#[tokio::test]
#[ignore] // integration-level: starts real daemon with IPC
async fn test_multiple_clients_concurrent() {
    zccache::test_support::test_timeout(async {
        let (endpoint, server_handle, shutdown) = start_daemon().await;

        let mut handles = Vec::new();
        for _ in 0..5 {
            let ep = endpoint.clone();
            handles.push(tokio::spawn(async move {
                let mut client = zccache::ipc::connect(&ep).await.unwrap();
                client.send(&Request::Ping).await.unwrap();
                let resp: Option<Response> = client.recv().await.unwrap();
                assert_eq!(resp, Some(Response::Pong));
            }));
        }

        for h in handles {
            h.await.unwrap();
        }

        shutdown.notify_one();
        server_handle.await.unwrap();
    })
    .await;
}

#[tokio::test]
#[ignore] // integration-level: starts real daemon with IPC
async fn test_session_start_with_nonexistent_compiler() {
    zccache::test_support::test_timeout(async {
        let (endpoint, server_handle, shutdown) = start_daemon().await;

        let mut client = zccache::ipc::connect(&endpoint).await.unwrap();
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

        let session_id = match client.recv().await.unwrap() {
            Some(Response::SessionStarted { session_id, .. }) => session_id,
            other => panic!("expected SessionStarted, got: {other:?}"),
        };

        // Now try to compile with a nonexistent compiler
        client
            .send(&Request::Compile {
                session_id: session_id.clone(),
                args: vec!["-c".to_string(), "dummy.cpp".to_string()],
                cwd: std::env::current_dir().unwrap().into(),
                compiler: "/nonexistent/compiler".to_string().into(),
                env: None,
                stdin: Vec::new(),
            })
            .await
            .unwrap();

        let resp: Option<Response> = client.recv().await.unwrap();
        match resp {
            Some(Response::Error { message }) => {
                assert!(
                    message.contains("not found") || message.contains("failed to run compiler"),
                    "expected compiler error in: {message}"
                );
            }
            other => panic!("expected Error response, got: {other:?}"),
        }

        shutdown.notify_one();
        server_handle.await.unwrap();
    })
    .await;
}

/// The main TDD target: client connects to daemon, starts a session,
/// and receives a UUID session ID.
#[tokio::test]
#[ignore] // integration-level: starts real daemon with IPC + compiler
async fn test_session_start_with_clang_toolchain() {
    if zccache::test_support::find_clang().is_none() {
        eprintln!("skipping test: clang not found");
        return;
    }

    zccache::test_support::test_timeout(async move {
        let (endpoint, server_handle, shutdown) = start_daemon().await;

        let mut client = zccache::ipc::connect(&endpoint).await.unwrap();
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

        let resp: Option<Response> = client.recv().await.unwrap();
        let session_id = match resp {
            Some(Response::SessionStarted { session_id, .. }) => {
                // Session ID should be a valid UUID string
                eprintln!("session_id: {session_id}");
                assert!(
                    !session_id.is_empty(),
                    "session ID should be a non-empty UUID string"
                );
                session_id
            }
            other => panic!("expected SessionStarted, got: {other:?}"),
        };

        // Second session should get a different ID
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

        let resp2: Option<Response> = client.recv().await.unwrap();
        match resp2 {
            Some(Response::SessionStarted {
                session_id: id2, ..
            }) => {
                assert_ne!(id2, session_id, "second session should have a different ID");
            }
            other => panic!("expected SessionStarted for second session, got: {other:?}"),
        }

        shutdown.notify_one();
        server_handle.await.unwrap();
    })
    .await;
}

/// Test the full flow: ping → session start → status → shutdown.
#[tokio::test]
#[ignore] // integration-level: starts real daemon with IPC + compiler
async fn test_full_client_flow() {
    if zccache::test_support::find_clang().is_none() {
        eprintln!("skipping test: clang not found");
        return;
    }

    zccache::test_support::test_timeout(async move {
        let (endpoint, server_handle, shutdown) = start_daemon().await;

        let mut client = zccache::ipc::connect(&endpoint).await.unwrap();

        // 1. Ping
        client.send(&Request::Ping).await.unwrap();
        let resp: Option<Response> = client.recv().await.unwrap();
        assert_eq!(resp, Some(Response::Pong));

        // 2. Session start
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
        let resp: Option<Response> = client.recv().await.unwrap();
        assert!(matches!(resp, Some(Response::SessionStarted { .. })));

        // 3. Status
        client.send(&Request::Status).await.unwrap();
        let resp: Option<Response> = client.recv().await.unwrap();
        assert!(matches!(resp, Some(Response::Status(_))));

        // 4. Shutdown
        client.send(&Request::Shutdown).await.unwrap();
        let resp: Option<Response> = client.recv().await.unwrap();
        assert_eq!(resp, Some(Response::ShuttingDown));

        shutdown.notify_one();
        server_handle.await.unwrap();
    })
    .await;
}

/// TDD: Compile hello.cpp through the daemon, verify caching works.
///
/// 1. Create a temp dir with hello.cpp
/// 2. Start daemon, start session with clang + log_file
/// 3. Compile hello.cpp → expect success (cache miss)
/// 4. Remove .o, compile hello.cpp again → expect success (cache hit)
/// 5. Read log_file → verify cache miss then cache hit entries
#[tokio::test]
#[ignore] // integration-level: starts real daemon with IPC + compiler
async fn test_compile_hello_cpp_cached() {
    let clang_path = match zccache::test_support::find_clang() {
        Some(p) => p,
        None => {
            eprintln!("skipping test: clang not found");
            return;
        }
    };

    zccache::test_support::test_timeout(async move {
        // Create temp dir with hello.cpp
        let tmp = tempfile::tempdir().unwrap();
        let hello_cpp = tmp.path().join("hello.cpp");
        std::fs::write(
            &hello_cpp,
            r#"#include <stdio.h>
int main() {
    printf("hello world\n");
    return 0;
}
"#,
        )
        .unwrap();

        let log_file = tmp.path().join("session.log");
        let output_obj = tmp.path().join("hello.o");
        let depfile = tmp.path().join("hello.d");

        let (endpoint, server_handle, shutdown) = start_daemon().await;
        let mut client = zccache::ipc::connect(&endpoint).await.unwrap();

        // Start session with log file
        client
            .send(&Request::SessionStart {
                client_pid: std::process::id(),
                working_dir: tmp.path().to_string_lossy().into_owned().into(),
                log_file: Some(log_file.to_string_lossy().into_owned().into()),
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

        let compiler_str = clang_path.to_string_lossy().into_owned();

        // First compile — should be a cache miss
        client
            .send(&Request::Compile {
                session_id: session_id.clone(),
                args: vec![
                    "-c".to_string(),
                    hello_cpp.to_string_lossy().into_owned(),
                    "-o".to_string(),
                    output_obj.to_string_lossy().into_owned(),
                    "-MD".to_string(),
                    "-MF".to_string(),
                    depfile.to_string_lossy().into_owned(),
                ],
                cwd: tmp.path().to_string_lossy().into_owned().into(),
                compiler: compiler_str.clone().into(),
                env: None,
                stdin: Vec::new(),
            })
            .await
            .unwrap();

        let resp: Option<Response> = client.recv().await.unwrap();
        match resp {
            Some(Response::CompileResult {
                exit_code, cached, ..
            }) => {
                assert_eq!(exit_code, 0, "first compile should succeed");
                assert!(!cached, "first compile should be a cache miss");
            }
            other => panic!("expected CompileResult, got: {other:?}"),
        }

        // Verify .o was produced
        assert!(output_obj.exists(), "output object file should exist");
        assert!(depfile.exists(), "user-requested depfile should exist");
        let first_obj_size = std::fs::metadata(&output_obj).unwrap().len();
        assert!(first_obj_size > 0, "object file should not be empty");

        // Remove .o so we can verify cache restores it
        std::fs::remove_file(&output_obj).unwrap();
        std::fs::remove_file(&depfile).unwrap();
        assert!(!output_obj.exists(), ".o should be deleted");

        // Second compile — should be a cache hit
        client
            .send(&Request::Compile {
                session_id: session_id.clone(),
                args: vec![
                    "-c".to_string(),
                    hello_cpp.to_string_lossy().into_owned(),
                    "-o".to_string(),
                    output_obj.to_string_lossy().into_owned(),
                    "-MD".to_string(),
                    "-MF".to_string(),
                    depfile.to_string_lossy().into_owned(),
                ],
                cwd: tmp.path().to_string_lossy().into_owned().into(),
                compiler: compiler_str.into(),
                env: None,
                stdin: Vec::new(),
            })
            .await
            .unwrap();

        let resp: Option<Response> = client.recv().await.unwrap();
        match resp {
            Some(Response::CompileResult {
                exit_code, cached, ..
            }) => {
                assert_eq!(exit_code, 0, "second compile should succeed");
                assert!(cached, "second compile should be a cache hit");
            }
            other => panic!("expected CompileResult, got: {other:?}"),
        }

        // Verify .o was restored from cache
        assert!(
            output_obj.exists(),
            "cache hit should restore the object file"
        );
        assert!(depfile.exists(), "cache hit should restore the depfile");
        let second_obj_size = std::fs::metadata(&output_obj).unwrap().len();
        assert_eq!(
            first_obj_size, second_obj_size,
            "cached .o should be same size"
        );

        // Read the log file and verify cache miss + hit entries
        let log_contents = std::fs::read_to_string(&log_file).unwrap();
        eprintln!("=== session log ===\n{log_contents}");

        assert!(
            log_contents.contains("[MISS]"),
            "log should contain '[MISS]' for first compile"
        );
        assert!(
            log_contents.contains("[HIT]"),
            "log should contain '[HIT]' for second compile"
        );

        shutdown.notify_one();
        server_handle.await.unwrap();
    })
    .await;
}
