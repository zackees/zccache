//! Integration tests for fingerprint IPC flow.
//!
//! Tests the full CLI → daemon fingerprint pipeline:
//! check, mark-success, mark-failure, invalidate, and watcher-based change detection.

use zccache_monocrate::daemon::DaemonServer;
use zccache_monocrate::protocol::{Request, Response};

/// Helper: start a daemon server on a unique endpoint.
async fn start_daemon() -> (
    String,
    tokio::task::JoinHandle<()>,
    std::sync::Arc<tokio::sync::Notify>,
) {
    let endpoint = zccache_monocrate::ipc::unique_test_endpoint();
    let mut server = DaemonServer::bind(&endpoint).unwrap();
    let shutdown = server.shutdown_handle();
    let handle = tokio::spawn(async move {
        server.run(0).await.unwrap();
    });
    (endpoint, handle, shutdown)
}

fn create_file(dir: &std::path::Path, rel: &str, content: &str) {
    let path = dir.join(rel);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(&path, content).unwrap();
}

#[tokio::test]
#[ignore] // integration-level: starts real daemon
async fn test_fingerprint_check_miss_then_skip() {
    zccache_monocrate::test_support::test_timeout(async {
        let (endpoint, server_handle, shutdown) = start_daemon().await;
        let src = tempfile::TempDir::new().unwrap();
        let cache_dir = tempfile::TempDir::new().unwrap();

        create_file(src.path(), "a.rs", "fn main() {}");
        let cache_file = cache_dir.path().join("fp.json");

        // First check: should return "run" (no cache).
        let mut client = zccache_monocrate::ipc::connect(&endpoint).await.unwrap();
        client
            .send(&Request::FingerprintCheck {
                cache_file: cache_file.clone().into(),
                cache_type: "two-layer".into(),
                root: src.path().to_path_buf().into(),
                extensions: vec![],
                include_globs: vec![],
                exclude: vec![],
            })
            .await
            .unwrap();

        let resp: Option<Response> = client.recv().await.unwrap();
        match resp {
            Some(Response::FingerprintCheckResult { decision, .. }) => {
                assert_eq!(decision, "run", "first check should return 'run'");
            }
            other => panic!("unexpected response: {other:?}"),
        }

        // Mark success.
        client
            .send(&Request::FingerprintMarkSuccess {
                cache_file: cache_file.clone().into(),
            })
            .await
            .unwrap();
        let resp: Option<Response> = client.recv().await.unwrap();
        assert_eq!(resp, Some(Response::FingerprintAck));

        // Second check: should return "skip" (clean).
        client
            .send(&Request::FingerprintCheck {
                cache_file: cache_file.clone().into(),
                cache_type: "two-layer".into(),
                root: src.path().to_path_buf().into(),
                extensions: vec![],
                include_globs: vec![],
                exclude: vec![],
            })
            .await
            .unwrap();

        let resp: Option<Response> = client.recv().await.unwrap();
        match resp {
            Some(Response::FingerprintCheckResult { decision, .. }) => {
                assert_eq!(decision, "skip", "second check should return 'skip'");
            }
            other => panic!("unexpected response: {other:?}"),
        }

        shutdown.notify_one();
        server_handle.await.unwrap();
    })
    .await;
}

#[tokio::test]
#[ignore] // integration-level: starts real daemon
async fn test_fingerprint_mark_failure_forces_rerun() {
    zccache_monocrate::test_support::test_timeout(async {
        let (endpoint, server_handle, shutdown) = start_daemon().await;
        let src = tempfile::TempDir::new().unwrap();
        let cache_dir = tempfile::TempDir::new().unwrap();

        create_file(src.path(), "a.rs", "fn main() {}");
        let cache_file = cache_dir.path().join("fp.json");

        let mut client = zccache_monocrate::ipc::connect(&endpoint).await.unwrap();

        // Check → mark failure.
        client
            .send(&Request::FingerprintCheck {
                cache_file: cache_file.clone().into(),
                cache_type: "two-layer".into(),
                root: src.path().to_path_buf().into(),
                extensions: vec![],
                include_globs: vec![],
                exclude: vec![],
            })
            .await
            .unwrap();
        let _: Option<Response> = client.recv().await.unwrap();

        client
            .send(&Request::FingerprintMarkFailure {
                cache_file: cache_file.clone().into(),
            })
            .await
            .unwrap();
        let resp: Option<Response> = client.recv().await.unwrap();
        assert_eq!(resp, Some(Response::FingerprintAck));

        // Next check should return "run" (previous failure).
        client
            .send(&Request::FingerprintCheck {
                cache_file: cache_file.clone().into(),
                cache_type: "two-layer".into(),
                root: src.path().to_path_buf().into(),
                extensions: vec![],
                include_globs: vec![],
                exclude: vec![],
            })
            .await
            .unwrap();

        let resp: Option<Response> = client.recv().await.unwrap();
        match resp {
            Some(Response::FingerprintCheckResult {
                decision, reason, ..
            }) => {
                assert_eq!(decision, "run");
                assert_eq!(reason.as_deref(), Some("previous failure"));
            }
            other => panic!("unexpected response: {other:?}"),
        }

        shutdown.notify_one();
        server_handle.await.unwrap();
    })
    .await;
}

#[tokio::test]
#[ignore] // integration-level: starts real daemon
async fn test_fingerprint_invalidate() {
    zccache_monocrate::test_support::test_timeout(async {
        let (endpoint, server_handle, shutdown) = start_daemon().await;
        let src = tempfile::TempDir::new().unwrap();
        let cache_dir = tempfile::TempDir::new().unwrap();

        create_file(src.path(), "a.rs", "fn main() {}");
        let cache_file = cache_dir.path().join("fp.json");

        let mut client = zccache_monocrate::ipc::connect(&endpoint).await.unwrap();

        // Check → mark success.
        client
            .send(&Request::FingerprintCheck {
                cache_file: cache_file.clone().into(),
                cache_type: "two-layer".into(),
                root: src.path().to_path_buf().into(),
                extensions: vec![],
                include_globs: vec![],
                exclude: vec![],
            })
            .await
            .unwrap();
        let _: Option<Response> = client.recv().await.unwrap();

        client
            .send(&Request::FingerprintMarkSuccess {
                cache_file: cache_file.clone().into(),
            })
            .await
            .unwrap();
        let _: Option<Response> = client.recv().await.unwrap();

        // Invalidate.
        client
            .send(&Request::FingerprintInvalidate {
                cache_file: cache_file.clone().into(),
            })
            .await
            .unwrap();
        let resp: Option<Response> = client.recv().await.unwrap();
        assert_eq!(resp, Some(Response::FingerprintAck));

        // Next check should return "run" (no cache after invalidation).
        client
            .send(&Request::FingerprintCheck {
                cache_file: cache_file.clone().into(),
                cache_type: "two-layer".into(),
                root: src.path().to_path_buf().into(),
                extensions: vec![],
                include_globs: vec![],
                exclude: vec![],
            })
            .await
            .unwrap();

        let resp: Option<Response> = client.recv().await.unwrap();
        match resp {
            Some(Response::FingerprintCheckResult { decision, .. }) => {
                assert_eq!(decision, "run");
            }
            other => panic!("unexpected response: {other:?}"),
        }

        shutdown.notify_one();
        server_handle.await.unwrap();
    })
    .await;
}

#[tokio::test]
#[ignore] // integration-level: starts real daemon
async fn test_fingerprint_two_watches_independent() {
    zccache_monocrate::test_support::test_timeout(async {
        let (endpoint, server_handle, shutdown) = start_daemon().await;
        let src = tempfile::TempDir::new().unwrap();
        let cache_dir = tempfile::TempDir::new().unwrap();

        create_file(src.path(), "a.rs", "fn main() {}");
        let cache1 = cache_dir.path().join("c1.json");
        let cache2 = cache_dir.path().join("c2.json");

        let mut client = zccache_monocrate::ipc::connect(&endpoint).await.unwrap();

        // Initialize cache1 and mark success.
        client
            .send(&Request::FingerprintCheck {
                cache_file: cache1.clone().into(),
                cache_type: "two-layer".into(),
                root: src.path().to_path_buf().into(),
                extensions: vec![],
                include_globs: vec![],
                exclude: vec![],
            })
            .await
            .unwrap();
        let _: Option<Response> = client.recv().await.unwrap();
        client
            .send(&Request::FingerprintMarkSuccess {
                cache_file: cache1.clone().into(),
            })
            .await
            .unwrap();
        let _: Option<Response> = client.recv().await.unwrap();

        // cache2 should still return run (independent).
        client
            .send(&Request::FingerprintCheck {
                cache_file: cache2.clone().into(),
                cache_type: "hash".into(),
                root: src.path().to_path_buf().into(),
                extensions: vec![],
                include_globs: vec![],
                exclude: vec![],
            })
            .await
            .unwrap();
        let resp: Option<Response> = client.recv().await.unwrap();
        match resp {
            Some(Response::FingerprintCheckResult { decision, .. }) => {
                assert_eq!(decision, "run", "cache2 should be independent from cache1");
            }
            other => panic!("unexpected response: {other:?}"),
        }

        // cache1 should still return skip.
        client
            .send(&Request::FingerprintCheck {
                cache_file: cache1.clone().into(),
                cache_type: "two-layer".into(),
                root: src.path().to_path_buf().into(),
                extensions: vec![],
                include_globs: vec![],
                exclude: vec![],
            })
            .await
            .unwrap();
        let resp: Option<Response> = client.recv().await.unwrap();
        match resp {
            Some(Response::FingerprintCheckResult { decision, .. }) => {
                assert_eq!(decision, "skip", "cache1 should still be clean");
            }
            other => panic!("unexpected response: {other:?}"),
        }

        shutdown.notify_one();
        server_handle.await.unwrap();
    })
    .await;
}
