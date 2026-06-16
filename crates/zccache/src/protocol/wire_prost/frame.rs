//! Length-prefixed v16 prost frame encoder and decoder.
//!
//! Format: `[4-byte LE length][4-byte LE protocol version][prost payload]`.
//! The length field covers the protocol version plus payload bytes, matching
//! the existing bincode frame envelope.

use bytes::{Buf, BufMut, BytesMut};
use prost::Message;

use crate::protocol::{ProtocolError, MAX_MESSAGE_SIZE, PROST_PROTOCOL_VERSION};

/// Serialize a prost message to the planned v16 length-prefixed frame.
///
/// Format: `[4-byte LE length][4-byte LE protocol version][prost payload]`.
/// The length field covers the protocol version plus payload bytes, matching
/// the existing bincode frame envelope.
///
/// # Errors
///
/// Returns an error if prost encoding fails or the payload exceeds the frame
/// size budget.
pub fn encode_prost_message<M: Message>(msg: &M) -> Result<BytesMut, ProtocolError> {
    let mut payload = Vec::with_capacity(msg.encoded_len());
    msg.encode(&mut payload)
        .map_err(|e| ProtocolError::Serialization(e.to_string()))?;

    let frame_len: u32 = (4 + payload.len())
        .try_into()
        .map_err(|_| ProtocolError::MessageTooLarge(payload.len()))?;

    let mut buf = BytesMut::with_capacity(4 + 4 + payload.len());
    buf.put_u32_le(frame_len);
    buf.put_u32_le(PROST_PROTOCOL_VERSION);
    buf.extend_from_slice(&payload);
    Ok(buf)
}

/// Try to decode a v16 prost message from a byte buffer.
///
/// Returns `None` when the buffer does not contain a complete frame.
///
/// # Errors
///
/// Returns a version mismatch for non-v16 frames and a deserialization error
/// for malformed prost payloads.
pub fn decode_prost_message<M: Message + Default>(
    buf: &mut BytesMut,
) -> Result<Option<M>, ProtocolError> {
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
    if remote_ver != PROST_PROTOCOL_VERSION {
        return Err(ProtocolError::VersionMismatch {
            expected: PROST_PROTOCOL_VERSION,
            received: remote_ver,
        });
    }

    M::decode(&frame[4..])
        .map(Some)
        .map_err(|e| ProtocolError::Deserialization(e.to_string()))
}
