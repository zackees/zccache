//! Repro test for ISSUE.md: mid-session stats query.
//!
//! Tests that `Request::SessionStats` returns accurate hit/miss counts
//! without ending the session. This enables diagnosing the 11% hit rate
//! bug described in ISSUE.md — by querying stats mid-build, callers can
//! see artifact_not_found misses accumulating in real time.
//!
//! Flow:
//!   1. session-start (track_stats: true)
//!   2. Compile N files (cold misses)
//!   3. Query session-stats → verify all misses
//!   4. Delete .o files, recompile same files (should be hits)
//!   5. Query session-stats → verify hits accumulated
//!   6. session-end → verify final stats match mid-session snapshot

use zccache_daemon::DaemonServer;
use zccache_protocol::{Request, Response, SessionStats};

#[cfg(windows)]
type ClientConn = zccache_ipc::IpcClientConnection;
#[cfg(not(windows))]
type ClientConn = zccache_ipc::IpcConnection;

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

/// Helper: send a SessionStats request and extract the stats.
async fn query_session_stats(client: &mut ClientConn, session_id: &str) -> Option<SessionStats> {
    client
        .send(&Request::SessionStats {
            session_id: session_id.to_string(),
        })
        .await
        .unwrap();

    match client.recv().await.unwrap() {
        Some(Response::SessionStatsResult { stats }) => stats,
        other => panic!("expected SessionStatsResult, got: {other:?}"),
    }
}

/// Helper: compile a file and return (exit_code, cached).
async fn compile(
    client: &mut ClientConn,
    session_id: &str,
    compiler: &std::path::Path,
    src: &std::path::Path,
    obj: &std::path::Path,
    cwd: &str,
) -> (i32, bool) {
    client
        .send(&Request::Compile {
            session_id: session_id.to_string(),
            args: vec![
                "-c".to_string(),
                src.to_string_lossy().into_owned(),
                "-o".to_string(),
                obj.to_string_lossy().into_owned(),
            ],
            cwd: cwd.into(),
            compiler: compiler.to_string_lossy().into_owned().into(),
            env: None,
            stdin: Vec::new(),
        })
        .await
        .unwrap();

    match client.recv().await.unwrap() {
        Some(Response::CompileResult {
            exit_code, cached, ..
        }) => (exit_code, cached),
        other => panic!("expected CompileResult, got: {other:?}"),
    }
}

/// Core repro: compile files, query stats mid-session, recompile, verify hits.
#[tokio::test]
#[ignore] // integration-level: starts real daemon with IPC + compiler
async fn session_stats_mid_session_query() {
    let clang = match zccache_test_support::find_clang() {
        Some(p) => p,
        None => {
            eprintln!("skipping: clang not found");
            return;
        }
    };
    zccache_test_support::test_timeout(async move {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.path().to_string_lossy().into_owned();

        // Create 3 source files
        let sources: Vec<_> = (0..3)
            .map(|i| {
                let src = tmp.path().join(format!("file_{i}.cpp"));
                let obj = tmp.path().join(format!("file_{i}.o"));
                std::fs::write(&src, format!("int func_{i}() {{ return {i}; }}\n")).unwrap();
                (src, obj)
            })
            .collect();

        let (endpoint, server_handle, shutdown) = start_daemon().await;
        let mut client = zccache_ipc::connect(&endpoint).await.unwrap();

        // ── Start session with stats tracking ──
        client
            .send(&Request::SessionStart {
                client_pid: std::process::id(),
                working_dir: cwd.clone().into(),
                log_file: None,
                track_stats: true,
                journal_path: None,
            })
            .await
            .unwrap();

        let session_id = match client.recv().await.unwrap() {
            Some(Response::SessionStarted { session_id, .. }) => session_id,
            other => panic!("expected SessionStarted, got: {other:?}"),
        };

        // ── Phase 1: Cold compiles (all misses) ──
        for (src, obj) in &sources {
            let (exit_code, cached) =
                compile(&mut client, &session_id, &clang, src, obj, &cwd).await;
            assert_eq!(exit_code, 0);
            assert!(!cached, "first compile should be a miss");
        }

        // ── Query stats mid-session: should show 3 misses, 0 hits ──
        let stats = query_session_stats(&mut client, &session_id)
            .await
            .expect("stats tracking was enabled");

        eprintln!("After cold compiles: {stats:?}");
        assert_eq!(stats.compilations, 3, "should have 3 compilations");
        assert_eq!(stats.misses, 3, "all 3 should be misses");
        assert_eq!(stats.hits, 0, "no hits yet");

        // ── Phase 2: Delete .o files and recompile (should be hits) ──
        for (_, obj) in &sources {
            std::fs::remove_file(obj).unwrap();
        }

        for (src, obj) in &sources {
            let (exit_code, cached) =
                compile(&mut client, &session_id, &clang, src, obj, &cwd).await;
            assert_eq!(exit_code, 0);
            assert!(cached, "second compile should be a cache hit");
        }

        // ── Query stats mid-session: should show 3 misses + 3 hits ──
        let stats = query_session_stats(&mut client, &session_id)
            .await
            .expect("stats tracking was enabled");

        eprintln!("After cached compiles: {stats:?}");
        assert_eq!(stats.compilations, 6, "should have 6 total compilations");
        assert_eq!(stats.misses, 3, "still 3 misses from phase 1");
        assert_eq!(stats.hits, 3, "3 hits from phase 2");

        let total = stats.hits + stats.misses;
        let hit_rate = stats.hits as f64 / total as f64 * 100.0;
        assert!(
            hit_rate >= 49.0,
            "hit rate should be 50% (3 hits / 6 cacheable), got {hit_rate:.1}%"
        );

        // ── End session: verify final stats match ──
        client
            .send(&Request::SessionEnd {
                session_id: session_id.clone(),
            })
            .await
            .unwrap();

        match client.recv().await.unwrap() {
            Some(Response::SessionEnded { stats }) => {
                let final_stats = stats.expect("stats tracking was enabled");
                assert_eq!(final_stats.compilations, 6);
                assert_eq!(final_stats.hits, 3);
                assert_eq!(final_stats.misses, 3);
            }
            other => panic!("expected SessionEnded, got: {other:?}"),
        }

        shutdown.notify_one();
        server_handle.await.unwrap();
    })
    .await;
}

/// Querying stats on a session without track_stats returns None.
#[tokio::test]
#[ignore] // integration-level: starts real daemon with IPC
async fn session_stats_not_tracked_returns_none() {
    zccache_test_support::test_timeout(async {
        let (endpoint, server_handle, shutdown) = start_daemon().await;
        let mut client = zccache_ipc::connect(&endpoint).await.unwrap();

        client
            .send(&Request::SessionStart {
                client_pid: std::process::id(),
                working_dir: std::env::current_dir().unwrap().into(),
                log_file: None,
                track_stats: false,
                journal_path: None,
            })
            .await
            .unwrap();

        let session_id = match client.recv().await.unwrap() {
            Some(Response::SessionStarted { session_id, .. }) => session_id,
            other => panic!("expected SessionStarted, got: {other:?}"),
        };

        let stats = query_session_stats(&mut client, &session_id).await;
        assert!(stats.is_none(), "stats should be None when not tracking");

        shutdown.notify_one();
        server_handle.await.unwrap();
    })
    .await;
}

/// Querying stats on a nonexistent session returns an error.
#[tokio::test]
#[ignore] // integration-level: starts real daemon with IPC
async fn session_stats_unknown_session() {
    zccache_test_support::test_timeout(async {
        let (endpoint, server_handle, shutdown) = start_daemon().await;
        let mut client = zccache_ipc::connect(&endpoint).await.unwrap();

        client
            .send(&Request::SessionStats {
                session_id: "00000000-0000-0000-0000-000000000000".to_string(),
            })
            .await
            .unwrap();

        match client.recv().await.unwrap() {
            Some(Response::Error { message }) => {
                assert!(
                    message.contains("unknown session"),
                    "expected 'unknown session' in: {message}"
                );
            }
            other => panic!("expected Error, got: {other:?}"),
        }

        shutdown.notify_one();
        server_handle.await.unwrap();
    })
    .await;
}
