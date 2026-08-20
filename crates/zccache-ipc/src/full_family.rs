//! Prost-first full-family request roundtrips with legacy fallback.

use super::{connect_control_client, IpcError, DEFAULT_CLIENT_RECV_TIMEOUT};
use zccache_protocol::{self as protocol, wire_prost, Response};

/// Point in a full-family roundtrip at which an IPC failure occurred.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FullFamilyFailurePhase {
    /// The client could not connect, so no request bytes were dispatched.
    PreDispatch,
    /// A connection existed and delivery or execution may have begun.
    DeliveryUnknown,
}

/// IPC failure retaining whether request delivery was possible.
#[derive(Debug)]
pub struct FullFamilyRoundtripFailure {
    phase: FullFamilyFailurePhase,
    error: IpcError,
}

impl FullFamilyRoundtripFailure {
    /// Failure phase used to decide whether an idempotent connect failure may be ignored.
    #[must_use]
    pub const fn phase(&self) -> FullFamilyFailurePhase {
        self.phase
    }

    /// Borrow the underlying IPC error.
    #[must_use]
    pub const fn error(&self) -> &IpcError {
        &self.error
    }

    /// Discard phase provenance and return the underlying IPC error.
    #[must_use]
    pub fn into_error(self) -> IpcError {
        self.error
    }
}

/// Send any daemon request and receive its terminal response on the selected
/// full-family wire.
///
/// Unset/`auto` prefers prost and retries exactly once over bincode only when
/// the peer explicitly reports a protocol-version mismatch. The retry uses a
/// fresh connection because an old daemon has already rejected the prost
/// frame. Forced prost/frame selections never downgrade.
///
/// # Errors
///
/// Returns the IPC error from the selected send/receive path. Invalid
/// `ZCCACHE_DAEMON_WIRE` values preserve the historical bincode behavior.
pub async fn full_family_roundtrip(
    endpoint: &str,
    request: &protocol::Request,
    recv_timeout: Option<std::time::Duration>,
) -> Result<Option<Response>, IpcError> {
    full_family_roundtrip_classified(endpoint, request, recv_timeout)
        .await
        .map_err(FullFamilyRoundtripFailure::into_error)
}

/// Send any daemon request while preserving whether an error occurred before
/// the connection was established or after delivery became possible.
///
/// # Errors
///
/// Returns the IPC error together with its delivery phase. A structured prost
/// version mismatch may still retry once over bincode in auto mode.
pub async fn full_family_roundtrip_classified(
    endpoint: &str,
    request: &protocol::Request,
    recv_timeout: Option<std::time::Duration>,
) -> Result<Option<Response>, FullFamilyRoundtripFailure> {
    let selection = wire_prost::full_family_wire_selection_from_env();
    full_family_roundtrip_with_selection_classified(endpoint, request, recv_timeout, selection)
        .await
}

#[cfg(test)]
pub(crate) async fn full_family_roundtrip_with_selection(
    endpoint: &str,
    request: &protocol::Request,
    recv_timeout: Option<std::time::Duration>,
    selection: wire_prost::ClientWireSelection,
) -> Result<Option<Response>, IpcError> {
    full_family_roundtrip_with_selection_classified(endpoint, request, recv_timeout, selection)
        .await
        .map_err(FullFamilyRoundtripFailure::into_error)
}

async fn full_family_roundtrip_with_selection_classified(
    endpoint: &str,
    request: &protocol::Request,
    recv_timeout: Option<std::time::Duration>,
    selection: wire_prost::ClientWireSelection,
) -> Result<Option<Response>, FullFamilyRoundtripFailure> {
    let first = send_full_family_classified(
        endpoint,
        request,
        recv_timeout,
        selection.preferred_format(),
    )
    .await;

    match first {
        Err(ref err)
            if selection.allows_bincode_fallback()
                && full_family_wire_mismatch_error(err.error()) =>
        {
            send_full_family_classified(
                endpoint,
                request,
                recv_timeout,
                wire_prost::WireFormat::BincodeV15,
            )
            .await
        }
        result => result,
    }
}

/// Whether a full-family receive error proves that framing was rejected
/// before request dispatch. Connection closes and generic I/O errors are
/// intentionally excluded because delivery is ambiguous for compile/link.
#[must_use]
pub const fn full_family_wire_mismatch_error(err: &IpcError) -> bool {
    matches!(
        err,
        IpcError::Protocol(protocol::ProtocolError::VersionMismatch { .. })
    )
}

async fn send_full_family_classified(
    endpoint: &str,
    request: &protocol::Request,
    recv_timeout: Option<std::time::Duration>,
    wire: wire_prost::WireFormat,
) -> Result<Option<Response>, FullFamilyRoundtripFailure> {
    let mut conn =
        connect_control_client(endpoint)
            .await
            .map_err(|error| FullFamilyRoundtripFailure {
                phase: FullFamilyFailurePhase::PreDispatch,
                error,
            })?;
    conn.send_request(request, wire)
        .await
        .map_err(|error| FullFamilyRoundtripFailure {
            phase: FullFamilyFailurePhase::DeliveryUnknown,
            error,
        })?;
    conn.recv_response_for_wire_with_timeout(
        recv_timeout.unwrap_or(DEFAULT_CLIENT_RECV_TIMEOUT),
        wire,
    )
    .await
    .map_err(|error| FullFamilyRoundtripFailure {
        phase: FullFamilyFailurePhase::DeliveryUnknown,
        error,
    })
}
