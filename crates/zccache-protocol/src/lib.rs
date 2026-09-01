//! IPC protocol types and serialization for zccache.
//!
//! Defines the message types exchanged between CLI/wrapper and daemon,
//! and provides serialization/deserialization using the active daemon wire.

pub mod messages;
pub mod wire_frame;
pub mod wire_prost;

pub use messages::*;

/// Wrapper stderr prefix for a cache miss that reached the maximal
/// classification fallback.
///
/// The daemon emits this exact marker and the wrapper recognizes it when
/// applying terminal-only warning color. Keeping it here prevents the two
/// sides from silently drifting.
pub const UNKNOWN_MISS_WARNING_PREFIX: &str = "zccache[warn][M]:";

/// Prost daemon wire version.
///
/// The direct prost body lane uses this value.
pub const PROST_PROTOCOL_VERSION: u32 = 24;

/// Protocol version number. Bump this when the wire format changes:
/// new/removed/reordered enum variants or struct field changes.
/// Patch releases that don't change the protocol keep the same version.
///
/// v24: `DaemonStatus` gained `index_writer_gone` (issue #1177).
/// v22: `DaemonStatus` gained watcher state (issue #1156).
/// v21: added `Response::CompileProgress` (issue #1216).
/// v19: added staged-output telemetry to the prost schema.
pub const PROTOCOL_VERSION: u32 = PROST_PROTOCOL_VERSION;

use bytes::BytesMut;
use prost::Message as ProstMessage;

/// Message decoded from a version-dispatched daemon frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecodedWireMessage<Prost> {
    /// Prost payload (historical variant name; current header version is v24).
    ProstV16(Prost),
    /// Prost payload carried inside a running-process broker `Frame`
    /// envelope. `request_id` is the frame correlation id the responder
    /// must echo back.
    FrameV1 {
        /// The zccache prost message decoded from `Frame.payload`.
        message: Prost,
        /// The `Frame.request_id` to echo in the response frame.
        request_id: u64,
    },
}

impl<Prost> DecodedWireMessage<Prost> {
    /// Wire family selected by the frame protocol-version header (or the
    /// running-process envelope byte for the `Frame` lane).
    #[must_use]
    pub const fn wire_format(&self) -> wire_prost::WireFormat {
        match self {
            Self::ProstV16(_) => wire_prost::WireFormat::ProstV16,
            Self::FrameV1 { .. } => wire_prost::WireFormat::FrameV1,
        }
    }
}

/// Try to decode a v16 prost frame or a zccache prost message carried in a
/// running-process broker `Frame` envelope.
///
/// # Errors
///
/// Returns a protocol error if the frame version is unsupported, too large, or
/// if the selected decoder cannot deserialize the payload.
pub fn decode_wire_message<Prost>(
    buf: &mut BytesMut,
) -> Result<Option<DecodedWireMessage<Prost>>, ProtocolError>
where
    Prost: ProstMessage + Default,
{
    match wire_frame::buffer_starts_running_process_frame(buf) {
        // Empty or ambiguous prefix: wait for more bytes.
        None => return Ok(None),
        Some(true) => {
            return wire_frame::decode_frame_v1_message(buf).map(|decoded| {
                decoded.map(|frame| DecodedWireMessage::FrameV1 {
                    message: frame.message,
                    request_id: frame.request_id,
                })
            });
        }
        Some(false) => {}
    }

    let Some(version) = peek_frame_protocol_version(buf)? else {
        return Ok(None);
    };

    match wire_prost::wire_format_for_protocol_version(version) {
        Some(wire_prost::WireFormat::ProstV16) => {
            wire_prost::decode_prost_message(buf).map(|msg| msg.map(DecodedWireMessage::ProstV16))
        }
        // The Frame lane has no zccache protocol-version header; it is
        // routed above via the running-process envelope byte.
        Some(wire_prost::WireFormat::FrameV1) | None => Err(ProtocolError::VersionMismatch {
            expected: PROST_PROTOCOL_VERSION,
            received: version,
        }),
    }
}

/// Read the protocol-version header without consuming the buffer.
///
/// Returns `None` until a complete frame is buffered.
///
/// # Errors
///
/// Returns an error when the announced frame length is impossible or exceeds
/// the maximum message size.
pub fn peek_frame_protocol_version(buf: &BytesMut) -> Result<Option<u32>, ProtocolError> {
    if buf.len() < 4 {
        return Ok(None);
    }

    let len = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;

    if len > MAX_MESSAGE_SIZE {
        return Err(ProtocolError::MessageTooLarge(len));
    }

    if len < 4 {
        return Err(ProtocolError::Deserialization(
            "frame too small for protocol version".into(),
        ));
    }

    if buf.len() < 4 + len {
        return Ok(None);
    }

    Ok(Some(u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]])))
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
    fn protocol_version_uses_the_prost_wire_version() {
        assert_eq!(PROTOCOL_VERSION, PROST_PROTOCOL_VERSION);
    }

    #[test]
    fn retired_v25_header_with_low_length_byte_one_is_not_a_frame_v1_envelope() {
        // The first byte is also the running-process envelope version. Keep
        // enough body bytes buffered that direct-frame version dispatch runs.
        let mut buf = BytesMut::with_capacity(4 + 0x101);
        buf.extend_from_slice(&0x101_u32.to_le_bytes());
        buf.extend_from_slice(&25_u32.to_le_bytes());
        buf.resize(4 + 0x101, 0);

        assert_eq!(
            wire_frame::buffer_starts_running_process_frame(&buf),
            Some(false)
        );
        assert!(matches!(
            decode_wire_message::<wire_prost::zccache_v1::Request>(&mut buf),
            Err(ProtocolError::VersionMismatch {
                expected: PROST_PROTOCOL_VERSION,
                received: 25,
            })
        ));
    }
}
