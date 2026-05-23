//! IPC error types.

/// Errors that can occur during IPC operations.
#[derive(Debug, thiserror::Error)]
pub enum IpcError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("protocol error: {0}")]
    Protocol(#[from] crate::protocol::ProtocolError),

    #[error("connection closed")]
    ConnectionClosed,

    #[error("endpoint error: {0}")]
    Endpoint(String),

    /// `recv` exceeded the configured timeout while waiting for a response.
    ///
    /// Distinct from peer-death (which surfaces as `Io` from the OS closing
    /// the socket / pipe). A `Timeout` means we successfully connected, the
    /// peer is presumably still alive, but it didn't respond in the allotted
    /// time — caller should treat this as a real fault, not as
    /// daemon-unreachable.
    #[error("recv timed out after {0:?}")]
    Timeout(std::time::Duration),
}
