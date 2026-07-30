//! Unix-side client connect and stream adoption for [`IpcConnection`].

use std::time::Duration;

use bytes::BytesMut;

use crate::error::IpcError;

use super::IpcConnection;

const UNIX_CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// Why an accepted connection was refused before it could send a request.
///
/// The connectable `Request` surface is process execution as the daemon user
/// (#1171), so "who is on the other end" is a security decision, not
/// diagnostics. Both variants are rejections: if the kernel cannot tell us the
/// peer's identity we cannot claim the peer is us, and failing open there would
/// hand the whole control back to whatever made the lookup fail.
#[derive(Debug)]
pub(super) enum PeerRejection {
    /// The peer authenticated as a different local user.
    ForeignUid { peer_uid: u32, self_uid: u32 },
    /// The kernel would not report the peer's credentials.
    Unknown(std::io::Error),
}

impl PeerRejection {
    /// Stable `reason` field for the lifecycle event.
    pub(super) fn reason(&self) -> &'static str {
        match self {
            PeerRejection::ForeignUid { .. } => "foreign-uid",
            PeerRejection::Unknown(_) => "peer-cred-unavailable",
        }
    }

    /// Human-readable detail for the log line and the event payload.
    pub(super) fn detail(&self) -> String {
        match self {
            PeerRejection::ForeignUid { peer_uid, self_uid } => {
                format!("peer uid {peer_uid} is not the daemon uid {self_uid}")
            }
            PeerRejection::Unknown(err) => format!("peer credentials unavailable: {err}"),
        }
    }
}

/// Effective uid of this process — the identity every peer must match.
pub(super) fn self_uid() -> u32 {
    // SAFETY: `geteuid` takes no arguments, cannot fail, and touches no memory.
    unsafe { libc::geteuid() }
}

/// Verify that an accepted connection comes from the daemon's own user.
///
/// `self_uid` is injected rather than read inside so the rejection path is
/// testable without a second real user on the host: a test binds, connects to
/// itself, and passes a uid the peer cannot possibly have.
///
/// This is defense-in-depth behind the `0700` socket directory. The directory
/// mode is the load-bearing control on macOS/BSD, where the kernel ignores mode
/// bits on `connect()`; this check is what makes "same-user only" an assertion
/// the daemon itself enforces rather than a property of where the socket lives.
pub(super) fn verify_peer_is_self(
    stream: &tokio::net::UnixStream,
    self_uid: u32,
) -> Result<(), PeerRejection> {
    let cred = stream.peer_cred().map_err(PeerRejection::Unknown)?;
    let peer_uid = cred.uid();
    if peer_uid == self_uid {
        Ok(())
    } else {
        Err(PeerRejection::ForeignUid { peer_uid, self_uid })
    }
}

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
