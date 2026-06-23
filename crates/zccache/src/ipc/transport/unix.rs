//! Unix-side client connect and stream adoption for [`IpcConnection`].

use std::time::Duration;

use bytes::BytesMut;

use crate::ipc::error::IpcError;

use super::IpcConnection;

const UNIX_CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// Connect to an IPC endpoint as a client.
///
/// On Unix, returns an `IpcConnection`. On Windows, returns an
/// `IpcClientConnection` (which has the same send/recv interface).
pub async fn connect(endpoint: &str) -> Result<IpcConnection, IpcError> {
    let stream = match tokio::time::timeout(
        UNIX_CONNECT_TIMEOUT,
        tokio::net::UnixStream::connect(endpoint),
    )
    .await
    {
        Ok(result) => result?,
        Err(_) => {
            return Err(IpcError::Io(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                format!(
                    "cannot connect to daemon at {endpoint}: connect timed out after {UNIX_CONNECT_TIMEOUT:?}"
                ),
            )));
        }
    };
    Ok(IpcConnection::from_unix_stream(stream))
}

impl IpcConnection {
    /// Wrap an already-connected `UnixStream` as an `IpcConnection`.
    ///
    /// The broker lane uses this to adopt the live socket handed back by
    /// [`AsyncBrokerSession::into_backend_io`] (re-exported through
    /// `protocol_v2::client_compat` per zccache#782 slice 25) instead of
    /// re-dialing the endpoint, so the negotiated connection is reused
    /// as the data connection.
    ///
    /// [`AsyncBrokerSession::into_backend_io`]: running_process::broker::protocol_v2::client_compat::AsyncBrokerSession
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
