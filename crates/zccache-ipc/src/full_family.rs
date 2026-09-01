//! Prost full-family request roundtrips.

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
/// Unset/`auto` selects prost. Frame is available only by explicit selection.
///
/// # Errors
///
/// Returns the IPC error from the selected send/receive path, including an
/// unsupported `ZCCACHE_DAEMON_WIRE` value.
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
/// Returns the IPC error together with its delivery phase.
pub async fn full_family_roundtrip_classified(
    endpoint: &str,
    request: &protocol::Request,
    recv_timeout: Option<std::time::Duration>,
) -> Result<Option<Response>, FullFamilyRoundtripFailure> {
    let selection = wire_prost::full_family_wire_selection_from_env().map_err(|error| {
        FullFamilyRoundtripFailure {
            phase: FullFamilyFailurePhase::PreDispatch,
            error: IpcError::Endpoint(error),
        }
    })?;
    full_family_roundtrip_with_selection_classified(endpoint, request, recv_timeout, selection)
        .await
}

async fn full_family_roundtrip_with_selection_classified(
    endpoint: &str,
    request: &protocol::Request,
    recv_timeout: Option<std::time::Duration>,
    selection: wire_prost::ClientWireSelection,
) -> Result<Option<Response>, FullFamilyRoundtripFailure> {
    send_full_family_classified(
        endpoint,
        request,
        recv_timeout,
        selection.preferred_format(),
    )
    .await
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
