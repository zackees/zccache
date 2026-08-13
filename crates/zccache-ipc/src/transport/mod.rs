//! IPC transport layer.
//!
//! Provides platform-abstracted IPC using named pipes on Windows
//! and Unix domain sockets on Unix. Messages are length-prefixed
//! bincode via `zccache-protocol`. Explicit migration hooks can send v16 prost
//! frames and receive either v15 bincode or v16 prost frames without changing
//! the default v15 client/server path.
//!
//! A third lane carries zccache prost payloads inside running-process broker
//! `Frame` envelopes (`[u8 envelope_version=1][u32 LE body_len][Frame]`,
//! `payload_protocol` =
//! [`ZCCACHE_FRAME_PAYLOAD_PROTOCOL`](zccache_protocol::wire_frame::ZCCACHE_FRAME_PAYLOAD_PROTOCOL)).
//! It is selected only by an explicit `ZCCACHE_DAEMON_WIRE=frame` and shares
//! the running-process framing already used by the `BackendHandle` identity
//! probe; `recv_wire` disambiguates it from v15/v16 the same way
//! `try_serve_backend_handle_probe` does.

use std::time::Duration;

use bytes::BytesMut;
use tokio::io::{AsyncWriteExt, ReadHalf, WriteHalf};

use super::error::IpcError;

mod framing;
mod probe;

use framing::{decode_response_wire, recv_bincode_loop, recv_wire_loop};

pub type IpcClientConnection = IpcConnection;

/// Suggested per-recv timeout for client-side request/response IPC.
///
/// Five minutes. Covers the slowest legitimate workload â€” unity / LTO
/// builds where the daemon runs the compile inline and only responds when
/// the linker finishes â€” while still bounding the rare "daemon alive but
/// stuck" failure mode.
///
/// **This is an opt-in default; the IPC layer does not apply it on its
/// own.** Callers that want timeout enforcement must call
/// `set_recv_timeout(DEFAULT_CLIENT_RECV_TIMEOUT)` after connecting (or
/// pass a per-call value to `recv_with_timeout`). Server-side and
/// idle-style readers leave the field as `None` and keep the historical
/// unbounded behavior. Peer death is OS-detected and surfaces as
/// `IpcError::Io(_)` or `IpcError::ConnectionClosed` without involving
/// this timeout.
///
/// Five minutes is intentionally generous for Compile/Link responses. Cheap
/// daemon-health probes that need fast recovery, such as `ensure_daemon`'s
/// version `Status` probe, must override this with `recv_with_timeout` and a
/// short per-call budget. If a real workload exceeds this default, switch that
/// specific call site to `recv_with_timeout` with a longer budget rather than
/// bumping the const.
pub const DEFAULT_CLIENT_RECV_TIMEOUT: Duration = Duration::from_secs(300);

// â”€â”€ Platform-specific connection inner â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€

type StreamType = zccache_platform::ipc::Stream;

/// A bidirectional IPC connection that sends/receives protocol messages.
///
/// On Unix this wraps a `UnixStream`. On Windows this wraps a
/// `NamedPipeServer` (server-side) or `NamedPipeClient` (client-side).
/// Both sides use the same send/recv interface.
pub struct IpcConnection {
    pub(super) reader: ReadHalf<StreamType>,
    pub(super) writer: WriteHalf<StreamType>,
    pub(super) read_buf: BytesMut,
    /// Optional default timeout for `recv`. `None` means unbounded
    /// (today's historical behavior, kept for server-side and other
    /// idle-style readers). Set via `set_recv_timeout`.
    pub(super) recv_timeout: Option<Duration>,
    /// Monotonic correlation id for outgoing running-process `Frame`
    /// envelopes on the FrameV1 lane.
    pub(super) next_frame_request_id: u64,
}

// â”€â”€ IpcConnection impl (server-side on Windows, both on Unix) â”€â”€â”€â”€â”€â”€â”€

impl IpcConnection {
    fn from_stream(stream: StreamType) -> Self {
        let (reader, writer) = tokio::io::split(stream);
        Self {
            reader,
            writer,
            read_buf: BytesMut::with_capacity(4096),
            recv_timeout: None,
            next_frame_request_id: 1,
        }
    }

    /// Serve a running-process `BackendHandle` endpoint identity probe.
    ///
    /// Returns `true` when this connection was a probe and has been answered.
    /// Returns `false` after buffering enough bytes to prove the peer is using
    /// zccache's normal daemon wire; those bytes remain queued for the next
    /// `recv`/`recv_wire` call.
    pub async fn try_serve_backend_handle_probe(
        &mut self,
        daemon: &running_process::broker::protocol_v2::backend_handle::DaemonProcess,
    ) -> Result<bool, IpcError> {
        probe::try_serve_backend_handle_probe(
            &mut self.reader,
            &mut self.writer,
            &mut self.read_buf,
            daemon,
        )
        .await
    }

    /// Send a serializable message over the connection.
    pub async fn send<T: serde::Serialize>(&mut self, msg: &T) -> Result<(), IpcError> {
        let buf = zccache_protocol::encode_message(msg)?;
        self.writer.write_all(&buf).await?;
        self.writer.flush().await?;
        Ok(())
    }

    /// Send a prost message over the v16 daemon wire.
    ///
    /// This is an explicit migration hook. The default [`Self::send`] method
    /// remains v15 bincode so existing clients keep working until the daemon
    /// flips its live protocol policy.
    pub async fn send_prost<M: prost::Message>(&mut self, msg: &M) -> Result<(), IpcError> {
        let buf = zccache_protocol::wire_prost::encode_prost_message(msg)?;
        self.writer.write_all(&buf).await?;
        self.writer.flush().await?;
        Ok(())
    }

    /// Send a prost message as a running-process `Frame` request envelope.
    ///
    /// Returns the frame correlation id assigned to the request so the
    /// caller can match the daemon's echoed `request_id`.
    pub async fn send_frame_v1_request<M: prost::Message>(
        &mut self,
        msg: &M,
    ) -> Result<u64, IpcError> {
        let request_id = self.next_frame_request_id;
        self.next_frame_request_id = self.next_frame_request_id.wrapping_add(1);
        let buf = zccache_protocol::wire_frame::encode_frame_v1_request(msg, request_id)?;
        self.writer.write_all(&buf).await?;
        self.writer.flush().await?;
        Ok(request_id)
    }

    /// Send a prost message as a running-process `Frame` response envelope,
    /// echoing the client's frame correlation id.
    pub async fn send_frame_v1_response<M: prost::Message>(
        &mut self,
        msg: &M,
        request_id: u64,
    ) -> Result<(), IpcError> {
        let buf = zccache_protocol::wire_frame::encode_frame_v1_response(msg, request_id)?;
        self.writer.write_all(&buf).await?;
        self.writer.flush().await?;
        Ok(())
    }

    /// Configure the default timeout applied to subsequent `recv` calls.
    ///
    /// Until called, `recv` is unbounded (today's behavior). After this
    /// call, `recv` returns `Err(IpcError::Timeout(_))` if the next
    /// message does not arrive within `timeout`. Use this once after
    /// `connect()` on the client side to bound request/response round
    /// trips. Server-side readers should leave it unset.
    pub fn set_recv_timeout(&mut self, timeout: Duration) {
        self.recv_timeout = Some(timeout);
    }

    /// Clear the default `recv` timeout, restoring unbounded behavior.
    pub fn clear_recv_timeout(&mut self) {
        self.recv_timeout = None;
    }

    /// Current default `recv` timeout. `None` means unbounded.
    pub fn recv_timeout(&self) -> Option<Duration> {
        self.recv_timeout
    }

    /// Receive a deserializable message from the connection.
    ///
    /// Returns `None` if the connection was closed cleanly. If a default
    /// timeout has been configured via [`Self::set_recv_timeout`] and the
    /// next message does not arrive within that window, returns
    /// `Err(IpcError::Timeout(_))`.
    pub async fn recv<T: serde::de::DeserializeOwned>(&mut self) -> Result<Option<T>, IpcError> {
        match self.recv_timeout {
            Some(t) => self.recv_with_timeout(t).await,
            None => self.recv_loop().await,
        }
    }

    /// Receive a deserializable message with a per-call timeout override.
    ///
    /// Independent of any default set via [`Self::set_recv_timeout`].
    pub async fn recv_with_timeout<T: serde::de::DeserializeOwned>(
        &mut self,
        timeout: Duration,
    ) -> Result<Option<T>, IpcError> {
        match tokio::time::timeout(timeout, self.recv_loop()).await {
            Ok(result) => result,
            Err(_) => Err(IpcError::Timeout(timeout)),
        }
    }

    /// Receive a message using the version-dispatching daemon wire decoder.
    ///
    /// This accepts both v15 bincode and v16 prost frames while preserving
    /// [`Self::recv`] as the compatibility-only bincode receive path.
    pub async fn recv_wire<Bincode, Prost>(
        &mut self,
    ) -> Result<Option<zccache_protocol::DecodedWireMessage<Bincode, Prost>>, IpcError>
    where
        Bincode: serde::de::DeserializeOwned,
        Prost: prost::Message + Default,
    {
        match self.recv_timeout {
            Some(t) => self.recv_wire_with_timeout(t).await,
            None => self.recv_wire_loop().await,
        }
    }

    /// Receive a version-dispatched daemon wire message with a timeout.
    pub async fn recv_wire_with_timeout<Bincode, Prost>(
        &mut self,
        timeout: Duration,
    ) -> Result<Option<zccache_protocol::DecodedWireMessage<Bincode, Prost>>, IpcError>
    where
        Bincode: serde::de::DeserializeOwned,
        Prost: prost::Message + Default,
    {
        match tokio::time::timeout(timeout, self.recv_wire_loop()).await {
            Ok(result) => result,
            Err(_) => Err(IpcError::Timeout(timeout)),
        }
    }

    /// Send a protocol [`Request`](zccache_protocol::Request) on the selected wire.
    ///
    /// `BincodeV15` keeps the legacy [`Self::send`] frame; `ProstV16`
    /// converts via [`wire_prost::request_to_prost`] using the canonical
    /// per-family request id and sends a v16 prost frame.
    ///
    /// [`wire_prost::request_to_prost`]: zccache_protocol::wire_prost::request_to_prost
    pub async fn send_request(
        &mut self,
        request: &zccache_protocol::Request,
        wire: zccache_protocol::wire_prost::WireFormat,
    ) -> Result<(), IpcError> {
        match wire {
            zccache_protocol::wire_prost::WireFormat::BincodeV15 => self.send(request).await,
            zccache_protocol::wire_prost::WireFormat::ProstV16 => {
                let request_id = zccache_protocol::wire_prost::default_request_id(request);
                let request = zccache_protocol::wire_prost::request_to_prost(request, request_id);
                self.send_prost(&request).await
            }
            zccache_protocol::wire_prost::WireFormat::FrameV1 => {
                let request_id = zccache_protocol::wire_prost::default_request_id(request);
                let request = zccache_protocol::wire_prost::request_to_prost(request, request_id);
                self.send_frame_v1_request(&request).await.map(|_| ())
            }
        }
    }

    /// Receive a protocol [`Response`](zccache_protocol::Response), accepting
    /// v15 bincode, v16 prost, and running-process `Frame` envelopes.
    pub async fn recv_response(&mut self) -> Result<Option<zccache_protocol::Response>, IpcError> {
        let message = self
            .recv_wire::<zccache_protocol::Response, zccache_protocol::wire_prost::zccache_v1::Response>()
            .await?;
        decode_response_wire(message)
    }

    /// Like [`Self::recv_response`] but with a per-call timeout override.
    pub async fn recv_response_with_timeout(
        &mut self,
        timeout: Duration,
    ) -> Result<Option<zccache_protocol::Response>, IpcError> {
        let message = self
            .recv_wire_with_timeout::<zccache_protocol::Response, zccache_protocol::wire_prost::zccache_v1::Response>(timeout)
            .await?;
        decode_response_wire(message)
    }

    /// Resolve when the peer disconnects while the server is NOT otherwise
    /// reading a request â€” i.e. while a long-running handler (compile / link /
    /// exec) is in flight and the client is blocked awaiting the response.
    ///
    /// The server dispatch loop races this against the handler future
    /// (`tokio::select!`). If the client goes away â€” clean EOF, a killed
    /// process, a broken pipe â€” this resolves, the losing handler future is
    /// dropped, and the daemon-owned compiler [`tokio::process::Child`]
    /// (spawned with `kill_on_drop(true)`) is reaped as a side effect. Without
    /// this, the daemon parks inside the compile await, never notices the dead
    /// client, and holds its compile-concurrency permit until the child exits
    /// on its own (issue #967, meta #968).
    ///
    /// Bytes that arrive while waiting (an unexpected pipelined request) are
    /// buffered via the shared `read_next_chunk` path and the method keeps
    /// waiting â€” it resolves ONLY on disconnect, never on data. This is
    /// cancellation-safe: dropping the returned future (the common case, when
    /// the handler wins the race) leaves any buffered bytes intact in
    /// `read_buf` for the next `recv`/`recv_wire` call.
    pub async fn wait_for_disconnect(&mut self) {
        loop {
            match framing::read_next_chunk(&mut self.reader, &mut self.read_buf).await {
                // Unexpected pipelined bytes: keep them buffered for the next
                // recv and keep watching for the actual disconnect.
                Ok(true) => continue,
                // Clean EOF (Ok(false)) or broken pipe / peer death (Err(_)).
                // Either way the peer is gone.
                Ok(false) | Err(_) => return,
            }
        }
    }

    /// The recv read loop, factored out so both `recv` and
    /// `recv_with_timeout` share the same implementation. Always
    /// unbounded â€” the wrapping methods add the deadline.
    async fn recv_loop<T: serde::de::DeserializeOwned>(&mut self) -> Result<Option<T>, IpcError> {
        recv_bincode_loop(&mut self.reader, &mut self.read_buf).await
    }

    async fn recv_wire_loop<Bincode, Prost>(
        &mut self,
    ) -> Result<Option<zccache_protocol::DecodedWireMessage<Bincode, Prost>>, IpcError>
    where
        Bincode: serde::de::DeserializeOwned,
        Prost: prost::Message + Default,
    {
        recv_wire_loop(&mut self.reader, &mut self.read_buf).await
    }
}

// â”€â”€ IpcListener â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€

/// Listens for incoming local IPC connections through the platform facade.
pub struct IpcListener {
    inner: zccache_platform::ipc::Listener,
}

impl IpcListener {
    /// Bind to the given endpoint and start listening.
    pub fn bind(endpoint: &str) -> Result<Self, IpcError> {
        let native = zccache_platform::ipc::Endpoint::from_native(endpoint);
        let result = zccache_platform::ipc::Listener::bind(&native);
        Self::finish_bind(endpoint, &native, result)
    }

    /// Bind from an asynchronous caller.
    pub async fn bind_async(endpoint: &str) -> Result<Self, IpcError> {
        let native = zccache_platform::ipc::Endpoint::from_native(endpoint);
        let result = zccache_platform::ipc::Listener::bind_async(&native).await;
        Self::finish_bind(endpoint, &native, result)
    }

    /// Accept the next same-user connection.
    pub async fn accept(&mut self) -> Result<IpcConnection, IpcError> {
        loop {
            let (stream, peer) = self.inner.accept().await?;
            if let Some(reason) = peer.rejection_reason() {
                tracing::warn!(
                    event = "ipc_peer_rejected",
                    reason,
                    peer_pid = ?peer.pid(),
                    "refused an IPC connection authenticated as another user"
                );
                zccache_core::lifecycle::write_event(
                    zccache_core::lifecycle::EVENT_IPC_PEER_REJECTED,
                    serde_json::json!({
                        "reason": reason,
                        "peer_pid": peer.pid(),
                    }),
                );
                continue;
            }
            return Ok(IpcConnection::from_stream(stream));
        }
    }

    #[cfg(test)]
    #[allow(dead_code)]
    pub(crate) fn test_drain_pool(&mut self) -> usize {
        self.inner.drain_accept_pool()
    }
}

impl IpcListener {
    fn finish_bind(
        endpoint: &str,
        native: &zccache_platform::ipc::Endpoint,
        result: std::io::Result<zccache_platform::ipc::Listener>,
    ) -> Result<Self, IpcError> {
        let inner = result.map_err(|error| {
            if native.uses_file_path()
                && error.to_string().starts_with("insecure socket directory:")
            {
                emit_insecure_socket_dir(endpoint, "refused", Some(&error));
            }
            IpcError::Io(error)
        })?;
        if inner.tightened_parent() {
            emit_insecure_socket_dir(endpoint, "tightened", None);
        }
        Ok(Self { inner })
    }
}

fn emit_insecure_socket_dir(endpoint: &str, outcome: &str, error: Option<&std::io::Error>) {
    let path = std::path::Path::new(endpoint)
        .parent()
        .map(|parent| parent.display().to_string())
        .unwrap_or_else(|| endpoint.to_owned());
    tracing::warn!(event = "insecure_socket_dir", %path, outcome, error = ?error,
        "IPC endpoint directory security required attention");
    zccache_core::lifecycle::write_event(
        zccache_core::lifecycle::EVENT_INSECURE_SOCKET_DIR,
        serde_json::json!({ "path": path, "outcome": outcome, "detail": error.map(ToString::to_string) }),
    );
}

/// Connect to a local endpoint without adding a protocol round trip.
pub async fn connect(endpoint: &str) -> Result<IpcConnection, IpcError> {
    let native = zccache_platform::ipc::Endpoint::from_native(endpoint);
    let timeout = native.connect_timeout();
    let stream =
        tokio::time::timeout(timeout, zccache_platform::ipc::connect(&native))
            .await
            .map_err(|_| {
                IpcError::Io(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            format!("cannot connect to daemon at {endpoint}: connect timed out after {timeout:?}"),
        ))
            })??;
    Ok(IpcConnection::from_stream(stream))
}

/// Generate a unique test endpoint name.
pub fn unique_test_endpoint() -> String {
    zccache_platform::ipc::Endpoint::unique_test("zccache").to_string()
}

#[cfg(test)]
mod tests;
