//! Prost wire helpers and v16 dispatcher scaffolding.
//!
//! The public `encode_message` / `decode_message` helpers remain v15 bincode
//! so hot-path requests keep their old wire shape. The live daemon dispatcher
//! can accept v16 prost control requests through the explicit helpers in this
//! module while the full enum conversion lands incrementally.
//!
//! A third wire lane carries zccache prost payloads inside running-process
//! broker `Frame` envelopes (`[u8 envelope_version=1][u32 LE body_len][Frame]`,
//! see [`encode_frame_v1_request`] / [`decode_frame_v1_message`]). It is
//! selected only by an explicit `ZCCACHE_DAEMON_WIRE=frame`; `auto` never
//! prefers it.

use super::{BINCODE_PROTOCOL_VERSION, PROST_PROTOCOL_VERSION};

/// Generated protobuf schema for the planned zccache v1 wire.
pub mod zccache_v1 {
    include!(concat!(env!("OUT_DIR"), "/zccache.v1.rs"));
}

mod api;
mod convert;
mod frame;
mod request;
mod response;

// Re-export the running-process Frame envelope lane so callers can keep
// addressing the whole daemon-wire surface through `wire_prost`.
pub use super::wire_frame::{
    buffer_starts_running_process_frame, decode_frame_v1_message, encode_frame_v1_request,
    encode_frame_v1_response, FrameV1Decoded, ZCCACHE_FRAME_PAYLOAD_PROTOCOL,
};

pub use api::{
    default_request_id, full_family_wire_format_from_env, response_from_decoded_wire,
    supported_control_request_from_prost, supported_control_request_to_prost,
    supported_control_response_from_prost, supported_control_response_to_prost,
};
pub use frame::{decode_prost_message, encode_prost_message};
pub use request::{request_from_prost, request_to_prost};
pub use response::{response_from_prost, response_to_prost};

/// Environment variable reserved for the daemon wire migration fallback.
pub const WIRE_FORMAT_ENV: &str = "ZCCACHE_DAEMON_WIRE";

/// Supported daemon wire families.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WireFormat {
    /// Current v15 bincode body.
    BincodeV15,
    /// Planned v16 prost body.
    ProstV16,
    /// zccache prost payloads inside running-process broker `Frame`
    /// envelopes: `[u8 envelope_version=1][u32 LE body_len][Frame]` with
    /// `payload_protocol` = [`ZCCACHE_FRAME_PAYLOAD_PROTOCOL`] and the raw
    /// prost-encoded `zccache_v1` message as the opaque payload (no inner
    /// `[len][version]` header).
    FrameV1,
}

impl WireFormat {
    /// Protocol version carried in the zccache `[len][version]` frame header.
    ///
    /// Returns `None` for [`Self::FrameV1`], which has no inner zccache
    /// header — it is identified by the running-process envelope byte plus
    /// the `Frame.payload_protocol` field instead.
    #[must_use]
    pub const fn protocol_version(self) -> Option<u32> {
        match self {
            Self::BincodeV15 => Some(BINCODE_PROTOCOL_VERSION),
            Self::ProstV16 => Some(PROST_PROTOCOL_VERSION),
            Self::FrameV1 => None,
        }
    }
}

/// Planned default for new clients once the live transport uses the dispatcher.
pub const DEFAULT_CLIENT_WIRE_FORMAT: WireFormat = WireFormat::ProstV16;

/// Client-side wire selection policy from `ZCCACHE_DAEMON_WIRE`.
///
/// `Auto` preserves the user's unset/auto intent so control-request callers
/// can prefer prost while still retrying v15 bincode against older daemons.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClientWireSelection {
    /// Prefer prost and allow a bincode retry on a clear protocol mismatch.
    Auto,
    /// Force the v15 bincode compatibility path.
    BincodeV15,
    /// Force the v16 prost path.
    ProstV16,
    /// Force the running-process `Frame` envelope path. Forced-only:
    /// `Auto` never prefers this lane.
    FrameV1,
}

impl ClientWireSelection {
    /// Wire family to try first for this selection.
    #[must_use]
    pub const fn preferred_format(self) -> WireFormat {
        match self {
            Self::Auto | Self::ProstV16 => WireFormat::ProstV16,
            Self::BincodeV15 => WireFormat::BincodeV15,
            Self::FrameV1 => WireFormat::FrameV1,
        }
    }

    /// Whether a failed prost control request may be retried as bincode.
    #[must_use]
    pub const fn allows_bincode_fallback(self) -> bool {
        matches!(self, Self::Auto)
    }
}

/// Return the wire family for a protocol-version header.
#[must_use]
pub const fn wire_format_for_protocol_version(version: u32) -> Option<WireFormat> {
    match version {
        BINCODE_PROTOCOL_VERSION => Some(WireFormat::BincodeV15),
        PROST_PROTOCOL_VERSION => Some(WireFormat::ProstV16),
        _ => None,
    }
}

/// Parse a `ZCCACHE_DAEMON_WIRE` value.
///
/// `None` and `auto` model the migration target: new clients prefer v16 prost,
/// while `bincode` remains the explicit v15 fallback spelling. Use
/// [`client_wire_selection_from_env_value`] when callers need to distinguish
/// auto from forced prost.
///
/// # Errors
///
/// Returns a message suitable for diagnostics when the value is not recognized.
pub fn wire_format_from_env_value(value: Option<&str>) -> Result<WireFormat, String> {
    client_wire_selection_from_env_value(value).map(ClientWireSelection::preferred_format)
}

/// Read `ZCCACHE_DAEMON_WIRE` from the process environment.
///
/// # Errors
///
/// Returns a message suitable for diagnostics when the value is not recognized.
pub fn wire_format_from_env() -> Result<WireFormat, String> {
    wire_format_from_env_value(std::env::var(WIRE_FORMAT_ENV).ok().as_deref())
}

/// Parse `ZCCACHE_DAEMON_WIRE` while preserving unset/auto as a distinct
/// selection for compatibility fallbacks.
///
/// # Errors
///
/// Returns a message suitable for diagnostics when the value is not recognized.
pub fn client_wire_selection_from_env_value(
    value: Option<&str>,
) -> Result<ClientWireSelection, String> {
    let Some(value) = value else {
        return Ok(ClientWireSelection::Auto);
    };

    match value.trim().to_ascii_lowercase().as_str() {
        "" | "auto" => Ok(ClientWireSelection::Auto),
        "bincode" | "bincode-v15" | "v15" => Ok(ClientWireSelection::BincodeV15),
        "prost" | "prost-v16" | "v16" => Ok(ClientWireSelection::ProstV16),
        "frame" | "frame-v1" => Ok(ClientWireSelection::FrameV1),
        other => Err(format!(
            "invalid {WIRE_FORMAT_ENV}={other:?}; expected auto, bincode, prost, or frame"
        )),
    }
}

/// Read `ZCCACHE_DAEMON_WIRE` as a client selection policy.
///
/// # Errors
///
/// Returns a message suitable for diagnostics when the value is not recognized.
pub fn client_wire_selection_from_env() -> Result<ClientWireSelection, String> {
    client_wire_selection_from_env_value(std::env::var(WIRE_FORMAT_ENV).ok().as_deref())
}
