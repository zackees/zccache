use bytes::BytesMut;
use prost::Message;
use zccache::protocol::wire_prost::{
    decode_prost_message, encode_prost_message, supported_control_request_from_prost,
    supported_control_request_to_prost, supported_control_response_from_prost,
    supported_control_response_to_prost, wire_format_for_protocol_version, zccache_v1 as pb,
    WireFormat,
};
use zccache::protocol::{decode_message, encode_message, PROST_PROTOCOL_VERSION};

#[test]
fn prost_request_frame_roundtrips_with_v16_header() {
    let request = pb::Request {
        body: Some(pb::request::Body::Ping(pb::Empty {})),
        request_id: "req-1".to_string(),
    };

    let encoded = encode_prost_message(&request).unwrap();
    let version = u32::from_le_bytes([encoded[4], encoded[5], encoded[6], encoded[7]]);
    assert_eq!(version, PROST_PROTOCOL_VERSION);

    let mut buf = BytesMut::from(&encoded[..]);
    let decoded = decode_prost_message::<pb::Request>(&mut buf)
        .unwrap()
        .unwrap();
    assert_eq!(decoded.request_id, "req-1");
    assert!(matches!(decoded.body, Some(pb::request::Body::Ping(_))));
    assert!(buf.is_empty());
}

#[test]
fn prost_response_frame_roundtrips_release_worktree_result() {
    let response = pb::Response {
        body: Some(pb::response::Body::ReleaseWorktreeHandlesResult(
            pb::ReleaseWorktreeHandlesResult {
                inspected: 2,
                released: 1,
                sessions_dropped: vec!["session-a".to_string()],
                unreleased: vec![pb::Path {
                    value: "/tmp/worktree/locked.obj".to_string(),
                }],
            },
        )),
        request_id: "req-release".to_string(),
    };

    let encoded = encode_prost_message(&response).unwrap();
    let mut buf = BytesMut::from(&encoded[..]);
    let decoded = decode_prost_message::<pb::Response>(&mut buf)
        .unwrap()
        .unwrap();

    match decoded.body {
        Some(pb::response::Body::ReleaseWorktreeHandlesResult(result)) => {
            assert_eq!(result.inspected, 2);
            assert_eq!(result.released, 1);
            assert_eq!(result.sessions_dropped, ["session-a"]);
            assert_eq!(result.unreleased[0].value, "/tmp/worktree/locked.obj");
        }
        _ => panic!("expected release-worktree response"),
    }
}

#[test]
fn prost_clear_request_and_response_convert_to_protocol_enums() {
    let request = zccache::protocol::Request::Clear;
    let prost_request = supported_control_request_to_prost(&request).unwrap();
    assert_eq!(prost_request.request_id, "control-clear");
    assert!(matches!(
        prost_request.body,
        Some(pb::request::Body::Clear(_))
    ));
    assert_eq!(
        supported_control_request_from_prost(prost_request).unwrap(),
        request
    );

    let response = zccache::protocol::Response::Cleared {
        artifacts_removed: 1,
        metadata_cleared: 2,
        dep_graph_contexts_cleared: 3,
        on_disk_bytes_freed: 4,
    };
    let prost_response = supported_control_response_to_prost(&response, "clear-1").unwrap();
    assert_eq!(prost_response.request_id, "clear-1");
    assert!(matches!(
        prost_response.body,
        Some(pb::response::Body::Cleared(_))
    ));
    assert_eq!(
        supported_control_response_from_prost(prost_response).unwrap(),
        response
    );
}

#[test]
fn generated_frame_envelope_can_carry_opaque_payload() {
    let request = pb::Request {
        body: Some(pb::request::Body::Status(pb::Empty {})),
        request_id: "req-status".to_string(),
    };
    let payload = request.encode_to_vec();
    let frame = pb::Frame {
        protocol_version: PROST_PROTOCOL_VERSION,
        payload,
        payload_type: "zccache.v1.Request".to_string(),
    };

    let encoded = encode_prost_message(&frame).unwrap();
    let mut buf = BytesMut::from(&encoded[..]);
    let decoded = decode_prost_message::<pb::Frame>(&mut buf)
        .unwrap()
        .unwrap();
    let request = pb::Request::decode(&decoded.payload[..]).unwrap();

    assert_eq!(decoded.protocol_version, PROST_PROTOCOL_VERSION);
    assert_eq!(decoded.payload_type, "zccache.v1.Request");
    assert!(matches!(request.body, Some(pb::request::Body::Status(_))));
}

#[test]
fn bincode_v15_frame_still_roundtrips_on_current_api() {
    let encoded = encode_message(&zccache::protocol::Request::Ping).unwrap();
    let mut buf = BytesMut::from(&encoded[..]);
    let decoded = decode_message::<zccache::protocol::Request>(&mut buf)
        .unwrap()
        .unwrap();

    assert_eq!(decoded, zccache::protocol::Request::Ping);
}

#[test]
fn protocol_version_dispatch_models_v15_and_v16() {
    assert_eq!(
        wire_format_for_protocol_version(WireFormat::BincodeV15.protocol_version()),
        Some(WireFormat::BincodeV15)
    );
    assert_eq!(
        wire_format_for_protocol_version(WireFormat::ProstV16.protocol_version()),
        Some(WireFormat::ProstV16)
    );
    assert_eq!(wire_format_for_protocol_version(99), None);
}
