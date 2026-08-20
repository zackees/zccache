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
    client_wire_selection_from_env_value, request_from_prost, request_to_prost,
    response_from_prost, response_to_prost, ClientWireSelection, WireFormat, WIRE_FORMAT_ENV,
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
) -> Result<crate::Request, String> {
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
    request: &crate::Request,
) -> Result<zccache_v1::Request, String> {
    match request {
        crate::Request::Ping
        | crate::Request::Status
        | crate::Request::Shutdown
        | crate::Request::Clear
        | crate::Request::ReleaseWorktreeHandles { .. } => {
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
) -> Result<crate::Response, String> {
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
    response: &crate::Response,
    request_id: &str,
) -> Result<zccache_v1::Response, String> {
    match response {
        crate::Response::Pong
        | crate::Response::ShuttingDown
        | crate::Response::Status(_)
        | crate::Response::Cleared { .. }
        | crate::Response::Error { .. }
        | crate::Response::ReleaseWorktreeHandlesResult { .. } => {
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
pub const fn default_request_id(request: &crate::Request) -> &'static str {
    match request {
        crate::Request::Ping => "control-ping",
        crate::Request::Status => "control-status",
        crate::Request::Shutdown => "control-shutdown",
        crate::Request::Clear => "control-clear",
        crate::Request::ReleaseWorktreeHandles { .. } => "control-release-worktree-handles",
        crate::Request::Lookup { .. } => "lookup",
        crate::Request::Store { .. } => "store",
        crate::Request::SessionStart { .. } => "session-start",
        crate::Request::Compile { .. } => "compile",
        crate::Request::SessionEnd { .. } => "session-end",
        crate::Request::CompileEphemeral { .. } => "compile-ephemeral",
        crate::Request::LinkEphemeral { .. } => "link-ephemeral",
        crate::Request::SessionStats { .. } => "session-stats",
        crate::Request::FingerprintCheck { .. } => "fingerprint-check",
        crate::Request::FingerprintMarkSuccess { .. } => "fingerprint-mark-success",
        crate::Request::FingerprintMarkFailure { .. } => "fingerprint-mark-failure",
        crate::Request::FingerprintInvalidate { .. } => "fingerprint-invalidate",
        crate::Request::ListRustArtifacts => "list-rust-artifacts",
        crate::Request::GenericToolExec { .. } => "generic-tool-exec",
        crate::Request::ExecProbe { .. } => "exec-probe",
        crate::Request::ExecStore { .. } => "exec-store",
    }
}

/// Wire family for full-message-family (non-control) client requests.
///
/// Unset/`auto` now selects v16 prost for wrapper, session, fingerprint, and
/// exec clients. Explicit `bincode` remains the rollout escape hatch, while
/// invalid values retain the historical build-safe bincode behavior.
#[must_use]
pub fn full_family_wire_format_from_env() -> WireFormat {
    match full_family_wire_selection_from_env() {
        ClientWireSelection::FrameV1 => WireFormat::FrameV1,
        ClientWireSelection::Auto | ClientWireSelection::ProstV16 => WireFormat::ProstV16,
        ClientWireSelection::BincodeV15 => WireFormat::BincodeV15,
    }
}

/// Full-family client policy while the default migration is staged.
///
/// Unlike the control-plane parser, invalid values retain the historical
/// fail-safe behavior and select bincode instead of breaking a compiler
/// invocation. `Auto` stays distinct so callers that own a complete
/// request/response cycle can prefer prost and perform the compatibility
/// retry before the remaining hot paths flip their default.
#[must_use]
pub fn full_family_wire_selection_from_env() -> ClientWireSelection {
    full_family_wire_selection_from_env_value(std::env::var(WIRE_FORMAT_ENV).ok().as_deref())
}

/// Value-based form of [`full_family_wire_selection_from_env`] for tests and
/// embedders that already own their environment snapshot.
#[must_use]
pub fn full_family_wire_selection_from_env_value(value: Option<&str>) -> ClientWireSelection {
    client_wire_selection_from_env_value(value).unwrap_or(ClientWireSelection::BincodeV15)
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
    message: crate::DecodedWireMessage<crate::Response, zccache_v1::Response>,
) -> Result<crate::Response, crate::ProtocolError> {
    match message {
        crate::DecodedWireMessage::BincodeV15(response) => Ok(response),
        crate::DecodedWireMessage::ProstV16(response)
        | crate::DecodedWireMessage::FrameV1 {
            message: response, ..
        } => response_from_prost(response).map_err(crate::ProtocolError::Deserialization),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_family_selection_preserves_auto_and_explicit_formats() {
        assert_eq!(
            full_family_wire_selection_from_env_value(None),
            ClientWireSelection::Auto
        );
        assert_eq!(
            full_family_wire_selection_from_env_value(Some("prost")),
            ClientWireSelection::ProstV16
        );
        assert_eq!(
            full_family_wire_selection_from_env_value(Some("frame")),
            ClientWireSelection::FrameV1
        );
        assert_eq!(
            full_family_wire_selection_from_env_value(Some("bincode")),
            ClientWireSelection::BincodeV15
        );
    }

    #[test]
    fn full_family_selection_keeps_invalid_values_build_safe() {
        assert_eq!(
            full_family_wire_selection_from_env_value(Some("not-a-wire")),
            ClientWireSelection::BincodeV15
        );
    }
}
