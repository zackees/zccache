//! Live IPC roundtrip tests for `server/connection/`: v15 bincode and v16
//! prost control requests through a real `DaemonServer` + client pair.
//! Moved out of `connection.rs`'s `live_ipc_prost_tests` module during the
//! #1154 phase-0 split (`crates/CLAUDE.md` § File-size discipline).

use super::super::*;
use crate::protocol::{
    wire_prost::{self, zccache_v1 as pb},
    DecodedWireMessage,
};

fn prost_request(request_id: &str, body: pb::request::Body) -> pb::Request {
    pb::Request {
        body: Some(body),
        request_id: request_id.to_string(),
    }
}

#[tokio::test]
async fn handle_connection_accepts_v15_and_v16_control_requests() {
    crate::test_support::test_timeout(async {
        let endpoint = crate::ipc::unique_test_endpoint();
        let temp = tempfile::tempdir().unwrap();
        let cache_dir: crate::core::NormalizedPath = temp.path().into();
        let DaemonServer {
            mut listener,
            state,
            ..
        } = DaemonServer::bind_with_cache_dir(&endpoint, &cache_dir).unwrap();

        let server_task = tokio::spawn(async move {
            let conn = listener.accept().await.unwrap();
            handle_connection(conn, state).await.unwrap();
        });

        let mut client = crate::ipc::connect(&endpoint).await.unwrap();

        client.send(&Request::Ping).await.unwrap();
        let response: Option<DecodedWireMessage<Response, pb::Response>> =
            client.recv_wire().await.unwrap();
        assert_eq!(
            response,
            Some(DecodedWireMessage::BincodeV15(Response::Pong))
        );

        client
            .send_prost(&prost_request(
                "prost-status",
                pb::request::Body::Status(pb::Empty {}),
            ))
            .await
            .unwrap();
        let response: Option<DecodedWireMessage<Response, pb::Response>> =
            client.recv_wire().await.unwrap();
        match response {
            Some(DecodedWireMessage::ProstV16(response)) => {
                assert_eq!(response.request_id, "prost-status");
                let response = wire_prost::supported_control_response_from_prost(response).unwrap();
                let Response::Status(status) = response else {
                    panic!("expected Status response, got {response:?}");
                };
                assert_eq!(status.endpoint, endpoint);
                assert!(status.bincode_request_telemetry_available);
                assert_eq!(
                    status.bincode_requests_by_type.get("control-ping"),
                    Some(&1)
                );
            }
            other => panic!("expected Status response, got {other:?}"),
        }

        client
            .send_prost(&prost_request(
                "prost-clear",
                pb::request::Body::Clear(pb::Empty {}),
            ))
            .await
            .unwrap();
        let response: Option<DecodedWireMessage<Response, pb::Response>> =
            client.recv_wire().await.unwrap();
        match response {
            Some(DecodedWireMessage::ProstV16(response)) => {
                assert_eq!(response.request_id, "prost-clear");
                let response = wire_prost::supported_control_response_from_prost(response).unwrap();
                let Response::Cleared { .. } = response else {
                    panic!("expected Cleared response, got {response:?}");
                };
            }
            other => panic!("expected Cleared response, got {other:?}"),
        }

        let release_path = temp.path().join("orphan-worktree");
        client
            .send_prost(&prost_request(
                "prost-release-worktree",
                pb::request::Body::ReleaseWorktreeHandles(pb::ReleaseWorktreeHandles {
                    path: Some(pb::Path {
                        value: release_path.to_string_lossy().into_owned(),
                    }),
                }),
            ))
            .await
            .unwrap();
        let response: Option<DecodedWireMessage<Response, pb::Response>> =
            client.recv_wire().await.unwrap();
        match response {
            Some(DecodedWireMessage::ProstV16(response)) => {
                assert_eq!(response.request_id, "prost-release-worktree");
                let response = wire_prost::supported_control_response_from_prost(response).unwrap();
                let Response::ReleaseWorktreeHandlesResult {
                    inspected,
                    released,
                    sessions_dropped,
                    unreleased,
                } = response
                else {
                    panic!("expected ReleaseWorktreeHandlesResult response, got {response:?}");
                };
                assert_eq!(inspected, 0);
                assert_eq!(released, 0);
                assert!(sessions_dropped.is_empty());
                assert!(unreleased.is_empty());
            }
            other => panic!("expected ReleaseWorktreeHandlesResult response, got {other:?}"),
        }

        client
            .send_prost(&prost_request(
                "prost-ping",
                pb::request::Body::Ping(pb::Empty {}),
            ))
            .await
            .unwrap();
        let response: Option<DecodedWireMessage<Response, pb::Response>> =
            client.recv_wire().await.unwrap();
        match response {
            Some(DecodedWireMessage::ProstV16(response)) => {
                assert_eq!(response.request_id, "prost-ping");
                let response = wire_prost::supported_control_response_from_prost(response).unwrap();
                assert_eq!(response, Response::Pong);
            }
            other => panic!("expected prost Pong response, got {other:?}"),
        }

        client
            .send_prost(&prost_request(
                "prost-shutdown",
                pb::request::Body::Shutdown(pb::Empty {}),
            ))
            .await
            .unwrap();
        let response: Option<DecodedWireMessage<Response, pb::Response>> =
            client.recv_wire().await.unwrap();
        match response {
            Some(DecodedWireMessage::ProstV16(response)) => {
                assert_eq!(response.request_id, "prost-shutdown");
                let response = wire_prost::supported_control_response_from_prost(response).unwrap();
                assert_eq!(response, Response::ShuttingDown);
            }
            other => panic!("expected prost ShuttingDown response, got {other:?}"),
        }

        server_task.await.unwrap();
    })
    .await;
}

/// #840 Slice 5: the legacy-lane counter is only reachable through
/// `Request::Status`, so an idle-timed-out daemon takes its counts with it and
/// the soak curve cannot be reconstructed afterwards. `bincode_request_totals`
/// is what the death events log; it has to agree with the live snapshot.
#[tokio::test]
async fn bincode_request_totals_sum_the_live_snapshot() {
    let endpoint = crate::ipc::unique_test_endpoint();
    let temp = tempfile::tempdir().unwrap();
    let cache_dir: crate::core::NormalizedPath = temp.path().into();
    let DaemonServer { state, .. } =
        DaemonServer::bind_with_cache_dir(&endpoint, &cache_dir).unwrap();

    let (total, by_type) = state.bincode_request_totals();
    assert_eq!(total, 0, "a fresh daemon has seen no legacy traffic");
    assert!(by_type.is_empty());

    state.record_bincode_request(&crate::protocol::Request::Ping);
    state.record_bincode_request(&crate::protocol::Request::Ping);
    state.record_bincode_request(&crate::protocol::Request::Status);

    let (total, by_type) = state.bincode_request_totals();
    assert_eq!(total, 3, "total must sum every request family");
    assert_eq!(by_type.get("control-ping"), Some(&2));
    assert_eq!(by_type.get("control-status"), Some(&1));
    assert_eq!(
        total,
        by_type.values().sum::<u64>(),
        "the logged total must agree with the logged breakdown"
    );
}
