//! IPC protocol types and serialization for zccache.
//!
//! Defines the message types exchanged between CLI/wrapper and daemon,
//! and provides serialization/deserialization using bincode.

pub mod messages;

pub use messages::*;

/// Protocol version number. Bump this when the wire format changes:
/// new/removed/reordered enum variants or struct field changes.
/// Patch releases that don't change the protocol keep the same version.
///
/// v9 (current): `SessionStats` gained `phase_profile: Option<PhaseProfileSummary>`
///                so per-session aggregate phase timing reaches clients.
/// v8: `Compile` / `CompileEphemeral` gained `stdin: Vec<u8>` and
///     `ArtifactPayload` replaced `ArtifactOutput.data: Arc<Vec<u8>>`.
pub const PROTOCOL_VERSION: u32 = 9;

use bytes::{Buf, BufMut, BytesMut};

/// Serialize a message to a length-prefixed byte buffer with protocol version.
///
/// Format: `[4-byte LE length][4-byte LE protocol version][bincode payload]`
///
/// The length field covers the protocol version + payload bytes.
///
/// # Errors
///
/// Returns an error if serialization fails.
pub fn encode_message<T: serde::Serialize>(msg: &T) -> Result<BytesMut, ProtocolError> {
    let payload =
        bincode::serialize(msg).map_err(|e| ProtocolError::Serialization(e.to_string()))?;
    let frame_len: u32 = (4 + payload.len())
        .try_into()
        .map_err(|_| ProtocolError::MessageTooLarge(payload.len()))?;

    let mut buf = BytesMut::with_capacity(4 + 4 + payload.len());
    buf.put_u32_le(frame_len);
    buf.put_u32_le(PROTOCOL_VERSION);
    buf.extend_from_slice(&payload);
    Ok(buf)
}

/// Try to decode a message from a byte buffer.
///
/// Returns `None` if the buffer does not contain a complete message.
/// Advances the buffer past the consumed message on success.
///
/// # Errors
///
/// Returns `VersionMismatch` if the sender's protocol version differs.
/// Returns a deserialization error if the payload is malformed.
pub fn decode_message<T: serde::de::DeserializeOwned>(
    buf: &mut BytesMut,
) -> Result<Option<T>, ProtocolError> {
    if buf.len() < 4 {
        return Ok(None);
    }

    let len = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;

    if len > MAX_MESSAGE_SIZE {
        return Err(ProtocolError::MessageTooLarge(len));
    }

    if buf.len() < 4 + len {
        return Ok(None);
    }

    if len < 4 {
        return Err(ProtocolError::Deserialization(
            "frame too small for protocol version".into(),
        ));
    }

    buf.advance(4);
    let frame = buf.split_to(len);

    let remote_ver = u32::from_le_bytes([frame[0], frame[1], frame[2], frame[3]]);
    if remote_ver != PROTOCOL_VERSION {
        return Err(ProtocolError::VersionMismatch {
            expected: PROTOCOL_VERSION,
            received: remote_ver,
        });
    }

    let msg = bincode::deserialize(&frame[4..])
        .map_err(|e| ProtocolError::Deserialization(e.to_string()))?;
    Ok(Some(msg))
}

/// Maximum message size (16 MB).
const MAX_MESSAGE_SIZE: usize = 16 * 1024 * 1024;

/// Protocol-level errors.
#[derive(Debug, thiserror::Error)]
pub enum ProtocolError {
    #[error("serialization error: {0}")]
    Serialization(String),

    #[error("deserialization error: {0}")]
    Deserialization(String),

    #[error("message too large: {0} bytes")]
    MessageTooLarge(usize),

    #[error(
        "protocol version mismatch: expected v{expected}, received v{received}. \
         Run `zccache stop` first."
    )]
    VersionMismatch { expected: u32, received: u32 },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_decode_roundtrip() {
        let msg = messages::Request::Ping;
        let encoded = encode_message(&msg).unwrap();
        let mut buf = BytesMut::from(&encoded[..]);
        let decoded: Option<messages::Request> = decode_message(&mut buf).unwrap();
        assert_eq!(decoded, Some(messages::Request::Ping));
        assert!(buf.is_empty());
    }

    #[test]
    fn frame_includes_protocol_version() {
        let encoded = encode_message(&messages::Request::Ping).unwrap();
        // Bytes 4..8 should be PROTOCOL_VERSION in LE
        let ver = u32::from_le_bytes([encoded[4], encoded[5], encoded[6], encoded[7]]);
        assert_eq!(ver, PROTOCOL_VERSION);
    }

    #[test]
    fn version_mismatch_returns_error() {
        let mut encoded = encode_message(&messages::Request::Ping).unwrap();
        // Overwrite protocol version with a different value
        let bad_ver: u32 = PROTOCOL_VERSION + 1;
        encoded[4..8].copy_from_slice(&bad_ver.to_le_bytes());

        let mut buf = BytesMut::from(&encoded[..]);
        let result: Result<Option<messages::Request>, _> = decode_message(&mut buf);
        assert!(matches!(result, Err(ProtocolError::VersionMismatch { .. })));
    }

    #[test]
    fn old_frame_without_protocol_version_fails() {
        // Simulate an old-format frame: [len][payload] with no protocol version.
        // Build a raw old-style frame (4-byte len + bincode payload, no proto ver).
        let payload = bincode::serialize(&messages::Request::Ping).unwrap();
        let len = payload.len() as u32;
        let mut buf = BytesMut::with_capacity(4 + payload.len());
        buf.put_u32_le(len);
        buf.extend_from_slice(&payload);

        let result: Result<Option<messages::Request>, _> = decode_message(&mut buf);
        // Either VersionMismatch (garbage proto ver) or Deserialization error —
        // either way, it must not succeed.
        assert!(
            result.is_err(),
            "old-format frame must not decode successfully"
        );
    }

    #[test]
    fn incomplete_frame_returns_none() {
        let encoded = encode_message(&messages::Request::Ping).unwrap();
        // Provide only part of the frame
        let mut buf = BytesMut::from(&encoded[..encoded.len() - 1]);
        let result: Option<messages::Request> = decode_message(&mut buf).unwrap();
        assert!(result.is_none());
    }
}
