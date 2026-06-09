//! Prost wire helpers and v16 dispatcher scaffolding.
//!
//! The public `encode_message` / `decode_message` helpers remain v15 bincode
//! so hot-path requests keep their old wire shape. The live daemon dispatcher
//! can accept v16 prost control requests through the explicit helpers in this
//! module while the full enum conversion lands incrementally.

use bytes::{Buf, BufMut, BytesMut};
use prost::Message;

use super::{ProtocolError, BINCODE_PROTOCOL_VERSION, MAX_MESSAGE_SIZE, PROST_PROTOCOL_VERSION};

/// Generated protobuf schema for the planned zccache v1 wire.
pub mod zccache_v1 {
    include!(concat!(env!("OUT_DIR"), "/zccache.v1.rs"));
}

/// Environment variable reserved for the daemon wire migration fallback.
pub const WIRE_FORMAT_ENV: &str = "ZCCACHE_DAEMON_WIRE";

/// Supported daemon wire families.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WireFormat {
    /// Current v15 bincode body.
    BincodeV15,
    /// Planned v16 prost body.
    ProstV16,
}

impl WireFormat {
    /// Protocol version carried in the existing frame header.
    #[must_use]
    pub const fn protocol_version(self) -> u32 {
        match self {
            Self::BincodeV15 => BINCODE_PROTOCOL_VERSION,
            Self::ProstV16 => PROST_PROTOCOL_VERSION,
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
}

impl ClientWireSelection {
    /// Wire family to try first for this selection.
    #[must_use]
    pub const fn preferred_format(self) -> WireFormat {
        match self {
            Self::Auto | Self::ProstV16 => WireFormat::ProstV16,
            Self::BincodeV15 => WireFormat::BincodeV15,
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
        other => Err(format!(
            "invalid {WIRE_FORMAT_ENV}={other:?}; expected auto, bincode, or prost"
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

/// Convert the narrow set of v16 prost daemon-control requests that the live
/// dispatcher can handle before the full enum conversion lands.
///
/// # Errors
///
/// Returns a clear diagnostic for missing or unsupported request bodies. The
/// caller should surface this as a daemon response instead of dropping the
/// connection.
pub fn supported_control_request_from_prost(
    request: zccache_v1::Request,
) -> Result<super::Request, String> {
    use zccache_v1::request::Body;

    match request.body {
        Some(Body::Ping(_)) => Ok(super::Request::Ping),
        Some(Body::Status(_)) => Ok(super::Request::Status),
        Some(Body::Shutdown(_)) => Ok(super::Request::Shutdown),
        Some(other) => Err(format!(
            "unsupported v16 prost request body {other:?}; only Ping, Status, and Shutdown are \
             supported before the full zccache prost conversion lands"
        )),
        None => Err(
            "unsupported v16 prost request: missing request body; only Ping, Status, and Shutdown \
             are supported before the full zccache prost conversion lands"
                .to_string(),
        ),
    }
}

/// Convert the narrow daemon-control request slice to the v16 prost schema.
///
/// # Errors
///
/// Returns a clear diagnostic when a caller tries to route an unsupported
/// request through the prost control path.
pub fn supported_control_request_to_prost(
    request: &super::Request,
) -> Result<zccache_v1::Request, String> {
    use zccache_v1::request::Body;

    let (request_id, body) = match request {
        super::Request::Ping => ("control-ping", Body::Ping(zccache_v1::Empty {})),
        super::Request::Status => ("control-status", Body::Status(zccache_v1::Empty {})),
        super::Request::Shutdown => ("control-shutdown", Body::Shutdown(zccache_v1::Empty {})),
        other => {
            return Err(format!(
                "unsupported v16 prost control request {other:?}; only Ping, Status, and Shutdown \
                 may select {WIRE_FORMAT_ENV} before the full zccache prost conversion lands"
            ));
        }
    };

    Ok(zccache_v1::Request {
        body: Some(body),
        request_id: request_id.to_string(),
    })
}

/// Convert the narrow daemon-control response slice from the v16 prost schema
/// back to the local protocol enum.
///
/// # Errors
///
/// Returns a clear diagnostic for unsupported response bodies or missing nested
/// fields in the supported `Status` response body.
pub fn supported_control_response_from_prost(
    response: zccache_v1::Response,
) -> Result<super::Response, String> {
    use zccache_v1::response::Body;

    match response.body {
        Some(Body::Pong(_)) => Ok(super::Response::Pong),
        Some(Body::ShuttingDown(_)) => Ok(super::Response::ShuttingDown),
        Some(Body::Status(status)) => daemon_status_from_prost(status).map(super::Response::Status),
        Some(Body::Error(error)) => Ok(super::Response::Error {
            message: error.message,
        }),
        Some(other) => Err(format!(
            "unsupported v16 prost response body {other:?}; only Pong, Status, ShuttingDown, and \
             Error are supported before the full zccache prost conversion lands"
        )),
        None => Err(
            "unsupported v16 prost response: missing response body; only Pong, Status, \
             ShuttingDown, and Error are supported before the full zccache prost conversion lands"
                .to_string(),
        ),
    }
}

/// Convert the narrow daemon-control response slice to the v16 prost schema.
///
/// # Errors
///
/// Returns a clear diagnostic when a caller tries to route an unsupported
/// response through the prost control path.
pub fn supported_control_response_to_prost(
    response: &super::Response,
    request_id: &str,
) -> Result<zccache_v1::Response, String> {
    use zccache_v1::response::Body;

    let body = match response {
        super::Response::Pong => Body::Pong(zccache_v1::Empty {}),
        super::Response::ShuttingDown => Body::ShuttingDown(zccache_v1::Empty {}),
        super::Response::Status(status) => Body::Status(daemon_status_to_prost(status)),
        super::Response::Error { message } => Body::Error(zccache_v1::Error {
            message: message.clone(),
        }),
        other => {
            return Err(format!(
                "unsupported v16 prost control response {other:?}; only Pong, Status, \
                 ShuttingDown, and Error may use the prost control response path before the full \
                 zccache prost conversion lands"
            ));
        }
    };

    Ok(zccache_v1::Response {
        body: Some(body),
        request_id: request_id.to_string(),
    })
}

fn daemon_status_to_prost(status: &super::DaemonStatus) -> zccache_v1::DaemonStatus {
    zccache_v1::DaemonStatus {
        version: status.version.clone(),
        daemon_namespace: status.daemon_namespace.clone(),
        endpoint: status.endpoint.clone(),
        private_daemon: Some(private_daemon_status_to_prost(&status.private_daemon)),
        artifact_count: status.artifact_count,
        cache_size_bytes: status.cache_size_bytes,
        metadata_entries: status.metadata_entries,
        uptime_secs: status.uptime_secs,
        cache_hits: status.cache_hits,
        cache_misses: status.cache_misses,
        total_compilations: status.total_compilations,
        non_cacheable: status.non_cacheable,
        compile_errors: status.compile_errors,
        compile_errors_cached: status.compile_errors_cached,
        time_saved_ms: status.time_saved_ms,
        total_links: status.total_links,
        link_hits: status.link_hits,
        link_misses: status.link_misses,
        link_non_cacheable: status.link_non_cacheable,
        dep_graph_contexts: status.dep_graph_contexts,
        dep_graph_files: status.dep_graph_files,
        sessions_total: status.sessions_total,
        sessions_active: status.sessions_active,
        cache_dir: Some(path_to_prost(&status.cache_dir)),
        dep_graph_version: status.dep_graph_version,
        dep_graph_disk_size: status.dep_graph_disk_size,
        dep_graph_persisted: status.dep_graph_persisted,
    }
}

fn daemon_status_from_prost(
    status: zccache_v1::DaemonStatus,
) -> Result<super::DaemonStatus, String> {
    Ok(super::DaemonStatus {
        version: status.version,
        daemon_namespace: status.daemon_namespace,
        endpoint: status.endpoint,
        private_daemon: private_daemon_status_from_prost(required_prost_field(
            status.private_daemon,
            "DaemonStatus.private_daemon",
        )?),
        artifact_count: status.artifact_count,
        cache_size_bytes: status.cache_size_bytes,
        metadata_entries: status.metadata_entries,
        uptime_secs: status.uptime_secs,
        cache_hits: status.cache_hits,
        cache_misses: status.cache_misses,
        total_compilations: status.total_compilations,
        non_cacheable: status.non_cacheable,
        compile_errors: status.compile_errors,
        compile_errors_cached: status.compile_errors_cached,
        time_saved_ms: status.time_saved_ms,
        total_links: status.total_links,
        link_hits: status.link_hits,
        link_misses: status.link_misses,
        link_non_cacheable: status.link_non_cacheable,
        dep_graph_contexts: status.dep_graph_contexts,
        dep_graph_files: status.dep_graph_files,
        sessions_total: status.sessions_total,
        sessions_active: status.sessions_active,
        cache_dir: path_from_prost(required_prost_field(
            status.cache_dir,
            "DaemonStatus.cache_dir",
        )?),
        dep_graph_version: status.dep_graph_version,
        dep_graph_disk_size: status.dep_graph_disk_size,
        dep_graph_persisted: status.dep_graph_persisted,
    })
}

fn private_daemon_status_to_prost(
    status: &super::PrivateDaemonStatus,
) -> zccache_v1::PrivateDaemonStatus {
    zccache_v1::PrivateDaemonStatus {
        enabled: status.enabled,
        owners: status
            .owners
            .iter()
            .map(|owner| zccache_v1::PrivateDaemonOwnerStatus {
                pid: owner.pid,
                ref_count: owner.ref_count,
            })
            .collect(),
        private_env_keys: status.private_env_keys.clone(),
    }
}

fn private_daemon_status_from_prost(
    status: zccache_v1::PrivateDaemonStatus,
) -> super::PrivateDaemonStatus {
    super::PrivateDaemonStatus {
        enabled: status.enabled,
        owners: status
            .owners
            .into_iter()
            .map(|owner| super::PrivateDaemonOwnerStatus {
                pid: owner.pid,
                ref_count: owner.ref_count,
            })
            .collect(),
        private_env_keys: status.private_env_keys,
    }
}

fn path_to_prost(path: &crate::core::NormalizedPath) -> zccache_v1::Path {
    zccache_v1::Path {
        value: path.as_path().to_string_lossy().into_owned(),
    }
}

fn path_from_prost(path: zccache_v1::Path) -> crate::core::NormalizedPath {
    crate::core::NormalizedPath::from(path.value)
}

fn required_prost_field<T>(value: Option<T>, field: &str) -> Result<T, String> {
    value.ok_or_else(|| format!("missing required v16 prost control response field {field}"))
}

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
