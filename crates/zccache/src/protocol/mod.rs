//! IPC protocol types and serialization for zccache.
//!
//! Defines the message types exchanged between CLI/wrapper and daemon,
//! and provides serialization/deserialization using the active daemon wire.

pub mod messages;
pub mod wire_frame;
pub mod wire_prost;

pub use messages::*;

/// Current bincode daemon wire version.
///
/// This remains the active compatibility version until the v16 prost dispatcher
/// is wired through the IPC transport. Do not change `PROTOCOL_VERSION` to v16
/// while `encode_message` and `decode_message` still serialize bincode bodies.
pub const BINCODE_PROTOCOL_VERSION: u32 = 15;

/// Planned prost daemon wire version.
///
/// The prost schema and frame helpers use this value. A future change will make
/// the daemon dispatch v15 bincode and v16 prost frames concurrently.
pub const PROST_PROTOCOL_VERSION: u32 = 16;

/// Protocol version number. Bump this when the wire format changes:
/// new/removed/reordered enum variants or struct field changes.
/// Patch releases that don't change the protocol keep the same version.
///
/// v16 (planned): prost-encoded protobuf body. The v16 schema lives in
///                  `proto/zccache_v1.proto`; v15 bincode remains accepted
///                  during the transition.
/// v15 (current bincode): added `Request::ReleaseWorktreeHandles` /
///                  `Response::ReleaseWorktreeHandlesResult` so callers
///                  (soldr Tier 3 worktree teardown, issue #690) can ask
///                  the daemon to drop sessions and close per-session
///                  journal handles under a path prefix before the
///                  worktree is deleted.
/// v14: `SessionStart` gained private daemon options, and
///                  `DaemonStatus` gained redacted private daemon diagnostics.
/// v13: `DaemonStatus` gained `daemon_namespace` and `endpoint`
///                  for soldr/zccache daemon namespace diagnostics.
/// v12: `DaemonStatus` and `SessionStats` gained cached-error
///                  counters for rustc negative-result caching.
/// v11: `Request::GenericToolExec` gained Path A (include scan)
///                  + Path B (depfile) + `non_deterministic` +
///                  `key_args_filter` fields completing issue #272.
/// v10: added `Request::GenericToolExec` / `Response::GenericToolExecResult`
///      for arbitrary-tool caching (issue #272). New protocol types
///      `ExecOutputStreams` and `ExecCachePolicy`.
/// v9: `SessionStats` gained `phase_profile: Option<PhaseProfileSummary>`
///     so per-session aggregate phase timing reaches clients.
/// v8: `Compile` / `CompileEphemeral` gained `stdin: Vec<u8>` and
///     `ArtifactPayload` replaced `ArtifactOutput.data: Arc<Vec<u8>>`.
pub const PROTOCOL_VERSION: u32 = BINCODE_PROTOCOL_VERSION;

use bytes::{Buf, BufMut, BytesMut};
use prost::Message as ProstMessage;

/// Message decoded from a version-dispatched daemon frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecodedWireMessage<Bincode, Prost> {
    /// v15 bincode payload, kept for old clients during the migration.
    BincodeV15(Bincode),
    /// v16 prost payload.
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

impl<Bincode, Prost> DecodedWireMessage<Bincode, Prost> {
    /// Wire family selected by the frame protocol-version header (or the
    /// running-process envelope byte for the `Frame` lane).
    #[must_use]
    pub const fn wire_format(&self) -> wire_prost::WireFormat {
        match self {
            Self::BincodeV15(_) => wire_prost::WireFormat::BincodeV15,
            Self::ProstV16(_) => wire_prost::WireFormat::ProstV16,
            Self::FrameV1 { .. } => wire_prost::WireFormat::FrameV1,
        }
    }
}

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
    encode_bincode_message(msg)
}

/// Serialize a message to the v15 bincode compatibility frame.
///
/// # Errors
///
/// Returns an error if serialization fails.
pub fn encode_bincode_message<T: serde::Serialize>(msg: &T) -> Result<BytesMut, ProtocolError> {
    let payload =
        bincode::serialize(msg).map_err(|e| ProtocolError::Serialization(e.to_string()))?;
    let frame_len: u32 = (4 + payload.len())
        .try_into()
        .map_err(|_| ProtocolError::MessageTooLarge(payload.len()))?;

    let mut buf = BytesMut::with_capacity(4 + 4 + payload.len());
    buf.put_u32_le(frame_len);
    buf.put_u32_le(BINCODE_PROTOCOL_VERSION);
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
    decode_bincode_message(buf)
}

/// Try to decode a v15 bincode compatibility frame from a byte buffer.
///
/// Returns `None` if the buffer does not contain a complete message.
/// Advances the buffer past the consumed message on success.
///
/// # Errors
///
/// Returns `VersionMismatch` if the sender is not using v15 bincode.
/// Returns a deserialization error if the payload is malformed.
pub fn decode_bincode_message<T: serde::de::DeserializeOwned>(
    buf: &mut BytesMut,
) -> Result<Option<T>, ProtocolError> {
    let Some((remote_ver, payload)) = take_complete_frame(buf)? else {
        return Ok(None);
    };

    if remote_ver != BINCODE_PROTOCOL_VERSION {
        return Err(ProtocolError::VersionMismatch {
            expected: BINCODE_PROTOCOL_VERSION,
            received: remote_ver,
        });
    }

    let msg = bincode::deserialize(&payload[..])
        .map_err(|e| ProtocolError::Deserialization(e.to_string()))?;
    Ok(Some(msg))
}

/// Try to decode a v15 bincode frame, a v16 prost frame, or a zccache prost
/// message carried in a running-process broker `Frame` envelope.
///
/// The live transport still calls [`decode_message`], which keeps today's v15
/// behavior. This helper is the migration hook for the daemon dispatcher: it
/// peeks the existing protocol-version header (or the running-process
/// envelope byte, disambiguated exactly like the daemon's BackendHandle probe
/// detector) and routes to the compatible decoder without consuming
/// incomplete or unsupported frames.
///
/// # Errors
///
/// Returns a protocol error if the frame version is unsupported, too large, or
/// if the selected decoder cannot deserialize the payload.
pub fn decode_wire_message<Bincode, Prost>(
    buf: &mut BytesMut,
) -> Result<Option<DecodedWireMessage<Bincode, Prost>>, ProtocolError>
where
    Bincode: serde::de::DeserializeOwned,
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
        Some(wire_prost::WireFormat::BincodeV15) => {
            decode_bincode_message(buf).map(|msg| msg.map(DecodedWireMessage::BincodeV15))
        }
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

fn take_complete_frame(buf: &mut BytesMut) -> Result<Option<(u32, BytesMut)>, ProtocolError> {
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

    buf.advance(4);
    let mut frame = buf.split_to(len);
    let remote_ver = frame.get_u32_le();
    Ok(Some((remote_ver, frame)))
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
