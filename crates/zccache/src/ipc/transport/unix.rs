//! Unix-side client connect and stream adoption for [`IpcConnection`].

use bytes::BytesMut;

use crate::ipc::error::IpcError;

use super::IpcConnection;

/// Connect to an IPC endpoint as a client.
///
/// On Unix, returns an `IpcConnection`. On Windows, returns an
/// `IpcClientConnection` (which has the same send/recv interface).
pub async fn connect(endpoint: &str) -> Result<IpcConnection, IpcError> {
    let stream = tokio::net::UnixStream::connect(endpoint).await?;
    Ok(IpcConnection::from_unix_stream(stream))
}

impl IpcConnection {
    /// Wrap an already-connected `UnixStream` as an `IpcConnection`.
    ///
    /// The broker lane uses this to adopt the live socket handed back by
    /// `running_process::broker::adopt::AsyncBrokerSession::into_backend_io`
    /// instead of re-dialing the endpoint, so the negotiated connection is
    /// reused as the data connection.
    pub fn from_unix_stream(stream: tokio::net::UnixStream) -> Self {
        let (reader, writer) = tokio::io::split(stream);
        IpcConnection {
            reader,
            writer,
            read_buf: BytesMut::with_capacity(4096),
            recv_timeout: None,
            next_frame_request_id: 1,
        }
    }
}
