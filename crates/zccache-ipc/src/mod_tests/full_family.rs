//! Full-family prost-default fallback and no-replay regressions.

use super::*;

#[tokio::test]
async fn full_family_roundtrip_classifies_connect_failure_as_pre_dispatch() {
    let endpoint = unique_test_endpoint();
    let failure = full_family_roundtrip_classified(
        &endpoint,
        &zccache_protocol::Request::SessionEnd {
            session_id: "already-gone".to_string(),
        },
        None,
    )
    .await
    .expect_err("unbound endpoint must fail");

    assert_eq!(failure.phase(), FullFamilyFailurePhase::PreDispatch);
}

#[tokio::test]
async fn full_family_roundtrip_auto_falls_back_to_bincode_for_old_daemon() {
    use super::broker::RUNNING_PROCESS_FAKE_BACKEND_ENV;
    use super::test_env::EnvVarGuard;

    let _env = EnvVarGuard::set_all(&[
        (RUNNING_PROCESS_DISABLE_ENV, Some("1".to_string())),
        (RUNNING_PROCESS_FAKE_BACKEND_ENV, None),
    ]);
    let endpoint = unique_test_endpoint();
    let mut listener = IpcListener::bind(&endpoint).unwrap();

    let server = tokio::spawn(async move {
        let mut first = listener.accept().await.unwrap();
        match first.recv::<zccache_protocol::Request>().await {
            Err(IpcError::Protocol(zccache_protocol::ProtocolError::VersionMismatch {
                expected: zccache_protocol::BINCODE_PROTOCOL_VERSION,
                received: zccache_protocol::PROST_PROTOCOL_VERSION,
            })) => {
                first
                    .send(&Response::Error {
                        message: "protocol version mismatch: expected v15, received v16"
                            .to_string(),
                    })
                    .await
                    .unwrap();
            }
            other => panic!("v16 full-family request must be rejected before dispatch: {other:?}"),
        }

        let mut second = listener.accept().await.unwrap();
        let request: Option<zccache_protocol::Request> = second.recv().await.unwrap();
        assert_eq!(
            request,
            Some(zccache_protocol::Request::SessionStats {
                session_id: "legacy-session".to_string(),
            })
        );
        second
            .send(&Response::SessionStatsResult { stats: None })
            .await
            .unwrap();
    });

    let response = full_family_roundtrip_with_selection(
        &endpoint,
        &zccache_protocol::Request::SessionStats {
            session_id: "legacy-session".to_string(),
        },
        None,
        wire_prost::ClientWireSelection::Auto,
    )
    .await
    .unwrap();

    assert_eq!(response, Some(Response::SessionStatsResult { stats: None }));
    server.await.unwrap();
}

#[tokio::test]
async fn full_family_roundtrip_does_not_replay_a_prost_application_error() {
    let endpoint = unique_test_endpoint();
    let mut listener = IpcListener::bind(&endpoint).unwrap();

    let server = tokio::spawn(async move {
        let mut first = listener.accept().await.unwrap();
        let request: Option<
            zccache_protocol::DecodedWireMessage<
                zccache_protocol::Request,
                wire_prost::zccache_v1::Request,
            >,
        > = first.recv_wire().await.unwrap();
        assert!(matches!(
            request,
            Some(zccache_protocol::DecodedWireMessage::ProstV16(_))
        ));
        let response = Response::Error {
            message: "nested protocol version mismatch".to_string(),
        };
        first
            .send_prost(&wire_prost::response_to_prost(&response, "session-stats"))
            .await
            .unwrap();

        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(100), listener.accept())
                .await
                .is_err(),
            "a prost application error must not open a bincode retry connection"
        );
    });

    let response = full_family_roundtrip_with_selection(
        &endpoint,
        &zccache_protocol::Request::SessionStats {
            session_id: "current-session".to_string(),
        },
        None,
        wire_prost::ClientWireSelection::Auto,
    )
    .await
    .unwrap();

    assert_eq!(
        response,
        Some(Response::Error {
            message: "nested protocol version mismatch".to_string(),
        })
    );
    server.await.unwrap();
}
