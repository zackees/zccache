use bytes::{BufMut, BytesMut};
use zccache::protocol::wire_prost::{encode_prost_message, zccache_v1 as pb, WireFormat};
use zccache::protocol::{
    decode_wire_message, encode_message, peek_frame_protocol_version, DecodedWireMessage,
    ProtocolError, Request, BINCODE_PROTOCOL_VERSION, PROST_PROTOCOL_VERSION,
};

#[test]
fn dispatcher_decodes_v15_bincode_request() {
    let encoded = encode_message(&Request::Ping).unwrap();
    let mut buf = BytesMut::from(&encoded[..]);

    assert_eq!(
        peek_frame_protocol_version(&buf).unwrap(),
        Some(BINCODE_PROTOCOL_VERSION)
    );

    let decoded = decode_wire_message::<Request, pb::Request>(&mut buf)
        .unwrap()
        .unwrap();

    assert_eq!(decoded.wire_format(), WireFormat::BincodeV15);
    assert_eq!(decoded, DecodedWireMessage::BincodeV15(Request::Ping));
    assert!(buf.is_empty());
}

#[test]
fn dispatcher_decodes_v16_prost_request() {
    let request = pb::Request {
        body: Some(pb::request::Body::Ping(pb::Empty {})),
        request_id: "prost-dispatch".to_string(),
    };
    let encoded = encode_prost_message(&request).unwrap();
    let mut buf = BytesMut::from(&encoded[..]);

    assert_eq!(
        peek_frame_protocol_version(&buf).unwrap(),
        Some(PROST_PROTOCOL_VERSION)
    );

    let decoded = decode_wire_message::<Request, pb::Request>(&mut buf)
        .unwrap()
        .unwrap();

    assert_eq!(decoded.wire_format(), WireFormat::ProstV16);
    match decoded {
        DecodedWireMessage::ProstV16(decoded) => {
            assert_eq!(decoded.request_id, "prost-dispatch");
            assert!(matches!(decoded.body, Some(pb::request::Body::Ping(_))));
        }
        other => panic!("expected prost request, got {other:?}"),
    }
    assert!(buf.is_empty());
}

#[test]
fn dispatcher_waits_for_complete_frame_before_consuming() {
    let encoded = encode_message(&Request::Ping).unwrap();
    let mut buf = BytesMut::from(&encoded[..encoded.len() - 1]);
    let before = buf.clone();

    assert_eq!(peek_frame_protocol_version(&buf).unwrap(), None);
    let decoded = decode_wire_message::<Request, pb::Request>(&mut buf).unwrap();

    assert!(decoded.is_none());
    assert_eq!(buf, before);
}

#[test]
fn dispatcher_rejects_unknown_version_without_consuming() {
    let mut buf = BytesMut::new();
    buf.put_u32_le(4);
    buf.put_u32_le(99);
    let before = buf.clone();

    let result = decode_wire_message::<Request, pb::Request>(&mut buf);

    assert!(matches!(
        result,
        Err(ProtocolError::VersionMismatch {
            expected: PROST_PROTOCOL_VERSION,
            received: 99
        })
    ));
    assert_eq!(buf, before);
}
