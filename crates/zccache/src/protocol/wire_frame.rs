//! running-process `Frame` envelope lane for the daemon wire.
//!
//! Outer framing is running-process broker framing:
//! `[u8 envelope_version=1][u32 LE body_len][prost broker_v1 Frame]`. The
//! `Frame.payload` carries the raw prost-encoded `zccache_v1` message (no
//! inner `[len][version]` header); `Frame.payload_protocol` is
//! [`ZCCACHE_FRAME_PAYLOAD_PROTOCOL`] and `Frame.request_id` correlates a
//! response to its request. This lane is selected only by an explicit
//! `ZCCACHE_DAEMON_WIRE=frame`; `auto` never prefers it.

use bytes::{Buf, BufMut, BytesMut};
use prost::Message;

use super::{ProtocolError, BINCODE_PROTOCOL_VERSION, PROST_PROTOCOL_VERSION};

/// `payload_protocol` registry value for zccache requests/responses carried
/// inside running-process broker `Frame` envelopes.
///
/// `0x7A63` is ASCII `"zc"` (`0x7A` = 'z', `0x63` = 'c'). Collision check
/// against the frozen running-process broker payload-protocol registry:
///
/// - `0x0000` — broker control (`Hello`/`HelloReply`)
/// - `0xAD01` — broker admin verbs
/// - `0xB232` — `BACKEND_HANDLE_PROBE_PAYLOAD_PROTOCOL`
///   (`running_process::broker::backend_lifecycle::probe`)
/// - `0xD0FF` — broker handoff
///
/// `0x7A63` collides with none of these; a compile-time assertion below pins
/// the probe-constant check so a future running-process bump cannot silently
/// introduce a collision.
pub const ZCCACHE_FRAME_PAYLOAD_PROTOCOL: u32 = 0x7A63;

const _: () = assert!(
    ZCCACHE_FRAME_PAYLOAD_PROTOCOL
        != running_process::broker::backend_lifecycle::probe::BACKEND_HANDLE_PROBE_PAYLOAD_PROTOCOL,
    "zccache Frame payload protocol must not collide with the BackendHandle probe"
);
const _: () = assert!(ZCCACHE_FRAME_PAYLOAD_PROTOCOL != 0x0000);
const _: () = assert!(ZCCACHE_FRAME_PAYLOAD_PROTOCOL != 0xAD01);
const _: () = assert!(ZCCACHE_FRAME_PAYLOAD_PROTOCOL != 0xD0FF);

/// Decoded zccache message extracted from a running-process `Frame`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrameV1Decoded<M> {
    /// The zccache prost message decoded from `Frame.payload`.
    pub message: M,
    /// The `Frame.request_id` (echoed verbatim in the response frame).
    pub request_id: u64,
}

/// Serialize a zccache prost request into a running-process `Frame` envelope.
///
/// # Errors
///
/// Returns an error if prost encoding fails or the frame exceeds the
/// running-process frame size cap.
pub fn encode_frame_v1_request<M: Message>(
    msg: &M,
    request_id: u64,
) -> Result<BytesMut, ProtocolError> {
    encode_frame_v1_message(
        msg,
        running_process::broker::protocol::FrameKind::Request,
        request_id,
    )
}

/// Serialize a zccache prost response into a running-process `Frame` envelope.
///
/// # Errors
///
/// Returns an error if prost encoding fails or the frame exceeds the
/// running-process frame size cap.
pub fn encode_frame_v1_response<M: Message>(
    msg: &M,
    request_id: u64,
) -> Result<BytesMut, ProtocolError> {
    encode_frame_v1_message(
        msg,
        running_process::broker::protocol::FrameKind::Response,
        request_id,
    )
}

fn encode_frame_v1_message<M: Message>(
    msg: &M,
    kind: running_process::broker::protocol::FrameKind,
    request_id: u64,
) -> Result<BytesMut, ProtocolError> {
    use running_process::broker::protocol::{Frame, PayloadEncoding};

    let mut payload = Vec::with_capacity(msg.encoded_len());
    msg.encode(&mut payload)
        .map_err(|e| ProtocolError::Serialization(e.to_string()))?;

    let frame = Frame {
        envelope_version: 1,
        kind: kind as i32,
        payload_protocol: ZCCACHE_FRAME_PAYLOAD_PROTOCOL,
        payload,
        request_id,
        payload_encoding: PayloadEncoding::None as i32,
        deadline_unix_ms: 0,
        traceparent: String::new(),
        tracestate: String::new(),
    };

    let mut body = Vec::with_capacity(frame.encoded_len());
    frame
        .encode(&mut body)
        .map_err(|e| ProtocolError::Serialization(e.to_string()))?;
    if body.len() > running_process::broker::protocol::MAX_FRAME_BYTES {
        return Err(ProtocolError::MessageTooLarge(body.len()));
    }

    let mut buf = BytesMut::with_capacity(1 + 4 + body.len());
    buf.put_u8(running_process::broker::protocol::ENVELOPE_VERSION);
    buf.put_u32_le(u32::try_from(body.len()).expect("frame body under 16 MiB cap fits in u32"));
    buf.extend_from_slice(&body);
    Ok(buf)
}

/// Whether the buffered bytes begin a running-process `Frame` envelope
/// rather than a zccache `[len][version]` frame.
///
/// Returns `None` until 8 bytes are buffered (the minimum needed to
/// disambiguate). The disambiguation matches the daemon's BackendHandle
/// probe detector exactly: a first byte equal to the running-process
/// `ENVELOPE_VERSION` (1) is still treated as a zccache frame when bytes
/// 0..4 form a plausible zccache length and bytes 4..8 carry a known
/// zccache protocol version (v15/v16).
#[must_use]
pub fn buffer_starts_running_process_frame(buf: &[u8]) -> Option<bool> {
    if buf.is_empty() {
        return None;
    }
    if buf[0] != running_process::broker::protocol::ENVELOPE_VERSION {
        return Some(false);
    }
    if buf.len() < 8 {
        return None;
    }
    let zccache_len = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
    let zccache_version = u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]);
    Some(
        !(zccache_len >= 4
            && matches!(
                zccache_version,
                BINCODE_PROTOCOL_VERSION | PROST_PROTOCOL_VERSION
            )),
    )
}

/// Try to decode a zccache message carried in a running-process `Frame`
/// envelope from a byte buffer.
///
/// Returns `None` when the buffer does not yet contain a complete frame.
/// The caller must have already established (via
/// [`buffer_starts_running_process_frame`]) that the buffer starts a
/// running-process frame.
///
/// # Errors
///
/// Returns an error for oversized frames, malformed `Frame` envelopes,
/// frames whose `payload_protocol` is not [`ZCCACHE_FRAME_PAYLOAD_PROTOCOL`],
/// or payloads that fail to decode as the expected zccache message.
pub fn decode_frame_v1_message<M: Message + Default>(
    buf: &mut BytesMut,
) -> Result<Option<FrameV1Decoded<M>>, ProtocolError> {
    use running_process::broker::protocol::{Frame, FrameKind, PayloadEncoding};

    if buf.len() < 5 {
        return Ok(None);
    }
    debug_assert_eq!(buf[0], running_process::broker::protocol::ENVELOPE_VERSION);

    let body_len = u32::from_le_bytes([buf[1], buf[2], buf[3], buf[4]]) as usize;
    if body_len > running_process::broker::protocol::MAX_FRAME_BYTES {
        return Err(ProtocolError::MessageTooLarge(body_len));
    }
    if buf.len() < 5 + body_len {
        return Ok(None);
    }

    buf.advance(5);
    let body = buf.split_to(body_len);
    let frame = Frame::decode(body.as_ref())
        .map_err(|e| ProtocolError::Deserialization(format!("running-process Frame: {e}")))?;

    if frame.envelope_version != 1 {
        return Err(ProtocolError::Deserialization(format!(
            "unsupported running-process Frame envelope_version {}",
            frame.envelope_version
        )));
    }
    if frame.payload_protocol != ZCCACHE_FRAME_PAYLOAD_PROTOCOL {
        return Err(ProtocolError::Deserialization(format!(
            "running-process Frame payload_protocol {:#06X} is not the zccache payload protocol \
             {ZCCACHE_FRAME_PAYLOAD_PROTOCOL:#06X}",
            frame.payload_protocol
        )));
    }
    if !matches!(
        FrameKind::try_from(frame.kind),
        Ok(FrameKind::Request | FrameKind::Response)
    ) {
        return Err(ProtocolError::Deserialization(format!(
            "unsupported running-process Frame kind {} on the zccache lane",
            frame.kind
        )));
    }
    if PayloadEncoding::try_from(frame.payload_encoding) != Ok(PayloadEncoding::None) {
        return Err(ProtocolError::Deserialization(format!(
            "unsupported running-process Frame payload_encoding {} on the zccache lane",
            frame.payload_encoding
        )));
    }

    let message = M::decode(frame.payload.as_slice())
        .map_err(|e| ProtocolError::Deserialization(e.to_string()))?;
    Ok(Some(FrameV1Decoded {
        message,
        request_id: frame.request_id,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::wire_prost::zccache_v1;

    /// Build a fixed, deterministic zccache `Lookup` request used as the
    /// golden-bytes fixture. The contents must never change — if a field is
    /// added/reordered in `zccache_v1`, the frozen bytes below catch it.
    fn golden_request() -> zccache_v1::Request {
        zccache_v1::Request {
            body: Some(zccache_v1::request::Body::Lookup(zccache_v1::Lookup {
                cache_key: "golden-cache-key".to_string(),
            })),
            request_id: "golden-request-id".to_string(),
        }
    }

    /// Golden-bytes freeze for the `0x7A63` Frame lane (zackees/running-process#435).
    ///
    /// Encodes a fixed request and asserts the EXACT outer envelope bytes
    /// (`[u8 envelope_version][u32 LE body_len][prost Frame]`). Any change to
    /// the running-process `Frame` field layout, the zccache payload protocol
    /// id, or the `zccache_v1` request encoding will break this — which is the
    /// point: the wire is a frozen contract once consumers depend on it.
    #[test]
    fn frame_v1_request_golden_bytes_are_frozen() {
        let encoded = encode_frame_v1_request(&golden_request(), 0x0102_0304_0506_0708)
            .expect("encode golden frame");

        // FROZEN: regenerate ONLY with an intentional, documented wire bump.
        // These bytes were captured from the running-process v1 `Frame` + the
        // `zccache_v1::Request` encoder; they are the canonical contract.
        let expected: &[u8] = &[
            0x01, // outer envelope_version
            0x3A, 0x00, 0x00, 0x00, // u32 LE body_len = 58
            // ── prost Frame body ──
            0x08, 0x01, // field 1 (envelope_version) = 1
            // field 2 (kind) = Request (0) is the default and is not serialized
            0x18, 0xE3, 0xF4, 0x01, // field 3 (payload_protocol) = 0x7A63 (31331)
            0x22, 0x28, // field 4 (payload), len 40
            // ── inner zccache_v1::Request prost payload ──
            0x22, 0x12, // field 4 (Lookup body), len 18
            0x0A, 0x10, // Lookup.cache_key (field 1), len 16
            b'g', b'o', b'l', b'd', b'e', b'n', b'-', b'c', b'a', b'c', b'h', b'e', b'-', b'k',
            b'e', b'y', // "golden-cache-key"
            0xA2, 0x06, 0x11, // Request.request_id (field 100), len 17
            b'g', b'o', b'l', b'd', b'e', b'n', b'-', b'r', b'e', b'q', b'u', b'e', b's', b't',
            b'-', b'i', b'd', // "golden-request-id"
            0x28, 0x88, 0x8E, 0x98, 0xA8, 0xC0, 0xE0, 0x80, 0x81,
            0x01, // field 5 (Frame.request_id) u64 = 0x0102030405060708
        ];

        assert_eq!(
            encoded.as_ref(),
            expected,
            "0x7A63 Frame lane golden bytes changed — wire is frozen; \
             see zackees/running-process#435"
        );
    }

    /// The golden bytes must round-trip back to the original request.
    #[test]
    fn frame_v1_golden_round_trips() {
        let request_id = 0x0102_0304_0506_0708;
        let mut buf =
            encode_frame_v1_request(&golden_request(), request_id).expect("encode golden frame");

        assert_eq!(buffer_starts_running_process_frame(&buf), Some(true));

        let decoded: FrameV1Decoded<zccache_v1::Request> = decode_frame_v1_message(&mut buf)
            .expect("decode")
            .expect("complete frame");
        assert_eq!(decoded.request_id, request_id);
        assert_eq!(decoded.message, golden_request());
    }

    /// A frame whose payload_protocol is not `0x7A63` is rejected before the
    /// payload is decoded.
    #[test]
    fn frame_v1_rejects_foreign_payload_protocol() {
        use prost::Message as _;
        use running_process::broker::protocol::{Frame, FrameKind, PayloadEncoding};

        let frame = Frame {
            envelope_version: 1,
            kind: FrameKind::Request as i32,
            payload_protocol: 0x7001, // not the zccache lane
            payload: golden_request().encode_to_vec(),
            request_id: 7,
            payload_encoding: PayloadEncoding::None as i32,
            deadline_unix_ms: 0,
            traceparent: String::new(),
            tracestate: String::new(),
        };
        let body = frame.encode_to_vec();
        let mut buf = BytesMut::new();
        buf.put_u8(running_process::broker::protocol::ENVELOPE_VERSION);
        buf.put_u32_le(u32::try_from(body.len()).unwrap());
        buf.extend_from_slice(&body);

        let err = decode_frame_v1_message::<zccache_v1::Request>(&mut buf)
            .expect_err("foreign payload protocol must be rejected");
        assert!(
            matches!(err, ProtocolError::Deserialization(msg) if msg.contains("payload_protocol")),
            "expected a payload_protocol rejection"
        );
    }
}
