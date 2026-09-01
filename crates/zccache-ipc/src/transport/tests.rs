//! Async unit tests for the IPC transport.

use super::*;
use zccache_protocol::{wire_prost::zccache_v1 as pb, DecodedWireMessage, Request, Response};

#[tokio::test]
async fn connect_error_includes_endpoint_context() {
    let endpoint = unique_test_endpoint();

    let error = match connect(&endpoint).await {
        Ok(_) => panic!("an unbound endpoint must reject the connection"),
        Err(error) => error,
    };

    assert!(
        error
            .to_string()
            .contains(&format!("cannot connect to daemon at {endpoint}")),
        "connection error should identify its endpoint: {error}"
    );
}

#[test]
fn connect_error_context_preserves_io_error_kind() {
    let error = contextualize_connect_error(
        "test-endpoint",
        std::io::Error::new(std::io::ErrorKind::ConnectionRefused, "test refusal"),
    );

    match error {
        IpcError::Io(error) => {
            assert_eq!(error.kind(), std::io::ErrorKind::ConnectionRefused);
            assert!(error.to_string().contains("test-endpoint"));
            assert!(error.to_string().contains("test refusal"));
            let context = error
                .get_ref()
                .and_then(|inner| inner.downcast_ref::<EndpointConnectError>())
                .expect("I/O error should retain its endpoint context");
            assert_eq!(context.endpoint, "test-endpoint");
            let source = std::error::Error::source(&error)
                .and_then(|source| source.downcast_ref::<std::io::Error>())
                .expect("original I/O error should remain in the error source chain");
            assert_eq!(source.kind(), std::io::ErrorKind::ConnectionRefused);
            assert_eq!(source.to_string(), "test refusal");
        }
        other => panic!("expected I/O error, got {other}"),
    }
}

#[tokio::test]
async fn test_ping_pong() {
    let endpoint = unique_test_endpoint();
    let mut listener = IpcListener::bind(&endpoint).unwrap();

    let server = tokio::spawn(async move {
        let mut conn = listener.accept().await.unwrap();
        let msg: Option<Request> = conn.recv().await.unwrap();
        assert_eq!(msg, Some(Request::Ping));
        conn.send(&Response::Pong).await.unwrap();
    });

    let mut client = connect(&endpoint).await.unwrap();
    client.send(&Request::Ping).await.unwrap();
    let resp: Option<Response> = client.recv().await.unwrap();
    assert_eq!(resp, Some(Response::Pong));

    server.await.unwrap();
}

#[tokio::test]
async fn test_multiple_messages() {
    let endpoint = unique_test_endpoint();
    let mut listener = IpcListener::bind(&endpoint).unwrap();

    let server = tokio::spawn(async move {
        let mut conn = listener.accept().await.unwrap();
        for _ in 0..5 {
            let msg: Option<Request> = conn.recv().await.unwrap();
            assert_eq!(msg, Some(Request::Ping));
            conn.send(&Response::Pong).await.unwrap();
        }
    });

    let mut client = connect(&endpoint).await.unwrap();
    for _ in 0..5 {
        client.send(&Request::Ping).await.unwrap();
        let resp: Option<Response> = client.recv().await.unwrap();
        assert_eq!(resp, Some(Response::Pong));
    }

    server.await.unwrap();
}

#[tokio::test]
async fn recv_wire_accepts_prost_request_on_live_ipc() {
    let endpoint = unique_test_endpoint();
    let mut listener = IpcListener::bind(&endpoint).unwrap();

    let server = tokio::spawn(async move {
        let mut conn = listener.accept().await.unwrap();
        let msg: Option<DecodedWireMessage<pb::Request>> = conn.recv_wire().await.unwrap();
        match msg {
            Some(DecodedWireMessage::ProstV16(request)) => {
                assert_eq!(request.request_id, "live-prost");
                assert!(matches!(request.body, Some(pb::request::Body::Ping(_))));
            }
            other => panic!("expected prost request, got {other:?}"),
        }
        conn.send(&Response::Pong).await.unwrap();
    });

    let mut client = connect(&endpoint).await.unwrap();
    let request = pb::Request {
        body: Some(pb::request::Body::Ping(pb::Empty {})),
        request_id: "live-prost".to_string(),
    };
    client.send_prost(&request).await.unwrap();
    let resp: Option<Response> = client.recv().await.unwrap();
    assert_eq!(resp, Some(Response::Pong));

    server.await.unwrap();
}

#[tokio::test]
async fn backend_handle_probe_detector_preserves_zccache_requests() {
    let endpoint = unique_test_endpoint();
    let daemon = crate::current_backend_identity(&endpoint).unwrap();
    let mut listener = IpcListener::bind(&endpoint).unwrap();

    let server = tokio::spawn(async move {
        let mut conn = listener.accept().await.unwrap();
        assert!(!conn.try_serve_backend_handle_probe(&daemon).await.unwrap());
        let msg: Option<Request> = conn.recv().await.unwrap();
        assert_eq!(msg, Some(Request::Ping));
        conn.send(&Response::Pong).await.unwrap();
    });

    let mut client = connect(&endpoint).await.unwrap();
    client.send(&Request::Ping).await.unwrap();
    let resp: Option<Response> = client.recv().await.unwrap();
    assert_eq!(resp, Some(Response::Pong));

    server.await.unwrap();
}

#[tokio::test]
async fn backend_handle_probe_detector_leaves_retired_v25_headers_for_version_rejection() {
    let endpoint = unique_test_endpoint();
    let daemon = crate::current_backend_identity(&endpoint).unwrap();
    let mut listener = IpcListener::bind(&endpoint).unwrap();

    let server = tokio::spawn(async move {
        let mut conn = listener.accept().await.unwrap();
        assert!(!conn.try_serve_backend_handle_probe(&daemon).await.unwrap());
        assert!(matches!(
            conn.recv_wire::<pb::Request>().await,
            Err(IpcError::Protocol(
                zccache_protocol::ProtocolError::VersionMismatch {
                    expected: zccache_protocol::PROST_PROTOCOL_VERSION,
                    received: 25,
                }
            ))
        ));
    });

    let mut client = connect(&endpoint).await.unwrap();
    let mut retired_v25 = Vec::with_capacity(4 + 0x101);
    retired_v25.extend_from_slice(&0x101_u32.to_le_bytes());
    retired_v25.extend_from_slice(&25_u32.to_le_bytes());
    retired_v25.resize(4 + 0x101, 0);
    client.send_opaque_bytes(&retired_v25).await.unwrap();

    server.await.unwrap();
}

#[tokio::test]
async fn backend_handle_probe_succeeds_on_direct_endpoint() {
    let endpoint = unique_test_endpoint();
    let daemon = crate::current_backend_identity(&endpoint).unwrap();
    let probe_endpoint = daemon.ipc_endpoint.clone();
    let expected_daemon = daemon.clone();
    let mut listener = IpcListener::bind(&endpoint).unwrap();

    let server = tokio::spawn(async move {
        let mut conn = listener.accept().await.unwrap();
        assert!(conn.try_serve_backend_handle_probe(&daemon).await.unwrap());
    });

    let (service_name, handle_endpoint) = tokio::task::spawn_blocking(move || {
        let handle = running_process::broker::protocol_v2::backend_handle::BackendHandle::probe_with_service(
            "zccache",
            zccache_core::VERSION,
            &probe_endpoint,
            &expected_daemon,
        )
        .unwrap();
        (
            handle.service_name.clone(),
            handle.daemon_process.ipc_endpoint.path.clone(),
        )
    })
    .await
    .unwrap();

    assert_eq!(service_name, "zccache");
    assert_eq!(
        handle_endpoint,
        crate::running_process_endpoint(&endpoint).path
    );
    server.await.unwrap();
}

#[tokio::test]
async fn test_connection_closed() {
    let endpoint = unique_test_endpoint();
    let mut listener = IpcListener::bind(&endpoint).unwrap();

    let server = tokio::spawn(async move {
        let _conn = listener.accept().await.unwrap();
        // Drop connection immediately
    });

    // Small delay to let server create pipe and start accepting
    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    let mut client = connect(&endpoint).await.unwrap();
    // Give server time to accept and drop
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let resp: Result<Option<Response>, _> = client.recv().await;
    // Should get None (clean close) or ConnectionClosed or broken pipe
    match resp {
        Ok(None) => {}
        Err(IpcError::ConnectionClosed) => {}
        Err(IpcError::Io(_)) => {}
        other => panic!("unexpected result: {other:?}"),
    }

    server.await.unwrap();
}

/// Regression test for <https://github.com/zackees/zccache/issues/666>.
///
/// The pre-#666 Windows accept path would `pop_front().expect(...)`-panic
/// the moment the pool ever depleted. After the fix, a fully drained pool
/// must recover via the emergency-create path on the next accept.
#[cfg(windows)]
#[tokio::test]
async fn pool_recovers_from_full_depletion() {
    let endpoint = unique_test_endpoint();
    let mut listener = IpcListener::bind(&endpoint).unwrap();

    // Simulate the issue #666 wedge: pool is fully drained by repeated
    // replacement-create failures (modelled here by an explicit drain).
    let drained = listener.test_drain_pool();
    assert!(drained > 0, "fresh listener should have pre-created pipes");

    let server = tokio::spawn(async move {
        // accept() on a drained pool must NOT panic — it must take the
        // emergency-create path and serve the client.
        let mut conn = listener.accept().await.expect("accept after drain");
        let msg: Option<Request> = conn.recv().await.unwrap();
        assert_eq!(msg, Some(Request::Ping));
        conn.send(&Response::Pong).await.unwrap();
    });

    // The emergency create + connect handshake adds a few ms — give the
    // server room to set up before the client connects.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let mut client = connect(&endpoint).await.unwrap();
    client.send(&Request::Ping).await.unwrap();
    let resp: Option<Response> = client.recv().await.unwrap();
    assert_eq!(resp, Some(Response::Pong));

    server.await.unwrap();
}

#[tokio::test]
async fn test_parallel_connections() {
    let endpoint = unique_test_endpoint();
    let mut listener = IpcListener::bind(&endpoint).unwrap();
    let n = 8;

    let server = tokio::spawn(async move {
        for _ in 0..n {
            let mut conn = listener.accept().await.unwrap();
            let msg: Option<Request> = conn.recv().await.unwrap();
            assert_eq!(msg, Some(Request::Ping));
            conn.send(&Response::Pong).await.unwrap();
        }
    });

    // Spawn N clients simultaneously to stress the pipe pool.
    let mut handles = Vec::new();
    let ep = endpoint.clone();
    for _ in 0..n {
        let ep = ep.clone();
        handles.push(tokio::spawn(async move {
            let mut client = connect(&ep).await.unwrap();
            client.send(&Request::Ping).await.unwrap();
            let resp: Option<Response> = client.recv().await.unwrap();
            assert_eq!(resp, Some(Response::Pong));
        }));
    }

    for h in handles {
        h.await.unwrap();
    }
    server.await.unwrap();
}

/// #1171: the accept path must recognise a same-user peer and refuse a foreign
/// one. A second real uid is not available in a test, so the daemon-side uid is
/// injected — a peer that is genuinely us fails the check when the daemon
/// believes it is someone else, which is the same comparison the real path
/// makes with the arguments swapped.
/// A same-user client still completes a full round trip through the real
/// `accept()` loop — the check must not cost the happy path a connection.
#[cfg(unix)]
#[tokio::test]
async fn same_user_client_is_served_through_the_peer_check() {
    let endpoint = unique_test_endpoint();
    let mut listener = IpcListener::bind(&endpoint).unwrap();

    let server = tokio::spawn(async move {
        let mut conn = listener.accept().await.unwrap();
        let msg: Option<Request> = conn.recv().await.unwrap();
        assert_eq!(msg, Some(Request::Ping));
        conn.send(&Response::Pong).await.unwrap();
    });

    let mut client = connect(&endpoint).await.unwrap();
    client.send(&Request::Ping).await.unwrap();
    let resp: Option<Response> = client.recv().await.unwrap();
    assert_eq!(resp, Some(Response::Pong));

    server.await.unwrap();
}
