//! Downstream-consumer smoke + decode-robustness tests for
//! `running_process::broker::protocol_v2`.
//!
//! Coordinates with [zackees/zccache#777](https://github.com/zackees/zccache/issues/777),
//! [zackees/zccache#848](https://github.com/zackees/zccache/issues/848), and
//! [zackees/running-process#483](https://github.com/zackees/running-process/issues/483).
//!
//! Slice 1 of the broker-v2 work (running-process PR #484) added the
//! `protocol_v2::ServiceDefinition` envelope + `HttpServerCapability`
//! optional sub-message. Slices 5-6 (#492, #493) added the
//! `BackendHttpReady` / `GetBrokerHttpEndpoint` control-plane messages.
//! This file covers:
//!
//! 1. Round-trip smokes for the two `ServiceDefinition` shapes (no-HTTP +
//!    with HTTP capability) — proves the v2 types are importable from a
//!    downstream consumer.
//! 2. Decode-robustness: unknown-field tolerance (prost forward-compat)
//!    and truncated-input rejection (DOS resistance).
//! 3. Round-trips for the control-plane messages — `BackendHttpReady`
//!    boundary values and `GetBrokerHttpEndpoint{Request,Response}` —
//!    that #777 slice 5+ will consume from the daemon side.
//!
//! Each test that touches a `prost` shape fails fast if the upstream
//! field set regresses, so the workspace-level path-dep pin
//! (`[patch.crates-io]` → local running-process) surfaces breakage at
//! `cargo check` time rather than at first runtime decode.

use prost::Message;
use running_process::broker::protocol_v2::{
    BackendHttpReady, GetBrokerHttpEndpointRequest, GetBrokerHttpEndpointResponse,
    HttpServerCapability, ServiceDefinition,
};

#[test]
fn protocol_v2_service_definition_round_trips_without_http() {
    let original = ServiceDefinition {
        service_name: "zccache".to_owned(),
        http_server: None,
    };

    let bytes = original.encode_to_vec();
    let decoded =
        ServiceDefinition::decode(bytes.as_slice()).expect("encoded ServiceDefinition decodes");

    assert_eq!(decoded.service_name, "zccache");
    assert!(decoded.http_server.is_none());
}

#[test]
fn protocol_v2_service_definition_round_trips_with_http_capability() {
    let original = ServiceDefinition {
        service_name: "zccache".to_owned(),
        http_server: Some(HttpServerCapability {
            bind_addr: "127.0.0.1".to_owned(),
            health_path: "/health".to_owned(),
            display_name: "zccache status".to_owned(),
        }),
    };

    let bytes = original.encode_to_vec();
    let decoded =
        ServiceDefinition::decode(bytes.as_slice()).expect("encoded ServiceDefinition decodes");

    let cap = decoded
        .http_server
        .expect("http_server survives round-trip");
    assert_eq!(decoded.service_name, "zccache");
    assert_eq!(cap.bind_addr, "127.0.0.1");
    assert_eq!(cap.health_path, "/health");
    assert_eq!(cap.display_name, "zccache status");
}

/// Forward-compat: an unknown field in a future v2 broker's
/// `ServiceDefinition` must not break this consumer. Prost silently
/// skips unknown fields per proto3 semantics; this test pins that
/// behavior so a future migration to a stricter decoder fails LOUDLY
/// here instead of at the first real broker upgrade. P1-7 from #848.
#[test]
fn service_definition_decode_tolerates_unknown_future_field() {
    // Build the on-wire bytes for the known fields, then append a
    // synthetic tag = 99 (well above any real future tag) carrying a
    // length-delimited "ignored" payload. Prost field tag encoding is
    // (field_no << 3) | wire_type. For field 99, length-delimited
    // (wire_type = 2): (99 << 3) | 2 = 0x32A == 794 → varint encoded as
    // two bytes (0x9A 0x06). Followed by length=10 then 10 bytes of payload.
    let mut bytes = ServiceDefinition {
        service_name: "zccache".to_owned(),
        http_server: None,
    }
    .encode_to_vec();
    bytes.extend_from_slice(&[0x9A, 0x06, 10]);
    bytes.extend_from_slice(b"ignoreddat");

    let decoded =
        ServiceDefinition::decode(bytes.as_slice()).expect("unknown field must not block decode");
    assert_eq!(decoded.service_name, "zccache");
    assert!(decoded.http_server.is_none());
}

/// DOS resistance: a truncated `ServiceDefinition` (length-delimited
/// sub-message field claims more bytes than the buffer carries) must
/// return an error, not panic or read past end. P1-7 from #848.
#[test]
fn service_definition_decode_rejects_truncated_input() {
    // Build a real `with-http` shape, then truncate to 6 bytes — enough
    // to land mid-field. Prost MUST surface this as Err, never panic.
    let mut bytes = ServiceDefinition {
        service_name: "zccache".to_owned(),
        http_server: Some(HttpServerCapability {
            bind_addr: "127.0.0.1".to_owned(),
            health_path: "/health".to_owned(),
            display_name: "zccache status".to_owned(),
        }),
    }
    .encode_to_vec();
    bytes.truncate(6);

    let result = ServiceDefinition::decode(bytes.as_slice());
    assert!(
        result.is_err(),
        "truncated ServiceDefinition must error, not panic; got: {result:?}"
    );

    // The empty buffer is the degenerate truncated case — prost decodes it
    // as the default struct (proto3 lets all fields be absent). Pin that
    // behavior so a future stricter decoder is caught here too.
    let empty = ServiceDefinition::decode(&[] as &[u8])
        .expect("empty buffer is a valid proto3 message with default fields");
    assert!(empty.service_name.is_empty());
    assert!(empty.http_server.is_none());
}

/// P2-9 from #848: `BackendHttpReady` carries a `uint32` port wire-type
/// but the daemon uses `u16` semantics. The boundary cases (0 and
/// u16::MAX) must survive the proto round-trip cleanly so the daemon
/// can rely on the broker rejecting out-of-range values at receive
/// time rather than silently coercing.
#[test]
fn backend_http_ready_round_trips_at_u16_boundaries() {
    for port in [0u16, 1, 80, 8080, 49_152, u16::MAX] {
        let original = BackendHttpReady {
            port: u32::from(port),
        };
        let bytes = original.encode_to_vec();
        let decoded = BackendHttpReady::decode(bytes.as_slice()).expect("BackendHttpReady decodes");
        assert_eq!(
            decoded.port,
            u32::from(port),
            "round-trip at port={port} must preserve value"
        );
    }
}

/// P2-9 from #848: `BackendHttpReady`'s wire type accepts `u32` values
/// above `u16::MAX`. Document the consumer-side contract: the broker
/// validates the range on receipt, NOT the proto decoder. This test
/// pins that the decode itself succeeds — the broker's range check
/// (running-process side) is the policy enforcement point.
#[test]
fn backend_http_ready_accepts_out_of_range_u32_at_decode_time() {
    let oversize = BackendHttpReady {
        port: u32::from(u16::MAX) + 1,
    };
    let bytes = oversize.encode_to_vec();
    let decoded = BackendHttpReady::decode(bytes.as_slice())
        .expect("out-of-range port still decodes; broker enforces u16 range");
    assert_eq!(decoded.port, u32::from(u16::MAX) + 1);
}

/// P2-9 from #848: `GetBrokerHttpEndpointRequest` is an empty marker;
/// pin both encode and decode so a future field addition is caught
/// immediately by this consumer.
#[test]
fn get_broker_http_endpoint_request_is_empty_marker() {
    let original = GetBrokerHttpEndpointRequest::default();
    let bytes = original.encode_to_vec();
    assert!(bytes.is_empty(), "empty marker must serialize to 0 bytes");

    let decoded =
        GetBrokerHttpEndpointRequest::decode(bytes.as_slice()).expect("empty marker decodes");
    assert_eq!(decoded, original);
}

/// P2-9 from #848: `GetBrokerHttpEndpointResponse` round-trip pins the
/// `(port, pid)` discovery shape #483 §4 specifies. The consumer
/// (zccache CLI in slice 21) needs both fields to distinguish stale
/// vs. live broker answers across a mid-restart.
#[test]
fn get_broker_http_endpoint_response_round_trips() {
    let original = GetBrokerHttpEndpointResponse {
        port: 12_345,
        pid: 0x0FFF_F1EE,
    };
    let bytes = original.encode_to_vec();
    let decoded = GetBrokerHttpEndpointResponse::decode(bytes.as_slice())
        .expect("GetBrokerHttpEndpointResponse decodes");
    assert_eq!(decoded.port, 12_345);
    assert_eq!(decoded.pid, 0x0FFF_F1EE);

    // Boundary: u32::MAX port + pid. Proto encodes both as varint; the
    // round-trip must preserve every bit even on the largest values.
    let max = GetBrokerHttpEndpointResponse {
        port: u32::MAX,
        pid: u32::MAX,
    };
    let bytes_max = max.encode_to_vec();
    let decoded_max = GetBrokerHttpEndpointResponse::decode(bytes_max.as_slice())
        .expect("u32::MAX values decode");
    assert_eq!(decoded_max.port, u32::MAX);
    assert_eq!(decoded_max.pid, u32::MAX);
}
