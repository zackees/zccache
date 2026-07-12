//! End-to-end IPC-based daemon tests: a real `DaemonServer` is started
//! on a unique endpoint, a client connects, and protocol roundtrips
//! (Ping, Status, Clear, Shutdown, SessionStart/End, Compile) are
//! validated. All marked `#[ignore]` so the default unit-test run stays
//! fast; the integration entrypoint opts them back in.

use super::super::*;
use super::CacheDirEnvGuard;

pub(super) async fn start_daemon() -> (String, tokio::task::JoinHandle<()>, Arc<Notify>) {
    let endpoint = crate::ipc::unique_test_endpoint();
    let mut server = DaemonServer::bind(&endpoint).unwrap();
    let shutdown = server.shutdown_handle();
    let handle = tokio::spawn(async move {
        server.run(0).await.unwrap();
    });
    (endpoint, handle, shutdown)
}

#[tokio::test]
#[ignore] // integration-level: starts real daemon with IPC + file watcher
async fn test_server_ping_pong() {
    crate::test_support::test_timeout(async {
        let (endpoint, server_task, shutdown) = start_daemon().await;

        let mut client = crate::ipc::connect(&endpoint).await.unwrap();
        client.send(&Request::Ping).await.unwrap();
        let resp: Option<Response> = client.recv().await.unwrap();
        assert_eq!(resp, Some(Response::Pong));

        shutdown.notify_one();
        server_task.await.unwrap();
    })
    .await;
}

#[tokio::test]
#[ignore] // integration-level: starts real daemon with IPC + file watcher
async fn test_server_shutdown_request() {
    crate::test_support::test_timeout(async {
        let (endpoint, server_task, shutdown) = start_daemon().await;

        let mut client = crate::ipc::connect(&endpoint).await.unwrap();
        client.send(&Request::Shutdown).await.unwrap();
        let resp: Option<Response> = client.recv().await.unwrap();
        assert_eq!(resp, Some(Response::ShuttingDown));

        shutdown.notify_one();
        server_task.await.unwrap();
    })
    .await;
}

#[tokio::test]
#[ignore] // integration-level: starts real daemon with IPC + file watcher
async fn test_server_clear_empty() {
    crate::test_support::test_timeout(async {
        let (endpoint, server_task, shutdown) = start_daemon().await;

        let mut client = crate::ipc::connect(&endpoint).await.unwrap();
        client.send(&Request::Clear).await.unwrap();
        let resp: Option<Response> = client.recv().await.unwrap();
        match resp {
            Some(Response::Cleared {
                metadata_cleared,
                dep_graph_contexts_cleared,
                ..
            }) => {
                // artifacts_removed may be >0 if persistent cache has entries
                // from a prior run. Metadata and dep graph are always fresh.
                assert_eq!(metadata_cleared, 0);
                assert_eq!(dep_graph_contexts_cleared, 0);
            }
            other => panic!("expected Cleared, got: {other:?}"),
        }

        shutdown.notify_one();
        server_task.await.unwrap();
    })
    .await;
}

#[tokio::test]
#[ignore] // integration-level: starts real daemon with IPC + file watcher
async fn test_server_status() {
    crate::test_support::test_timeout(async {
        let tmp = tempfile::tempdir().unwrap();
        let _env = CacheDirEnvGuard::set(tmp.path());
        let (endpoint, server_task, shutdown) = start_daemon().await;

        let mut client = crate::ipc::connect(&endpoint).await.unwrap();
        client.send(&Request::Status).await.unwrap();
        let resp: Option<Response> = client.recv().await.unwrap();
        match resp {
            Some(Response::Status(status)) => {
                assert_eq!(status.endpoint, endpoint);
                assert_eq!(
                    status.daemon_namespace,
                    crate::core::config::DEFAULT_DAEMON_NAMESPACE
                );
            }
            other => panic!("expected Status, got: {other:?}"),
        }

        shutdown.notify_one();
        server_task.await.unwrap();
    })
    .await;
}

// ── CLI session flow tests (IPC-based) ──────────────────────────────

/// Full session lifecycle: start → compile (miss) → compile (hit) → end.
#[tokio::test]
#[ignore] // integration-level: starts real daemon with IPC + file watcher
async fn test_server_status_reports_explicit_daemon_namespace() {
    crate::test_support::test_timeout(async {
        let tmp = tempfile::tempdir().unwrap();
        let _env = CacheDirEnvGuard::set_with_namespace(tmp.path(), "soldr-dev");
        let (endpoint, server_task, shutdown) = start_daemon().await;

        let mut client = crate::ipc::connect(&endpoint).await.unwrap();
        client.send(&Request::Status).await.unwrap();
        let resp: Option<Response> = client.recv().await.unwrap();
        match resp {
            Some(Response::Status(status)) => {
                assert_eq!(status.endpoint, endpoint);
                assert_eq!(status.daemon_namespace, "soldr-dev");
            }
            other => panic!("expected Status, got: {other:?}"),
        }

        shutdown.notify_one();
        server_task.await.unwrap();
    })
    .await;
}

#[tokio::test]
#[ignore] // integration-level: starts real daemon with IPC + file watcher
async fn private_session_start_registers_redacted_status_and_owner_refs() {
    crate::test_support::test_timeout(async {
        let tmp = tempfile::tempdir().unwrap();
        let _env = CacheDirEnvGuard::set_with_namespace(tmp.path(), "soldr-dev");
        let (endpoint, server_task, shutdown) = start_daemon().await;

        let mut client = crate::ipc::connect(&endpoint).await.unwrap();
        client
            .send(&Request::SessionStart {
                client_pid: std::process::id(),
                working_dir: tmp.path().into(),
                log_file: None,
                track_stats: false,
                journal_path: None,
                profile: false,
                private_daemon: Some(crate::protocol::PrivateDaemonSessionOptions {
                    daemon_name: Some("soldr-dev".to_string()),
                    endpoint: Some(endpoint.clone()),
                    cache_dir: Some(crate::core::config::default_cache_dir()),
                    owner_pids: vec![std::process::id()],
                    env: vec![("ZCCACHE_PATH_REMAP".to_string(), "secret".to_string())],
                }),
            })
            .await
            .unwrap();
        let session_id = match client.recv().await.unwrap() {
            Some(Response::SessionStarted { session_id, .. }) => session_id,
            other => panic!("expected SessionStarted, got: {other:?}"),
        };

        client.send(&Request::Status).await.unwrap();
        match client.recv().await.unwrap() {
            Some(Response::Status(status)) => {
                assert!(status.private_daemon.enabled);
                assert_eq!(
                    status.private_daemon.private_env_keys,
                    vec!["ZCCACHE_PATH_REMAP"]
                );
                assert_eq!(status.private_daemon.owners.len(), 1);
                assert_eq!(status.private_daemon.owners[0].pid, std::process::id());
                assert_eq!(status.private_daemon.owners[0].ref_count, 1);
            }
            other => panic!("expected Status, got: {other:?}"),
        }

        client
            .send(&Request::SessionEnd { session_id })
            .await
            .unwrap();
        let _: Option<Response> = client.recv().await.unwrap();

        client.send(&Request::Status).await.unwrap();
        match client.recv().await.unwrap() {
            Some(Response::Status(status)) => {
                assert!(status.private_daemon.enabled);
                assert!(status.private_daemon.owners.is_empty());
            }
            other => panic!("expected Status, got: {other:?}"),
        }

        shutdown.notify_one();
        server_task.await.unwrap();
    })
    .await;
}

#[tokio::test]
#[ignore] // integration-level: starts real daemon with IPC + file watcher
async fn private_session_start_accepts_top_level_cache_root() {
    crate::test_support::test_timeout(async {
        let tmp = tempfile::tempdir().unwrap();
        let _env = CacheDirEnvGuard::set_with_namespace(tmp.path(), "soldr-dev-parent-root");
        let (endpoint, server_task, shutdown) = start_daemon().await;

        let mut client = crate::ipc::connect(&endpoint).await.unwrap();
        client
            .send(&Request::SessionStart {
                client_pid: std::process::id(),
                working_dir: tmp.path().into(),
                log_file: None,
                track_stats: false,
                journal_path: None,
                profile: false,
                private_daemon: Some(crate::protocol::PrivateDaemonSessionOptions {
                    daemon_name: Some("soldr-dev-parent-root".to_string()),
                    endpoint: Some(endpoint.clone()),
                    cache_dir: Some(tmp.path().into()),
                    owner_pids: vec![std::process::id()],
                    env: Vec::new(),
                }),
            })
            .await
            .unwrap();

        match client.recv().await.unwrap() {
            Some(Response::SessionStarted { .. }) => {}
            other => panic!("expected SessionStarted for top-level cache root, got: {other:?}"),
        }

        shutdown.notify_one();
        server_task.await.unwrap();
    })
    .await;
}

#[tokio::test]
#[ignore] // integration-level: starts real daemon with IPC + compiler
async fn cli_session_lifecycle() {
    let clang = match crate::test_support::find_clang() {
        Some(p) => p,
        None => return,
    };
    crate::test_support::test_timeout(async move {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("hello.cpp");
        let obj = tmp.path().join("hello.o");
        let log = tmp.path().join("session.log");
        let cwd = tmp.path().to_string_lossy().into_owned();

        std::fs::write(
            &src,
            format!(
                "// isolated fixture: {}\n#include <stdio.h>\nint main() {{ printf(\"hello\\n\"); return 0; }}\n",
                tmp.path().display()
            ),
        )
        .unwrap();

        let (endpoint, server_handle, shutdown) = start_daemon().await;
        let mut client = crate::ipc::connect(&endpoint).await.unwrap();

        // session-start
        client
            .send(&Request::SessionStart {
                client_pid: std::process::id(),
                working_dir: cwd.clone().into(),
                log_file: Some(log.to_string_lossy().into_owned().into()),
                track_stats: true,
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

        // first compile (cache miss)
        client
            .send(&Request::Compile {
                session_id: session_id.clone(),
                args: vec![
                    "-c".to_string(),
                    src.to_string_lossy().into_owned(),
                    "-o".to_string(),
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
                exit_code, cached, ..
            }) => {
                assert_eq!(exit_code, 0, "first compile should succeed");
                assert!(!cached, "first compile should be a miss");
            }
            other => panic!("expected CompileResult, got: {other:?}"),
        }

        assert!(obj.exists(), ".o should exist after first compile");
        let obj_data = std::fs::read(&obj).unwrap();

        // second compile (cache hit)
        std::fs::remove_file(&obj).unwrap();

        client
            .send(&Request::Compile {
                session_id: session_id.clone(),
                args: vec![
                    "-c".to_string(),
                    src.to_string_lossy().into_owned(),
                    "-o".to_string(),
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
                exit_code, cached, ..
            }) => {
                assert_eq!(exit_code, 0, "cached compile should succeed");
                assert!(cached, "second compile should be a hit");
            }
            other => panic!("expected CompileResult, got: {other:?}"),
        }

        assert!(obj.exists(), ".o should exist after cached compile");
        let cached_data = std::fs::read(&obj).unwrap();
        assert_eq!(obj_data.len(), cached_data.len(), "cached .o should match");

        // session-end
        client
            .send(&Request::SessionEnd {
                session_id: session_id.clone(),
            })
            .await
            .unwrap();

        match client.recv().await.unwrap() {
            Some(Response::SessionEnded { stats: Some(stats) }) => {
                let staged = &stats.phase_profile.expect("phase profile").staged;
                assert!(staged.counters["plan_attempted"] >= 1);
                assert!(staged.counters["plan_enabled"] >= 1);
                assert!(staged.counters["compiler_staged"] >= 1);
                assert!(
                    staged.counters["publication_success"] >= 1,
                    "staged counters: {:?}; failures: {:?}",
                    staged.counters,
                    staged.failures
                );
                assert!(
                    staged.counters["materialize_reflink"]
                        + staged.counters["materialize_copy"]
                        >= 1
                );
            }
            other => panic!("expected SessionEnded, got: {other:?}"),
        }

        // compile after session-end should fail
        client
            .send(&Request::Compile {
                session_id,
                args: vec!["-c".to_string(), src.to_string_lossy().into_owned()],
                cwd: cwd.clone().into(),
                compiler: clang.to_string_lossy().into_owned().into(),
                env: None,
                stdin: Vec::new(),
            })
            .await
            .unwrap();

        match client.recv().await.unwrap() {
            Some(Response::Error { message }) => {
                assert!(
                    message.contains("unknown session"),
                    "should report unknown session after end: {message}"
                );
            }
            other => panic!("expected Error after session-end, got: {other:?}"),
        }

        // verify log
        let log_text = std::fs::read_to_string(&log).unwrap();
        assert!(log_text.contains("[MISS]"), "log should show miss");
        assert!(log_text.contains("[HIT]"), "log should show hit");

        shutdown.notify_one();
        server_handle.await.unwrap();
    })
    .await;
}

#[tokio::test]
#[ignore] // integration-level: real clang + daemon IPC + deterministic store faults
async fn staged_publication_fault_salvages_or_fails_closed_with_forensics() {
    let Some(clang) = crate::test_support::find_clang() else {
        return;
    };
    crate::test_support::test_timeout(async move {
        let tmp = tempfile::tempdir().unwrap();
        let cache_dir = tmp.path().join("cache");
        let _cache_env = CacheDirEnvGuard::set(&cache_dir);
        let endpoint = crate::ipc::unique_test_endpoint();
        let mut server = DaemonServer::bind(&endpoint).unwrap();
        let state = server.test_state_arc();
        let shutdown = server.shutdown_handle();
        let server_task = tokio::spawn(async move { server.run(0).await.unwrap() });
        let mut client = crate::ipc::connect(&endpoint).await.unwrap();
        let cwd = tmp.path().to_string_lossy().into_owned();
        let source = tmp.path().join("fault-success.c");
        let output = tmp.path().join("fault-success.o");
        let failed_source = tmp.path().join("fault-failed.c");
        let failed_output = tmp.path().join("fault-failed.o");
        let index_source = tmp.path().join("fault-index.c");
        let index_output = tmp.path().join("fault-index.o");
        std::fs::write(&source, "int staged_fault_success(void) { return 1; }\n").unwrap();
        std::fs::write(
            &failed_source,
            "int staged_fault_failed(void) { return 2; }\n",
        )
        .unwrap();
        std::fs::write(
            &index_source,
            "int staged_fault_index(void) { return 3; }\n",
        )
        .unwrap();

        client
            .send(&Request::SessionStart {
                client_pid: std::process::id(),
                working_dir: cwd.clone().into(),
                log_file: None,
                track_stats: true,
                journal_path: None,
                profile: false,
                private_daemon: None,
            })
            .await
            .unwrap();
        let session_id = match client.recv().await.unwrap() {
            Some(Response::SessionStarted { session_id, .. }) => session_id,
            other => panic!("expected SessionStarted, got {other:?}"),
        };
        let compile = |session_id: String, source: &Path, output: &Path| Request::Compile {
            session_id,
            args: vec![
                "-c".to_string(),
                source.to_string_lossy().into_owned(),
                "-o".to_string(),
                output.to_string_lossy().into_owned(),
            ],
            cwd: cwd.clone().into(),
            compiler: clang.to_string_lossy().into_owned().into(),
            env: None,
            stdin: Vec::new(),
        };

        let publish_fault =
            StagedFaultGuard::arm(&state.artifact_dir, [StagedFaultPoint::PointerCommit]);
        client
            .send(&compile(session_id.clone(), &source, &output))
            .await
            .unwrap();
        assert!(matches!(
            client.recv().await.unwrap(),
            Some(Response::CompileResult {
                exit_code: 0,
                cached: false,
                ..
            })
        ));
        assert!(output.exists(), "successful compile was not salvaged");
        publish_fault.assert_all_consumed();

        let publish_fault =
            StagedFaultGuard::arm(&state.artifact_dir, [StagedFaultPoint::PointerCommit]);
        let salvage_fault =
            StagedFaultGuard::arm(&failed_output, [StagedFaultPoint::MaterializeOutput(0)]);
        client
            .send(&compile(session_id.clone(), &failed_source, &failed_output))
            .await
            .unwrap();
        assert!(matches!(
            client.recv().await.unwrap(),
            Some(Response::Error { .. })
        ));
        assert!(
            !failed_output.exists(),
            "failed salvage reported a requested output"
        );
        publish_fault.assert_all_consumed();
        salvage_fault.assert_all_consumed();

        let index_fault =
            StagedFaultGuard::arm(&state.artifact_dir, [StagedFaultPoint::IndexCommit]);
        client
            .send(&compile(session_id.clone(), &index_source, &index_output))
            .await
            .unwrap();
        assert!(matches!(
            client.recv().await.unwrap(),
            Some(Response::CompileResult {
                exit_code: 0,
                cached: false,
                ..
            })
        ));
        assert!(index_output.exists(), "index failure was not salvaged");
        index_fault.assert_all_consumed();

        // Failed index publication must not leave a process-local cache entry.
        // The identical request must execute again, not become a false hit.
        std::fs::remove_file(&index_output).unwrap();
        client
            .send(&compile(session_id.clone(), &index_source, &index_output))
            .await
            .unwrap();
        assert!(matches!(
            client.recv().await.unwrap(),
            Some(Response::CompileResult {
                exit_code: 0,
                cached: false,
                ..
            })
        ));
        assert!(
            index_output.exists(),
            "retry after index failure did not compile"
        );

        client
            .send(&Request::SessionEnd {
                session_id: session_id.clone(),
            })
            .await
            .unwrap();
        let staged = match client.recv().await.unwrap() {
            Some(Response::SessionEnded { stats: Some(stats) }) => {
                stats.phase_profile.unwrap().staged
            }
            other => panic!("expected SessionEnded stats, got {other:?}"),
        };
        assert_eq!(staged.counters["publication_failure"], 3);
        assert_eq!(staged.failures["pointer_commit"], 2);
        assert_eq!(staged.failures["index_commit"], 1);
        assert_eq!(staged.counters["salvage_attempt"], 3);
        assert_eq!(staged.counters["salvage_success"], 2);
        assert_eq!(staged.counters["salvage_failure"], 1);
        assert_eq!(staged.counters["materialize_failure"], 1);
        assert!(staged.timings_ns.contains_key("salvage"));

        shutdown.notify_one();
        server_task.await.unwrap();
        let restarted = DaemonServer::bind(&crate::ipc::unique_test_endpoint()).unwrap();
        drop(restarted);

        let lifecycle_path = crate::core::lifecycle::log_file_path();
        assert!(lifecycle_path.as_path().starts_with(&cache_dir));
        let lifecycle = std::fs::read_dir(lifecycle_path.parent().unwrap())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .contains("daemon-lifecycle")
            })
            .map(|entry| std::fs::read_to_string(entry.path()).unwrap())
            .collect::<String>();
        assert_eq!(
            lifecycle
                .matches("\"event\":\"staged_salvage_started\"")
                .count(),
            3
        );
        assert_eq!(
            lifecycle
                .matches("\"event\":\"staged_salvage_complete\"")
                .count(),
            2
        );
        assert_eq!(
            lifecycle
                .matches("\"event\":\"staged_salvage_failed\"")
                .count(),
            1
        );
        for event in lifecycle
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .filter(|event| {
                event["event"]
                    .as_str()
                    .is_some_and(|name| name.starts_with("staged_salvage_"))
            })
        {
            let reason = event["reason"].as_str().expect("bounded salvage reason");
            assert!(reason
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte == b'_'));
            assert!(event["output_count"].is_u64());
            assert!(event["copied_bytes"].is_u64());
            assert!(event["elapsed_ns"].is_u64());
        }
        assert!(!lifecycle.contains(state.staging.path().to_string_lossy().as_ref()));
    })
    .await;
}

/// Regression for #166 — Compile on an unknown session must not fail with
/// "unknown session", mirroring #137's SessionEnd idempotency. Triggered
/// when zccache-ci kills the daemon mid-build (#167).
///
/// The daemon used to short-circuit Compile with `Response::Error` if the
/// session UUID was unknown. After a daemon restart, soldr-managed rustc
/// wrappers keep using the old session UUID and would all fail; soldr in
/// turn exits 1 and the whole build breaks. We now let the compile
/// proceed; only per-session stats are lost.
#[tokio::test]
#[ignore] // integration-level: starts real daemon with IPC
async fn cli_compile_unknown_uuid_is_idempotent() {
    crate::test_support::test_timeout(async {
        let tmp = tempfile::tempdir().unwrap();
        // Use an isolated cache dir so we don't clash with any
        // production daemon writing the global index blob.
        let _cache_dir = CacheDirEnvGuard::set(&tmp.path().join("zccache-cache"));

        let (endpoint, server_handle, shutdown) = start_daemon().await;
        let mut client = crate::ipc::connect(&endpoint).await.unwrap();

        let cwd = tmp.path().to_string_lossy().into_owned();

        // Send a Compile with a well-formed UUID the daemon has never
        // seen. We intentionally pass a bogus compiler path and trivial
        // args — the only assertion is that we don't get the
        // "unknown session" Error response that the pre-#166 code emitted
        // before any real compilation work began.
        client
            .send(&Request::Compile {
                session_id: "00000000-0000-0000-0000-000000000000".to_string(),
                args: vec!["--version".to_string()],
                cwd: cwd.clone().into(),
                compiler: "/nonexistent/compiler".to_string().into(),
                env: None,
                stdin: Vec::new(),
            })
            .await
            .unwrap();

        // Any non-Error response is acceptable — typically a
        // CompileResult with a non-zero exit code because the compiler
        // path is bogus. The key invariant is the absence of the
        // pre-#166 "unknown session" hard error.
        if let Some(Response::Error { message }) = client.recv().await.unwrap() {
            assert!(
                !message.contains("unknown session"),
                "Compile must not fail with 'unknown session' on an unknown UUID, got: {message}"
            );
        }

        shutdown.notify_one();
        server_handle.await.unwrap();
    })
    .await;
}

/// Cache clear resets: miss → hit → clear → miss again.
#[tokio::test]
#[ignore] // integration-level: starts real daemon with IPC + compiler
async fn cli_clear_resets_cache() {
    let clang = match crate::test_support::find_clang() {
        Some(p) => p,
        None => return,
    };

    crate::test_support::test_timeout(async move {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("clear_test.cpp");
        let obj = tmp.path().join("clear_test.o");
        let cwd = tmp.path().to_string_lossy().into_owned();

        std::fs::write(&src, "int main() { return 0; }\n").unwrap();

        let (endpoint, server_handle, shutdown) = start_daemon().await;
        let mut client = crate::ipc::connect(&endpoint).await.unwrap();

        // Start session
        client
            .send(&Request::SessionStart {
                client_pid: std::process::id(),
                working_dir: cwd.clone().into(),
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

        let compile_args = vec![
            "-c".to_string(),
            src.to_string_lossy().into_owned(),
            "-o".to_string(),
            obj.to_string_lossy().into_owned(),
        ];

        // First compile → miss
        client
            .send(&Request::Compile {
                session_id: session_id.clone(),
                args: compile_args.clone(),
                cwd: cwd.clone().into(),
                compiler: clang.to_string_lossy().into_owned().into(),
                env: None,
                stdin: Vec::new(),
            })
            .await
            .unwrap();

        match client.recv().await.unwrap() {
            Some(Response::CompileResult {
                exit_code, cached, ..
            }) => {
                assert_eq!(exit_code, 0);
                assert!(!cached, "first compile should be a miss");
            }
            other => panic!("expected CompileResult, got: {other:?}"),
        }

        // Second compile → hit
        std::fs::remove_file(&obj).unwrap();
        client
            .send(&Request::Compile {
                session_id: session_id.clone(),
                args: compile_args.clone(),
                cwd: cwd.clone().into(),
                compiler: clang.to_string_lossy().into_owned().into(),
                env: None,
                stdin: Vec::new(),
            })
            .await
            .unwrap();

        match client.recv().await.unwrap() {
            Some(Response::CompileResult {
                exit_code, cached, ..
            }) => {
                assert_eq!(exit_code, 0);
                assert!(cached, "second compile should be a hit");
            }
            other => panic!("expected CompileResult, got: {other:?}"),
        }

        // Clear the cache
        client.send(&Request::Clear).await.unwrap();
        match client.recv().await.unwrap() {
            Some(Response::Cleared {
                artifacts_removed, ..
            }) => {
                assert!(
                    artifacts_removed > 0,
                    "should have cleared at least one artifact"
                );
            }
            other => panic!("expected Cleared, got: {other:?}"),
        }

        // End old session and start a new one
        client
            .send(&Request::SessionEnd { session_id })
            .await
            .unwrap();
        let _: Option<Response> = client.recv().await.unwrap();

        client
            .send(&Request::SessionStart {
                client_pid: std::process::id(),
                working_dir: cwd.clone().into(),
                log_file: None,
                track_stats: false,
                journal_path: None,
                profile: false,
                private_daemon: None,
            })
            .await
            .unwrap();

        let session_id2 = match client.recv().await.unwrap() {
            Some(Response::SessionStarted { session_id, .. }) => session_id,
            other => panic!("expected SessionStarted, got: {other:?}"),
        };

        // Compile again → should be a miss (cache was cleared)
        std::fs::remove_file(&obj).unwrap();
        client
            .send(&Request::Compile {
                session_id: session_id2,
                args: compile_args,
                cwd: cwd.into(),
                compiler: clang.to_string_lossy().into_owned().into(),
                env: None,
                stdin: Vec::new(),
            })
            .await
            .unwrap();

        match client.recv().await.unwrap() {
            Some(Response::CompileResult {
                exit_code, cached, ..
            }) => {
                assert_eq!(exit_code, 0);
                assert!(!cached, "compile after clear should be a miss");
            }
            other => panic!("expected CompileResult, got: {other:?}"),
        }

        shutdown.notify_one();
        server_handle.await.unwrap();
    })
    .await;
}

/// Multi-file compilations cache each source independently.
#[tokio::test]
#[ignore] // integration-level: starts real daemon with IPC + compiler
async fn cli_multi_file_compilation_runs_directly() {
    let clang = match crate::test_support::find_clang() {
        Some(p) => p,
        None => return,
    };

    crate::test_support::test_timeout(async move {
        let tmp = tempfile::tempdir().unwrap();
        let src_a = tmp.path().join("multi_a.cpp");
        let src_b = tmp.path().join("multi_b.cpp");
        let cwd = tmp.path().to_string_lossy().into_owned();

        std::fs::write(&src_a, "int foo() { return 1; }\n").unwrap();
        std::fs::write(&src_b, "int bar() { return 2; }\n").unwrap();

        let (endpoint, server_handle, shutdown) = start_daemon().await;
        let mut client = crate::ipc::connect(&endpoint).await.unwrap();

        // Start session
        client
            .send(&Request::SessionStart {
                client_pid: std::process::id(),
                working_dir: cwd.clone().into(),
                log_file: None,
                track_stats: true,
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

        // First compile: multi-file → both are cache misses
        let multi_args = vec![
            "-c".to_string(),
            src_a.to_string_lossy().into_owned(),
            src_b.to_string_lossy().into_owned(),
        ];
        client
            .send(&Request::Compile {
                session_id: session_id.clone(),
                args: multi_args.clone(),
                cwd: cwd.clone().into(),
                compiler: clang.to_string_lossy().into_owned().into(),
                env: None,
                stdin: Vec::new(),
            })
            .await
            .unwrap();

        match client.recv().await.unwrap() {
            Some(Response::CompileResult {
                exit_code, cached, ..
            }) => {
                assert_eq!(exit_code, 0, "multi-file compile should succeed");
                assert!(!cached, "first multi-file compile should be a miss");
            }
            other => panic!("expected CompileResult, got: {other:?}"),
        }

        // Verify both .o files were produced
        let obj_a = tmp.path().join("multi_a.o");
        let obj_b = tmp.path().join("multi_b.o");
        assert!(obj_a.exists(), "multi_a.o should exist");
        assert!(obj_b.exists(), "multi_b.o should exist");

        // Second compile: same files → should be all cache hits
        client
            .send(&Request::Compile {
                session_id: session_id.clone(),
                args: multi_args,
                cwd: cwd.clone().into(),
                compiler: clang.to_string_lossy().into_owned().into(),
                env: None,
                stdin: Vec::new(),
            })
            .await
            .unwrap();

        match client.recv().await.unwrap() {
            Some(Response::CompileResult {
                exit_code, cached, ..
            }) => {
                assert_eq!(exit_code, 0, "second multi-file compile should succeed");
                assert!(cached, "second multi-file compile should be all cache hits");
            }
            other => panic!("expected CompileResult, got: {other:?}"),
        }

        // End session and verify stats
        client
            .send(&Request::SessionEnd { session_id })
            .await
            .unwrap();

        match client.recv().await.unwrap() {
            Some(Response::SessionEnded { stats }) => {
                if let Some(s) = stats {
                    assert!(
                        s.misses >= 2,
                        "first multi-file compile should have 2 misses, got: {}",
                        s.misses
                    );
                    assert!(
                        s.hits >= 2,
                        "second multi-file compile should have 2 hits, got: {}",
                        s.hits
                    );
                }
            }
            other => panic!("expected SessionEnded, got: {other:?}"),
        }

        shutdown.notify_one();
        server_handle.await.unwrap();
    })
    .await;
}
