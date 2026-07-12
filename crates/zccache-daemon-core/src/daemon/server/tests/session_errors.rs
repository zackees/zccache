//! Session error-path IPC tests kept separate from the main lifecycle matrix.

use super::super::*;
use super::server_ipc::start_daemon;

#[tokio::test]
#[ignore] // integration-level: starts real daemon with IPC
async fn cli_session_end_invalid_id() {
    crate::test_support::test_timeout(async {
        let (endpoint, server_handle, shutdown) = start_daemon().await;
        let mut client = crate::ipc::connect(&endpoint).await.unwrap();

        client
            .send(&Request::SessionEnd {
                session_id: 999999.to_string(),
            })
            .await
            .unwrap();

        match client.recv().await.unwrap() {
            Some(Response::Error { message }) => {
                assert!(
                    message.contains("unknown session") || message.contains("invalid session"),
                    "expected session error, got: {message}"
                );
            }
            other => panic!("expected Error, got: {other:?}"),
        }

        shutdown.notify_one();
        server_handle.await.unwrap();
    })
    .await;
}

/// A well-formed but unknown session is expected after daemon restart, so
/// session-end remains idempotent and returns no stale statistics.
#[tokio::test]
#[ignore] // integration-level: starts real daemon with IPC
async fn cli_session_end_unknown_uuid_is_idempotent() {
    crate::test_support::test_timeout(async {
        let (endpoint, server_handle, shutdown) = start_daemon().await;
        let mut client = crate::ipc::connect(&endpoint).await.unwrap();

        client
            .send(&Request::SessionEnd {
                session_id: "00000000-0000-0000-0000-000000000000".to_string(),
            })
            .await
            .unwrap();

        match client.recv().await.unwrap() {
            Some(Response::SessionEnded { stats }) => assert!(
                stats.is_none(),
                "no stats expected for unknown session, got: {stats:?}"
            ),
            other => panic!("expected SessionEnded for unknown UUID, got: {other:?}"),
        }

        shutdown.notify_one();
        server_handle.await.unwrap();
    })
    .await;
}
