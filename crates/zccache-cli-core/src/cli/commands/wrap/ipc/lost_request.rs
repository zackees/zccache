//! Requests the daemon lost after dispatch with no tool left running, logged as
//! `wrapper-no-verdict` so the build tool that ran the wrapper can recover.

use super::{
    FailurePhase, RelayFailureCause, RelayFailureDiagnostic, RelayOutcome, TransportFailure,
};

/// The phase of a receive that failed after the request was sent.
pub(super) fn recv_failure_phase(error: &crate::ipc::IpcError) -> FailurePhase {
    use std::io::ErrorKind;
    match error {
        crate::ipc::IpcError::ConnectionClosed => FailurePhase::ClosedAfterDispatch,
        crate::ipc::IpcError::Io(error)
            if matches!(
                error.kind(),
                ErrorKind::ConnectionReset
                    | ErrorKind::ConnectionAborted
                    | ErrorKind::BrokenPipe
                    | ErrorKind::UnexpectedEof
            ) =>
        {
            FailurePhase::ClosedAfterDispatch
        }
        _ => FailurePhase::DeliveryUnknown,
    }
}

impl TransportFailure {
    /// Whether the daemon lost this request with no tool left running for it.
    pub(super) fn lost_request(&self) -> Option<LostRequest> {
        (self.phase == FailurePhase::ClosedAfterDispatch).then_some(LostRequest::ClosedConnection)
    }
}

impl RelayOutcome {
    /// Whether the daemon lost this request with no tool left running for it.
    pub(super) fn lost_request(&self) -> Option<LostRequest> {
        match self {
            Self::NoVerdict(RelayFailureDiagnostic {
                cause: RelayFailureCause::ClosedConnection,
                ..
            }) => Some(LostRequest::ClosedConnection),
            _ => None,
        }
    }
}

/// A request the daemon lost with no tool left running for it, so a build
/// tool may run that tool again directly. The wrapper still exits 1 and logs a
/// `wrapper-no-verdict` event stamped with its pid.
///
/// A connection the daemon closed or reset after dispatch, with no response
/// or mid-response, drops the daemon's handler, whose `kill_on_drop` session
/// kills the tool, and the tool's owner-death containment kills it when the
/// daemon itself died. A wedged daemon counts only once
/// [`stop_wedged_daemon`](crate::cli::runtime::stop_wedged_daemon) confirmed
/// it exited. A daemon error, an unexpected response or a busy daemon past its
/// wedge budget is not lost: the tool may have run, or may still be running.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum LostRequest {
    ClosedConnection,
    WedgedDaemonKilled,
}

impl LostRequest {
    /// `stop_wedged_daemon` returns the pid of a daemon it confirmed exited;
    /// the wrapper checks that pid is gone itself before the build replays.
    /// A recycled pid only reads alive, which keeps the failure.
    pub(super) fn after_wedge_stop(
        stopped: Option<u32>,
        is_alive: impl Fn(u32) -> bool,
    ) -> Option<Self> {
        stopped
            .filter(|pid| !is_alive(*pid))
            .map(|_| Self::WedgedDaemonKilled)
    }

    pub(super) const fn as_str(self) -> &'static str {
        match self {
            Self::ClosedConnection => "closed-connection",
            Self::WedgedDaemonKilled => "wedged-daemon-killed",
        }
    }
}

/// The `wrapper-no-verdict` fields for a request the daemon lost.
pub(super) fn lost_request_event(endpoint: &str, lost: LostRequest) -> serde_json::Value {
    serde_json::json!({
        "endpoint": endpoint,
        "cause": lost.as_str(),
        // `ExitCode::FAILURE`, which every lost request still returns.
        "exit_code": 1,
    })
}

/// Log a lost request; the envelope stamps it with this wrapper's pid.
pub(super) fn record_lost_request(endpoint: &str, lost: Option<LostRequest>) {
    if let Some(lost) = lost {
        crate::core::lifecycle::write_event(
            crate::core::lifecycle::EVENT_WRAPPER_NO_VERDICT,
            lost_request_event(endpoint, lost),
        );
    }
}
