//! IPC transport layer.
//!
//! Provides platform-abstracted IPC using named pipes on Windows
//! and Unix domain sockets on Unix. Messages are length-prefixed
//! prost via `zccache-protocol`.
//!
//! A third lane carries zccache prost payloads inside running-process broker
//! `Frame` envelopes (`[u8 envelope_version=1][u32 LE body_len][Frame]`,
//! `payload_protocol` =
//! [`ZCCACHE_FRAME_PAYLOAD_PROTOCOL`](zccache_protocol::wire_frame::ZCCACHE_FRAME_PAYLOAD_PROTOCOL)).
//! It is selected only by an explicit `ZCCACHE_DAEMON_WIRE=frame` and shares
//! the running-process framing already used by the `BackendHandle` identity
//! probe; `recv_wire` disambiguates it from direct prost the same way
//! `try_serve_backend_handle_probe` does.

use std::time::Duration;

use bytes::BytesMut;
use tokio::io::{AsyncWriteExt, ReadHalf, WriteHalf};

use super::error::IpcError;

mod framing;
mod probe;

use framing::{decode_response_wire, recv_wire_loop};

pub type IpcClientConnection = IpcConnection;

/// Daemon IPC message convertible to and from the prost body schema.
pub trait DaemonWireMessage: Sized {
    /// Prost schema message paired with this internal message.
    type Prost: prost::Message + Default;

    /// Convert this message to its prost body.
    fn to_prost(&self) -> Self::Prost;
    /// Convert a decoded prost body to this message.
    fn from_prost(message: Self::Prost) -> Result<Self, zccache_protocol::ProtocolError>;
}

impl DaemonWireMessage for zccache_protocol::Request {
    type Prost = zccache_protocol::wire_prost::zccache_v1::Request;

    fn to_prost(&self) -> Self::Prost {
        let request_id = zccache_protocol::wire_prost::default_request_id(self);
        zccache_protocol::wire_prost::request_to_prost(self, request_id)
    }

    fn from_prost(message: Self::Prost) -> Result<Self, zccache_protocol::ProtocolError> {
        zccache_protocol::wire_prost::request_from_prost(message)
            .map_err(zccache_protocol::ProtocolError::Deserialization)
    }
}

impl DaemonWireMessage for zccache_protocol::Response {
    type Prost = zccache_protocol::wire_prost::zccache_v1::Response;

    fn to_prost(&self) -> Self::Prost {
        zccache_protocol::wire_prost::response_to_prost(self, "unpaired-response")
    }

    fn from_prost(message: Self::Prost) -> Result<Self, zccache_protocol::ProtocolError> {
        zccache_protocol::wire_prost::response_from_prost(message)
            .map_err(zccache_protocol::ProtocolError::Deserialization)
    }
}

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

type StreamType = crate::platform::ipc::Stream;

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

    /// Send a daemon request or response over the prost wire.
    pub async fn send<T: DaemonWireMessage>(&mut self, msg: &T) -> Result<(), IpcError> {
        self.send_prost(&msg.to_prost()).await
    }

    /// Send a complete frame owned by a separate, non-daemon IPC protocol.
    ///
    /// The bytes are deliberately opaque to this transport: their framing and
    /// codec remain owned by the caller. Main daemon traffic must use
    /// [`Self::send`], [`Self::send_request`], or the explicit FrameV1 APIs.
    pub async fn send_opaque_bytes(&mut self, frame: &[u8]) -> Result<(), IpcError> {
        self.writer.write_all(frame).await?;
        self.writer.flush().await?;
        Ok(())
    }

    /// Send a prost message over the v16 daemon wire.
    ///
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

    /// Receive a daemon message from the connection.
    ///
    /// Returns `None` if the connection was closed cleanly. If a default
    /// timeout has been configured via [`Self::set_recv_timeout`] and the
    /// next message does not arrive within that window, returns
    /// `Err(IpcError::Timeout(_))`.
    pub async fn recv<T: DaemonWireMessage>(&mut self) -> Result<Option<T>, IpcError> {
        match self.recv_timeout {
            Some(t) => self.recv_with_timeout(t).await,
            None => self.recv_loop().await,
        }
    }

    /// Receive a daemon message with a per-call timeout override.
    ///
    /// Independent of any default set via [`Self::set_recv_timeout`].
    pub async fn recv_with_timeout<T: DaemonWireMessage>(
        &mut self,
        timeout: Duration,
    ) -> Result<Option<T>, IpcError> {
        match tokio::time::timeout(timeout, self.recv_loop()).await {
            Ok(result) => result,
            Err(_) => Err(IpcError::Timeout(timeout)),
        }
    }

    /// Receive one message using a decoder owned by a separate, non-daemon
    /// IPC protocol.
    ///
    /// The decoder is called against the shared read buffer until it returns a
    /// complete message. Returning `Ok(None)` retains partial bytes and reads
    /// more input, exactly like the daemon wire receive paths.
    pub async fn recv_opaque_with<T, E, Decode>(
        &mut self,
        mut decode: Decode,
    ) -> Result<Option<T>, IpcError>
    where
        E: std::fmt::Display,
        Decode: FnMut(&mut BytesMut) -> Result<Option<T>, E>,
    {
        match self.recv_timeout {
            Some(timeout) => {
                match tokio::time::timeout(timeout, self.recv_opaque_with_loop(&mut decode)).await {
                    Ok(result) => result,
                    Err(_) => Err(IpcError::Timeout(timeout)),
                }
            }
            None => self.recv_opaque_with_loop(&mut decode).await,
        }
    }

    /// Receive a message using the version-dispatching daemon wire decoder.
    ///
    /// This accepts prost and FrameV1 envelopes.
    pub async fn recv_wire<Prost>(
        &mut self,
    ) -> Result<Option<zccache_protocol::DecodedWireMessage<Prost>>, IpcError>
    where
        Prost: prost::Message + Default,
    {
        match self.recv_timeout {
            Some(t) => self.recv_wire_with_timeout(t).await,
            None => self.recv_wire_loop().await,
        }
    }

    /// Receive a version-dispatched daemon wire message with a timeout.
    pub async fn recv_wire_with_timeout<Prost>(
        &mut self,
        timeout: Duration,
    ) -> Result<Option<zccache_protocol::DecodedWireMessage<Prost>>, IpcError>
    where
        Prost: prost::Message + Default,
    {
        match tokio::time::timeout(timeout, self.recv_wire_loop()).await {
            Ok(result) => result,
            Err(_) => Err(IpcError::Timeout(timeout)),
        }
    }

    /// Send a protocol [`Request`](zccache_protocol::Request) on the selected wire.
    ///
    /// `ProstV16` converts via [`wire_prost::request_to_prost`] using the canonical
    /// per-family request id and sends a v16 prost frame.
    ///
    /// [`wire_prost::request_to_prost`]: zccache_protocol::wire_prost::request_to_prost
    pub async fn send_request(
        &mut self,
        request: &zccache_protocol::Request,
        wire: zccache_protocol::wire_prost::WireFormat,
    ) -> Result<(), IpcError> {
        match wire {
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
    /// prost and running-process `Frame` envelopes.
    pub async fn recv_response(&mut self) -> Result<Option<zccache_protocol::Response>, IpcError> {
        let message = self
            .recv_wire::<zccache_protocol::wire_prost::zccache_v1::Response>()
            .await?;
        decode_response_wire(message)
    }

    /// Like [`Self::recv_response`] but with a per-call timeout override.
    pub async fn recv_response_with_timeout(
        &mut self,
        timeout: Duration,
    ) -> Result<Option<zccache_protocol::Response>, IpcError> {
        let message = self
            .recv_wire_with_timeout::<zccache_protocol::wire_prost::zccache_v1::Response>(timeout)
            .await?;
        decode_response_wire(message)
    }

    /// Receive a response with a per-call timeout while retaining the selected
    /// request wire.
    pub async fn recv_response_for_wire_with_timeout(
        &mut self,
        timeout: Duration,
        _expected: zccache_protocol::wire_prost::WireFormat,
    ) -> Result<Option<zccache_protocol::Response>, IpcError> {
        let message = self
            .recv_wire_with_timeout::<zccache_protocol::wire_prost::zccache_v1::Response>(timeout)
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
    async fn recv_loop<T: DaemonWireMessage>(&mut self) -> Result<Option<T>, IpcError> {
        let message = self.recv_wire_loop::<T::Prost>().await?;
        message
            .map(|message| match message {
                zccache_protocol::DecodedWireMessage::ProstV16(message)
                | zccache_protocol::DecodedWireMessage::FrameV1 { message, .. } => {
                    T::from_prost(message)
                }
            })
            .transpose()
            .map_err(IpcError::Protocol)
    }

    async fn recv_opaque_with_loop<T, E, Decode>(
        &mut self,
        decode: &mut Decode,
    ) -> Result<Option<T>, IpcError>
    where
        E: std::fmt::Display,
        Decode: FnMut(&mut BytesMut) -> Result<Option<T>, E>,
    {
        loop {
            if let Some(message) = decode(&mut self.read_buf)
                .map_err(|error| IpcError::OpaqueProtocol(error.to_string()))?
            {
                return Ok(Some(message));
            }
            if !framing::read_next_chunk(&mut self.reader, &mut self.read_buf).await? {
                return Ok(None);
            }
        }
    }

    async fn recv_wire_loop<Prost>(
        &mut self,
    ) -> Result<Option<zccache_protocol::DecodedWireMessage<Prost>>, IpcError>
    where
        Prost: prost::Message + Default,
    {
        recv_wire_loop(&mut self.reader, &mut self.read_buf).await
    }
}

// â”€â”€ IpcListener â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€

/// Listens for incoming local IPC connections through the platform facade.
pub struct IpcListener {
    inner: crate::platform::ipc::Listener,
}

impl IpcListener {
    /// Bind to the given endpoint and start listening.
    pub fn bind(endpoint: &str) -> Result<Self, IpcError> {
        let native = crate::platform::ipc::Endpoint::from_native(endpoint);
        let result = crate::platform::ipc::Listener::bind(&native);
        Self::finish_bind(endpoint, &native, result)
    }

    /// Bind from an asynchronous caller.
    pub async fn bind_async(endpoint: &str) -> Result<Self, IpcError> {
        let native = crate::platform::ipc::Endpoint::from_native(endpoint);
        let result = crate::platform::ipc::Listener::bind_async(&native).await;
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
        native: &crate::platform::ipc::Endpoint,
        result: std::io::Result<crate::platform::ipc::Listener>,
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
    let native = crate::platform::ipc::Endpoint::from_native(endpoint);
    let timeout = native.connect_timeout();
    let stream = tokio::time::timeout(timeout, crate::platform::ipc::connect(&native))
        .await
        .map_err(|_| {
            contextualize_connect_error(
                endpoint,
                std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    format!("connect timed out after {timeout:?}"),
                ),
            )
        })?
        .map_err(|error| contextualize_connect_error(endpoint, error))?;
    Ok(IpcConnection::from_stream(stream))
}

fn contextualize_connect_error(endpoint: &str, error: std::io::Error) -> IpcError {
    let kind = error.kind();
    IpcError::Io(std::io::Error::new(
        kind,
        EndpointConnectError {
            endpoint: endpoint.to_owned(),
            source: error,
        },
    ))
}

#[derive(Debug)]
struct EndpointConnectError {
    endpoint: String,
    source: std::io::Error,
}

impl std::fmt::Display for EndpointConnectError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "cannot connect to daemon at {}: {}",
            self.endpoint, self.source
        )
    }
}

impl std::error::Error for EndpointConnectError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

/// Generate a unique test endpoint name.
pub fn unique_test_endpoint() -> String {
    crate::platform::ipc::Endpoint::unique_test("zccache").to_string()
}

#[cfg(test)]
mod tests;
