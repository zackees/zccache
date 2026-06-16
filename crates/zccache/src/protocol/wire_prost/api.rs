//! Narrow daemon-control and maintenance API helpers over the v16 prost wire.
//!
//! These wrappers gate the full enum converters in [`super::request`] /
//! [`super::response`] to the subset of `Request`/`Response` variants that
//! today's prost control roundtrip helper accepts, and centralize the small
//! glue (`default_request_id`, `full_family_wire_format_from_env`,
//! `response_from_decoded_wire`) that ties the prost lane to the dual-wire
//! dispatcher.

use super::zccache_v1;
use super::{
    client_wire_selection_from_env, request_from_prost, request_to_prost, response_from_prost,
    response_to_prost, ClientWireSelection, WireFormat, WIRE_FORMAT_ENV,
};

/// Convert v16 prost daemon-control and maintenance requests, rejecting
/// non-control bodies so the control roundtrip helper keeps its narrow scope.
///
/// # Errors
///
/// Returns a clear diagnostic for missing, malformed, or non-control request
/// bodies. The caller should surface this as a daemon response instead of
/// dropping the connection.
pub fn supported_control_request_from_prost(
    request: zccache_v1::Request,
) -> Result<crate::protocol::Request, String> {
    use zccache_v1::request::Body;

    match &request.body {
        Some(
            Body::Ping(_)
            | Body::Status(_)
            | Body::Shutdown(_)
            | Body::Clear(_)
            | Body::ReleaseWorktreeHandles(_),
        ) => request_from_prost(request),
        Some(other) => Err(format!(
            "unsupported v16 prost control request body {other:?}; only Ping, Status, Shutdown, \
             Clear, and ReleaseWorktreeHandles may use the prost control request path"
        )),
        None => Err(
            "unsupported v16 prost request: missing request body; only Ping, Status, Shutdown, \
             Clear, and ReleaseWorktreeHandles may use the prost control request path"
                .to_string(),
        ),
    }
}

/// Convert the narrow daemon-control and maintenance request slice to the v16
/// prost schema.
///
/// # Errors
///
/// Returns a clear diagnostic when a caller tries to route an unsupported
/// request through the prost control path.
pub fn supported_control_request_to_prost(
    request: &crate::protocol::Request,
) -> Result<zccache_v1::Request, String> {
    match request {
        crate::protocol::Request::Ping
        | crate::protocol::Request::Status
        | crate::protocol::Request::Shutdown
        | crate::protocol::Request::Clear
        | crate::protocol::Request::ReleaseWorktreeHandles { .. } => {
            Ok(request_to_prost(request, default_request_id(request)))
        }
        other => Err(format!(
            "unsupported v16 prost control request {other:?}; only Ping, Status, Shutdown, \
             Clear, and ReleaseWorktreeHandles may select {WIRE_FORMAT_ENV} through the prost \
             control request path"
        )),
    }
}

/// Convert v16 prost daemon-control and maintenance responses, rejecting
/// non-control bodies so the control roundtrip helper keeps its narrow scope.
///
/// # Errors
///
/// Returns a clear diagnostic for non-control response bodies or missing
/// nested fields in the supported `Status` response body.
pub fn supported_control_response_from_prost(
    response: zccache_v1::Response,
) -> Result<crate::protocol::Response, String> {
    use zccache_v1::response::Body;

    match &response.body {
        Some(
            Body::Pong(_)
            | Body::ShuttingDown(_)
            | Body::Status(_)
            | Body::Cleared(_)
            | Body::Error(_)
            | Body::ReleaseWorktreeHandlesResult(_),
        ) => response_from_prost(response),
        Some(other) => Err(format!(
            "unsupported v16 prost control response body {other:?}; only Pong, Status, \
             ShuttingDown, Cleared, Error, and ReleaseWorktreeHandlesResult may use the prost \
             control response path"
        )),
        None => Err(
            "unsupported v16 prost response: missing response body; only Pong, Status, \
             ShuttingDown, Cleared, Error, and ReleaseWorktreeHandlesResult may use the prost \
             control response path"
                .to_string(),
        ),
    }
}

/// Convert the narrow daemon-control and maintenance response slice to the v16
/// prost schema.
///
/// # Errors
///
/// Returns a clear diagnostic when a caller tries to route an unsupported
/// response through the prost control path.
pub fn supported_control_response_to_prost(
    response: &crate::protocol::Response,
    request_id: &str,
) -> Result<zccache_v1::Response, String> {
    match response {
        crate::protocol::Response::Pong
        | crate::protocol::Response::ShuttingDown
        | crate::protocol::Response::Status(_)
        | crate::protocol::Response::Cleared { .. }
        | crate::protocol::Response::Error { .. }
        | crate::protocol::Response::ReleaseWorktreeHandlesResult { .. } => {
            Ok(response_to_prost(response, request_id))
        }
        other => Err(format!(
            "unsupported v16 prost control response {other:?}; only Pong, Status, \
             ShuttingDown, Cleared, Error, and ReleaseWorktreeHandlesResult may use the prost \
             control response path"
        )),
    }
}

/// Canonical request id used when a request is routed over the v16 prost lane
/// without a caller-supplied id.
#[must_use]
pub const fn default_request_id(request: &crate::protocol::Request) -> &'static str {
    match request {
        crate::protocol::Request::Ping => "control-ping",
        crate::protocol::Request::Status => "control-status",
        crate::protocol::Request::Shutdown => "control-shutdown",
        crate::protocol::Request::Clear => "control-clear",
        crate::protocol::Request::ReleaseWorktreeHandles { .. } => {
            "control-release-worktree-handles"
        }
        crate::protocol::Request::Lookup { .. } => "lookup",
        crate::protocol::Request::Store { .. } => "store",
        crate::protocol::Request::SessionStart { .. } => "session-start",
        crate::protocol::Request::Compile { .. } => "compile",
        crate::protocol::Request::SessionEnd { .. } => "session-end",
        crate::protocol::Request::CompileEphemeral { .. } => "compile-ephemeral",
        crate::protocol::Request::LinkEphemeral { .. } => "link-ephemeral",
        crate::protocol::Request::SessionStats { .. } => "session-stats",
        crate::protocol::Request::FingerprintCheck { .. } => "fingerprint-check",
        crate::protocol::Request::FingerprintMarkSuccess { .. } => "fingerprint-mark-success",
        crate::protocol::Request::FingerprintMarkFailure { .. } => "fingerprint-mark-failure",
        crate::protocol::Request::FingerprintInvalidate { .. } => "fingerprint-invalidate",
        crate::protocol::Request::ListRustArtifacts => "list-rust-artifacts",
        crate::protocol::Request::GenericToolExec { .. } => "generic-tool-exec",
    }
}

/// Wire family for full-message-family (non-control) client requests.
///
/// The hot wrapper, session, fingerprint, and exec client paths keep their
/// current v15 bincode default: only an explicit `ZCCACHE_DAEMON_WIRE=prost`
/// (or `=frame` for the running-process `Frame` envelope lane) opts them out
/// of bincode. `auto`/unset intentionally stays
/// bincode here (even though the control slice prefers prost under auto) so
/// the staged migration does not change default wire selection. Invalid
/// values also fall back to bincode instead of failing a build.
#[must_use]
pub fn full_family_wire_format_from_env() -> WireFormat {
    match client_wire_selection_from_env() {
        Ok(ClientWireSelection::ProstV16) => WireFormat::ProstV16,
        Ok(ClientWireSelection::FrameV1) => WireFormat::FrameV1,
        Ok(ClientWireSelection::Auto | ClientWireSelection::BincodeV15) | Err(_) => {
            WireFormat::BincodeV15
        }
    }
}

/// Convert a dual-wire decoded daemon response into the internal enum.
///
/// v15 bincode responses pass through unchanged; v16 prost responses are
/// converted via [`response_from_prost`].
///
/// # Errors
///
/// Returns a deserialization error when a v16 prost response body is missing
/// or carries malformed required fields.
pub fn response_from_decoded_wire(
    message: crate::protocol::DecodedWireMessage<crate::protocol::Response, zccache_v1::Response>,
) -> Result<crate::protocol::Response, crate::protocol::ProtocolError> {
    match message {
        crate::protocol::DecodedWireMessage::BincodeV15(response) => Ok(response),
        crate::protocol::DecodedWireMessage::ProstV16(response)
        | crate::protocol::DecodedWireMessage::FrameV1 {
            message: response, ..
        } => response_from_prost(response).map_err(crate::protocol::ProtocolError::Deserialization),
    }
}
