//! FrameV1 daemon-wire lane tests (running-process `Frame` envelopes).
//!
//! Covers, for zackees/running-process#383:
//! 1. Golden-bytes pinning of the exact on-wire layout of one request and
//!    one response frame (modeled on running-process's
//!    `tests/broker/golden_bytes.rs`). The expected byte arrays were derived
//!    ONCE (by encoding the sample values and pasting the output) and are
//!    now frozen; the tests never re-derive expectations from the encoder
//!    under test. Any diff is a wire-format break.
//! 2. A live IPC round-trip against a real daemon with
//!    `WireFormat::FrameV1`, including multiple sequential requests on one
//!    connection.
//! 3. Mixed-wire coexistence: sequential connections speaking v15 bincode,
//!    v16 prost, FrameV1, and a running-process `BackendHandle` identity
//!    probe against the same daemon endpoint.

use bytes::BytesMut;
use zccache::daemon::DaemonServer;
use zccache::protocol::wire_frame::{
    buffer_starts_running_process_frame, decode_frame_v1_message, encode_frame_v1_request,
    encode_frame_v1_response, ZCCACHE_FRAME_PAYLOAD_PROTOCOL,
};
use zccache::protocol::wire_prost::{zccache_v1 as pb, WireFormat};
use zccache::protocol::{Request, Response};

// ---------------------------------------------------------------------------
// Sample values. Small, deterministic, every relevant field populated.
// ---------------------------------------------------------------------------

fn sample_ping_request() -> pb::Request {
    pb::Request {
        body: Some(pb::request::Body::Ping(pb::Empty {})),
        request_id: "frame-ping".to_string(),
    }
}

fn sample_pong_response() -> pb::Response {
    pb::Response {
        body: Some(pb::response::Body::Pong(pb::Empty {})),
        request_id: "frame-ping".to_string(),
    }
}

const SAMPLE_FRAME_REQUEST_ID: u64 = 7;

// ---------------------------------------------------------------------------
// FROZEN golden bytes. Derived once from the samples above; never
// regenerate these from the encoder. A mismatch is a wire-format break.
//
// Outer layout (running-process broker framing):
//   [u8 envelope_version = 1][u32 LE body_len][prost broker_v1 Frame]
//
// Frame body (prost, ascending field numbers — deterministic):
//   field 1 envelope_version  = 1
//   field 2 kind              = FRAME_KIND_REQUEST (0, omitted) /
//                               FRAME_KIND_RESPONSE (1)
//   field 3 payload_protocol  = ZCCACHE_FRAME_PAYLOAD_PROTOCOL (0x7A63 =
//                               31331, varint 0xE3 0xF4 0x01)
//   field 4 payload           = raw prost zccache_v1.Request / .Response
//                               (no inner [len][version] header)
//   field 5 request_id        = 7
//   fields 6..9 (payload_encoding NONE, deadline 0, empty trace strings)
//                               omitted as proto3 defaults
//
// Inner zccache_v1 payload:
//   field 1   ping/pong oneof arm = Empty {}   (tag 0x0A, len 0)
//   field 100 request_id = "frame-ping"        (tag 0xA2 0x06)
// ---------------------------------------------------------------------------

#[rustfmt::skip]
const GOLDEN_REQUEST_FRAME: &[u8] = &[
    // -- running-process outer framing --
    0x01,                                       // envelope_version byte = 1
    0x19, 0x00, 0x00, 0x00,                     // body_len = 25 (u32 LE)
    // -- broker_v1 Frame --
    0x08, 0x01,                                 // field 1: envelope_version = 1
                                                // field 2: kind = REQUEST (0) omitted
    0x18, 0xE3, 0xF4, 0x01,                     // field 3: payload_protocol = 0x7A63
    0x22, 0x0F,                                 // field 4: payload, 15 bytes
        // -- zccache_v1.Request payload --
        0x0A, 0x00,                             //   field 1: ping = Empty {}
        0xA2, 0x06, 0x0A,                       //   field 100: request_id, 10 bytes
        0x66, 0x72, 0x61, 0x6D, 0x65, 0x2D,     //   "frame-"
        0x70, 0x69, 0x6E, 0x67,                 //   "ping"
    0x28, 0x07,                                 // field 5: request_id = 7
];

#[rustfmt::skip]
const GOLDEN_RESPONSE_FRAME: &[u8] = &[
    // -- running-process outer framing --
    0x01,                                       // envelope_version byte = 1
    0x1B, 0x00, 0x00, 0x00,                     // body_len = 27 (u32 LE)
    // -- broker_v1 Frame --
    0x08, 0x01,                                 // field 1: envelope_version = 1
    0x10, 0x01,                                 // field 2: kind = RESPONSE (1)
    0x18, 0xE3, 0xF4, 0x01,                     // field 3: payload_protocol = 0x7A63
    0x22, 0x0F,                                 // field 4: payload, 15 bytes
        // -- zccache_v1.Response payload --
        0x0A, 0x00,                             //   field 1: pong = Empty {}
        0xA2, 0x06, 0x0A,                       //   field 100: request_id, 10 bytes
        0x66, 0x72, 0x61, 0x6D, 0x65, 0x2D,     //   "frame-"
        0x70, 0x69, 0x6E, 0x67,                 //   "ping"
    0x28, 0x07,                                 // field 5: request_id = 7 (echoed)
];

#[test]
fn payload_protocol_value_is_frozen_and_collision_free() {
    use running_process::broker::backend_lifecycle::probe::BACKEND_HANDLE_PROBE_PAYLOAD_PROTOCOL;

    assert_eq!(ZCCACHE_FRAME_PAYLOAD_PROTOCOL, 0x7A63, "ASCII \"zc\"");
    // The frozen running-process broker registry: 0x00 control (Hello),
    // 0xAD01 admin, 0xB232 probe, 0xD0FF handoff.
    assert_ne!(ZCCACHE_FRAME_PAYLOAD_PROTOCOL, 0x0000);
    assert_ne!(ZCCACHE_FRAME_PAYLOAD_PROTOCOL, 0xAD01);
    assert_ne!(
        ZCCACHE_FRAME_PAYLOAD_PROTOCOL,
        BACKEND_HANDLE_PROBE_PAYLOAD_PROTOCOL
    );
    assert_ne!(ZCCACHE_FRAME_PAYLOAD_PROTOCOL, 0xD0FF);
}

#[test]
fn request_frame_encodes_to_golden_bytes() {
    let wire = encode_frame_v1_request(&sample_ping_request(), SAMPLE_FRAME_REQUEST_ID).unwrap();
    assert_eq!(&wire[..], GOLDEN_REQUEST_FRAME);
}

#[test]
fn response_frame_encodes_to_golden_bytes() {
    let wire = encode_frame_v1_response(&sample_pong_response(), SAMPLE_FRAME_REQUEST_ID).unwrap();
    assert_eq!(&wire[..], GOLDEN_RESPONSE_FRAME);
}

#[test]
fn golden_request_frame_decodes_to_sample() {
    let mut buf = BytesMut::from(GOLDEN_REQUEST_FRAME);
    assert_eq!(buffer_starts_running_process_frame(&buf), Some(true));
    let decoded = decode_frame_v1_message::<pb::Request>(&mut buf)
        .unwrap()
        .unwrap();
    assert_eq!(decoded.request_id, SAMPLE_FRAME_REQUEST_ID);
    assert_eq!(decoded.message, sample_ping_request());
    assert!(buf.is_empty());
}

#[test]
fn golden_response_frame_decodes_to_sample() {
    let mut buf = BytesMut::from(GOLDEN_RESPONSE_FRAME);
    assert_eq!(buffer_starts_running_process_frame(&buf), Some(true));
    let decoded = decode_frame_v1_message::<pb::Response>(&mut buf)
        .unwrap()
        .unwrap();
    assert_eq!(decoded.request_id, SAMPLE_FRAME_REQUEST_ID);
    assert_eq!(decoded.message, sample_pong_response());
    assert!(buf.is_empty());
}

#[test]
fn zccache_v15_and_v16_headers_are_not_mistaken_for_frames() {
    // v15 bincode and v16 prost frames start with a u32 LE length whose low
    // byte may be 1 — the disambiguator must still classify them as zccache.
    let v15 = zccache::protocol::encode_message(&Request::Ping).unwrap();
    assert_eq!(buffer_starts_running_process_frame(&v15), Some(false));

    let v16 = zccache::protocol::wire_prost::encode_prost_message(&sample_ping_request()).unwrap();
    assert_eq!(buffer_starts_running_process_frame(&v16), Some(false));

    // A zccache header whose length low byte is exactly the running-process
    // envelope version (1) — len = 257 ≡ 1 (mod 256) — must stay zccache.
    let mut ambiguous = vec![0x01, 0x01, 0x00, 0x00];
    ambiguous.extend_from_slice(&zccache::protocol::PROST_PROTOCOL_VERSION.to_le_bytes());
    assert_eq!(buffer_starts_running_process_frame(&ambiguous), Some(false));

    // Fewer than 8 buffered bytes starting with 0x01 is still ambiguous.
    assert_eq!(buffer_starts_running_process_frame(&[0x01, 0x02]), None);
}

// ---------------------------------------------------------------------------
// Live IPC round-trips against a real daemon.
// ---------------------------------------------------------------------------

/// Start an isolated daemon (private cache dir) and return the endpoint,
/// server task handle, and shutdown notifier.
fn start_daemon(
    temp: &tempfile::TempDir,
) -> (
    String,
    tokio::task::JoinHandle<()>,
    std::sync::Arc<tokio::sync::Notify>,
) {
    let endpoint = zccache::ipc::unique_test_endpoint();
    let cache_dir: zccache::core::NormalizedPath = temp.path().into();
    let mut server = DaemonServer::bind_with_cache_dir(&endpoint, &cache_dir).unwrap();
    let shutdown = server.shutdown_handle();
    let handle = tokio::spawn(async move {
        server.run(0).await.unwrap();
    });
    (endpoint, handle, shutdown)
}

#[tokio::test]
async fn frame_v1_client_round_trips_control_requests_on_live_daemon() {
    zccache::test_support::test_timeout(async {
        let temp = tempfile::tempdir().unwrap();
        let (endpoint, server_handle, shutdown) = start_daemon(&temp);

        let mut client = zccache::ipc::connect(&endpoint).await.unwrap();

        // Multiple sequential requests on the same FrameV1 connection.
        client
            .send_request(&Request::Ping, WireFormat::FrameV1)
            .await
            .unwrap();
        let response = client.recv_response().await.unwrap();
        assert_eq!(response, Some(Response::Pong));

        client
            .send_request(&Request::Status, WireFormat::FrameV1)
            .await
            .unwrap();
        let response = client.recv_response().await.unwrap();
        let Some(Response::Status(status)) = response else {
            panic!("expected Status response, got {response:?}");
        };
        assert_eq!(status.endpoint, endpoint);

        client
            .send_request(&Request::Ping, WireFormat::FrameV1)
            .await
            .unwrap();
        let response = client.recv_response().await.unwrap();
        assert_eq!(response, Some(Response::Pong));

        shutdown.notify_one();
        server_handle.await.unwrap();
    })
    .await;
}

#[tokio::test]
async fn frame_v1_response_echoes_client_frame_request_id() {
    zccache::test_support::test_timeout(async {
        let temp = tempfile::tempdir().unwrap();
        let (endpoint, server_handle, shutdown) = start_daemon(&temp);

        let mut client = zccache::ipc::connect(&endpoint).await.unwrap();

        // Drive the frame correlation id forward, then assert the daemon
        // echoes the exact id of each request frame.
        for _ in 0..3 {
            let frame_request_id = client
                .send_frame_v1_request(&sample_ping_request())
                .await
                .unwrap();
            // Read the raw frame back so the echoed id is observable.
            let response: Option<zccache::protocol::DecodedWireMessage<Response, pb::Response>> =
                client.recv_wire().await.unwrap();
            match response {
                Some(zccache::protocol::DecodedWireMessage::FrameV1 {
                    message,
                    request_id,
                }) => {
                    assert_eq!(request_id, frame_request_id);
                    assert_eq!(message.request_id, "frame-ping");
                    assert!(matches!(message.body, Some(pb::response::Body::Pong(_))));
                }
                other => panic!("expected FrameV1 Pong response, got {other:?}"),
            }
        }

        shutdown.notify_one();
        server_handle.await.unwrap();
    })
    .await;
}

#[tokio::test]
async fn mixed_wires_and_backend_probe_coexist_on_one_endpoint() {
    zccache::test_support::test_timeout(async {
        let temp = tempfile::tempdir().unwrap();
        let (endpoint, server_handle, shutdown) = start_daemon(&temp);

        // 1) v15 bincode connection.
        {
            let mut client = zccache::ipc::connect(&endpoint).await.unwrap();
            client.send(&Request::Ping).await.unwrap();
            let response: Option<Response> = client.recv().await.unwrap();
            assert_eq!(response, Some(Response::Pong));
        }

        // 2) v16 prost connection.
        {
            let mut client = zccache::ipc::connect(&endpoint).await.unwrap();
            client
                .send_request(&Request::Ping, WireFormat::ProstV16)
                .await
                .unwrap();
            let response = client.recv_response().await.unwrap();
            assert_eq!(response, Some(Response::Pong));
        }

        // 3) FrameV1 connection.
        {
            let mut client = zccache::ipc::connect(&endpoint).await.unwrap();
            client
                .send_request(&Request::Ping, WireFormat::FrameV1)
                .await
                .unwrap();
            let response = client.recv_response().await.unwrap();
            assert_eq!(response, Some(Response::Pong));
        }

        // 4) running-process BackendHandle identity probe.
        {
            let expected = zccache::ipc::current_backend_identity(&endpoint).unwrap();
            let probe_endpoint = expected.ipc_endpoint.clone();
            let service_name = tokio::task::spawn_blocking(move || {
                let handle =
                    running_process::broker::backend_handle::BackendHandle::probe_with_service(
                        "zccache",
                        zccache::core::VERSION,
                        &probe_endpoint,
                        &expected,
                    )
                    .unwrap();
                handle.service_name.clone()
            })
            .await
            .unwrap();
            assert_eq!(service_name, "zccache");
        }

        // 5) The daemon is still healthy afterwards.
        {
            let mut client = zccache::ipc::connect(&endpoint).await.unwrap();
            client
                .send_request(&Request::Ping, WireFormat::FrameV1)
                .await
                .unwrap();
            let response = client.recv_response().await.unwrap();
            assert_eq!(response, Some(Response::Pong));
        }

        shutdown.notify_one();
        server_handle.await.unwrap();
    })
    .await;
}

// ---------------------------------------------------------------------------
// Env selection: `frame` is forced-only.
// ---------------------------------------------------------------------------

#[test]
fn frame_env_spelling_is_forced_only() {
    use zccache::protocol::wire_prost::{
        client_wire_selection_from_env_value, ClientWireSelection,
    };

    assert_eq!(
        client_wire_selection_from_env_value(Some("frame")).unwrap(),
        ClientWireSelection::FrameV1
    );
    assert_eq!(
        client_wire_selection_from_env_value(Some("frame-v1")).unwrap(),
        ClientWireSelection::FrameV1
    );
    assert_eq!(
        ClientWireSelection::FrameV1.preferred_format(),
        WireFormat::FrameV1
    );
    assert!(!ClientWireSelection::FrameV1.allows_bincode_fallback());
    // Auto must NOT prefer the Frame lane.
    assert_ne!(
        ClientWireSelection::Auto.preferred_format(),
        WireFormat::FrameV1
    );
}
