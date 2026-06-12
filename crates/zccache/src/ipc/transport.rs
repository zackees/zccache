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
//! [`ZCCACHE_FRAME_PAYLOAD_PROTOCOL`](crate::protocol::wire_frame::ZCCACHE_FRAME_PAYLOAD_PROTOCOL)).
//! It is selected only by an explicit `ZCCACHE_DAEMON_WIRE=frame` and shares
//! the running-process framing already used by the `BackendHandle` identity
//! probe; `recv_wire` disambiguates it from v15/v16 the same way
//! `try_serve_backend_handle_probe` does.

use std::time::Duration;

use bytes::{Buf, BytesMut};
use prost::Message as _;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};

use super::error::IpcError;

/// Suggested per-recv timeout for client-side request/response IPC.
///
/// Five minutes. Covers the slowest legitimate workload — unity / LTO
/// builds where the daemon runs the compile inline and only responds when
/// the linker finishes — while still bounding the rare "daemon alive but
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

// ── Platform-specific connection inner ──────────────────────────────

#[cfg(unix)]
type StreamType = tokio::net::UnixStream;

#[cfg(windows)]
type StreamType = tokio::net::windows::named_pipe::NamedPipeServer;

/// A bidirectional IPC connection that sends/receives protocol messages.
///
/// On Unix this wraps a `UnixStream`. On Windows this wraps a
/// `NamedPipeServer` (server-side) or `NamedPipeClient` (client-side).
/// Both sides use the same send/recv interface.
pub struct IpcConnection {
    reader: tokio::io::ReadHalf<StreamType>,
    writer: tokio::io::WriteHalf<StreamType>,
    read_buf: BytesMut,
    /// Optional default timeout for `recv`. `None` means unbounded
    /// (today's historical behavior, kept for server-side and other
    /// idle-style readers). Set via `set_recv_timeout`.
    recv_timeout: Option<Duration>,
    /// Monotonic correlation id for outgoing running-process `Frame`
    /// envelopes on the FrameV1 lane.
    next_frame_request_id: u64,
}

/// Client-side IPC connection (Windows uses a different type).
#[cfg(windows)]
pub struct IpcClientConnection {
    reader: tokio::io::ReadHalf<tokio::net::windows::named_pipe::NamedPipeClient>,
    writer: tokio::io::WriteHalf<tokio::net::windows::named_pipe::NamedPipeClient>,
    read_buf: BytesMut,
    /// Optional default timeout for `recv`. See `IpcConnection::recv_timeout`.
    recv_timeout: Option<Duration>,
    /// See `IpcConnection::next_frame_request_id`.
    next_frame_request_id: u64,
}

// ── IpcConnection impl (server-side on Windows, both on Unix) ───────

impl IpcConnection {
    /// Serve a running-process `BackendHandle` endpoint identity probe.
    ///
    /// Returns `true` when this connection was a probe and has been answered.
    /// Returns `false` after buffering enough bytes to prove the peer is using
    /// zccache's normal daemon wire; those bytes remain queued for the next
    /// `recv`/`recv_wire` call.
    pub async fn try_serve_backend_handle_probe(
        &mut self,
        daemon: &running_process::broker::backend_handle::DaemonProcess,
    ) -> Result<bool, IpcError> {
        try_serve_backend_handle_probe(
            &mut self.reader,
            &mut self.writer,
            &mut self.read_buf,
            daemon,
        )
        .await
    }

    /// Send a serializable message over the connection.
    pub async fn send<T: serde::Serialize>(&mut self, msg: &T) -> Result<(), IpcError> {
        let buf = crate::protocol::encode_message(msg)?;
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
        let buf = crate::protocol::wire_prost::encode_prost_message(msg)?;
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
        let buf = crate::protocol::wire_frame::encode_frame_v1_request(msg, request_id)?;
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
        let buf = crate::protocol::wire_frame::encode_frame_v1_response(msg, request_id)?;
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
    ) -> Result<Option<crate::protocol::DecodedWireMessage<Bincode, Prost>>, IpcError>
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
    ) -> Result<Option<crate::protocol::DecodedWireMessage<Bincode, Prost>>, IpcError>
    where
        Bincode: serde::de::DeserializeOwned,
        Prost: prost::Message + Default,
    {
        match tokio::time::timeout(timeout, self.recv_wire_loop()).await {
            Ok(result) => result,
            Err(_) => Err(IpcError::Timeout(timeout)),
        }
    }

    /// Send a protocol [`Request`](crate::protocol::Request) on the selected wire.
    ///
    /// `BincodeV15` keeps the legacy [`Self::send`] frame; `ProstV16`
    /// converts via [`wire_prost::request_to_prost`] using the canonical
    /// per-family request id and sends a v16 prost frame.
    ///
    /// [`wire_prost::request_to_prost`]: crate::protocol::wire_prost::request_to_prost
    pub async fn send_request(
        &mut self,
        request: &crate::protocol::Request,
        wire: crate::protocol::wire_prost::WireFormat,
    ) -> Result<(), IpcError> {
        match wire {
            crate::protocol::wire_prost::WireFormat::BincodeV15 => self.send(request).await,
            crate::protocol::wire_prost::WireFormat::ProstV16 => {
                let request_id = crate::protocol::wire_prost::default_request_id(request);
                let request = crate::protocol::wire_prost::request_to_prost(request, request_id);
                self.send_prost(&request).await
            }
            crate::protocol::wire_prost::WireFormat::FrameV1 => {
                let request_id = crate::protocol::wire_prost::default_request_id(request);
                let request = crate::protocol::wire_prost::request_to_prost(request, request_id);
                self.send_frame_v1_request(&request).await.map(|_| ())
            }
        }
    }

    /// Receive a protocol [`Response`](crate::protocol::Response), accepting
    /// v15 bincode, v16 prost, and running-process `Frame` envelopes.
    pub async fn recv_response(&mut self) -> Result<Option<crate::protocol::Response>, IpcError> {
        let message = self
            .recv_wire::<crate::protocol::Response, crate::protocol::wire_prost::zccache_v1::Response>()
            .await?;
        decode_response_wire(message)
    }

    /// Like [`Self::recv_response`] but with a per-call timeout override.
    pub async fn recv_response_with_timeout(
        &mut self,
        timeout: Duration,
    ) -> Result<Option<crate::protocol::Response>, IpcError> {
        let message = self
            .recv_wire_with_timeout::<crate::protocol::Response, crate::protocol::wire_prost::zccache_v1::Response>(timeout)
            .await?;
        decode_response_wire(message)
    }

    /// The recv read loop, factored out so both `recv` and
    /// `recv_with_timeout` share the same implementation. Always
    /// unbounded — the wrapping methods add the deadline.
    async fn recv_loop<T: serde::de::DeserializeOwned>(&mut self) -> Result<Option<T>, IpcError> {
        recv_bincode_loop(&mut self.reader, &mut self.read_buf).await
    }

    async fn recv_wire_loop<Bincode, Prost>(
        &mut self,
    ) -> Result<Option<crate::protocol::DecodedWireMessage<Bincode, Prost>>, IpcError>
    where
        Bincode: serde::de::DeserializeOwned,
        Prost: prost::Message + Default,
    {
        recv_wire_loop(&mut self.reader, &mut self.read_buf).await
    }
}

// ── IpcClientConnection (Windows client-side) ───────────────────────

#[cfg(windows)]
impl IpcClientConnection {
    /// Send a serializable message over the connection.
    pub async fn send<T: serde::Serialize>(&mut self, msg: &T) -> Result<(), IpcError> {
        let buf = crate::protocol::encode_message(msg)?;
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
        let buf = crate::protocol::wire_prost::encode_prost_message(msg)?;
        self.writer.write_all(&buf).await?;
        self.writer.flush().await?;
        Ok(())
    }

    /// See [`IpcConnection::send_frame_v1_request`].
    pub async fn send_frame_v1_request<M: prost::Message>(
        &mut self,
        msg: &M,
    ) -> Result<u64, IpcError> {
        let request_id = self.next_frame_request_id;
        self.next_frame_request_id = self.next_frame_request_id.wrapping_add(1);
        let buf = crate::protocol::wire_frame::encode_frame_v1_request(msg, request_id)?;
        self.writer.write_all(&buf).await?;
        self.writer.flush().await?;
        Ok(request_id)
    }

    /// See [`IpcConnection::set_recv_timeout`].
    pub fn set_recv_timeout(&mut self, timeout: Duration) {
        self.recv_timeout = Some(timeout);
    }

    /// See [`IpcConnection::clear_recv_timeout`].
    pub fn clear_recv_timeout(&mut self) {
        self.recv_timeout = None;
    }

    /// See [`IpcConnection::recv_timeout`].
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
    ) -> Result<Option<crate::protocol::DecodedWireMessage<Bincode, Prost>>, IpcError>
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
    ) -> Result<Option<crate::protocol::DecodedWireMessage<Bincode, Prost>>, IpcError>
    where
        Bincode: serde::de::DeserializeOwned,
        Prost: prost::Message + Default,
    {
        match tokio::time::timeout(timeout, self.recv_wire_loop()).await {
            Ok(result) => result,
            Err(_) => Err(IpcError::Timeout(timeout)),
        }
    }

    /// See [`IpcConnection::send_request`].
    pub async fn send_request(
        &mut self,
        request: &crate::protocol::Request,
        wire: crate::protocol::wire_prost::WireFormat,
    ) -> Result<(), IpcError> {
        match wire {
            crate::protocol::wire_prost::WireFormat::BincodeV15 => self.send(request).await,
            crate::protocol::wire_prost::WireFormat::ProstV16 => {
                let request_id = crate::protocol::wire_prost::default_request_id(request);
                let request = crate::protocol::wire_prost::request_to_prost(request, request_id);
                self.send_prost(&request).await
            }
            crate::protocol::wire_prost::WireFormat::FrameV1 => {
                let request_id = crate::protocol::wire_prost::default_request_id(request);
                let request = crate::protocol::wire_prost::request_to_prost(request, request_id);
                self.send_frame_v1_request(&request).await.map(|_| ())
            }
        }
    }

    /// See [`IpcConnection::recv_response`].
    pub async fn recv_response(&mut self) -> Result<Option<crate::protocol::Response>, IpcError> {
        let message = self
            .recv_wire::<crate::protocol::Response, crate::protocol::wire_prost::zccache_v1::Response>()
            .await?;
        decode_response_wire(message)
    }

    /// See [`IpcConnection::recv_response_with_timeout`].
    pub async fn recv_response_with_timeout(
        &mut self,
        timeout: Duration,
    ) -> Result<Option<crate::protocol::Response>, IpcError> {
        let message = self
            .recv_wire_with_timeout::<crate::protocol::Response, crate::protocol::wire_prost::zccache_v1::Response>(timeout)
            .await?;
        decode_response_wire(message)
    }

    async fn recv_loop<T: serde::de::DeserializeOwned>(&mut self) -> Result<Option<T>, IpcError> {
        recv_bincode_loop(&mut self.reader, &mut self.read_buf).await
    }

    async fn recv_wire_loop<Bincode, Prost>(
        &mut self,
    ) -> Result<Option<crate::protocol::DecodedWireMessage<Bincode, Prost>>, IpcError>
    where
        Bincode: serde::de::DeserializeOwned,
        Prost: prost::Message + Default,
    {
        recv_wire_loop(&mut self.reader, &mut self.read_buf).await
    }
}

/// Decode a dual-wire response message into the internal [`Response`]
/// type, mapping prost conversion failures into [`IpcError::Protocol`].
///
/// [`Response`]: crate::protocol::Response
fn decode_response_wire(
    message: Option<
        crate::protocol::DecodedWireMessage<
            crate::protocol::Response,
            crate::protocol::wire_prost::zccache_v1::Response,
        >,
    >,
) -> Result<Option<crate::protocol::Response>, IpcError> {
    message
        .map(crate::protocol::wire_prost::response_from_decoded_wire)
        .transpose()
        .map_err(IpcError::Protocol)
}

async fn recv_bincode_loop<R, T>(
    reader: &mut R,
    read_buf: &mut BytesMut,
) -> Result<Option<T>, IpcError>
where
    R: AsyncRead + Unpin,
    T: serde::de::DeserializeOwned,
{
    loop {
        if let Some(msg) = crate::protocol::decode_message::<T>(read_buf)? {
            return Ok(Some(msg));
        }
        if !read_next_chunk(reader, read_buf).await? {
            return Ok(None);
        }
    }
}

async fn recv_wire_loop<R, Bincode, Prost>(
    reader: &mut R,
    read_buf: &mut BytesMut,
) -> Result<Option<crate::protocol::DecodedWireMessage<Bincode, Prost>>, IpcError>
where
    R: AsyncRead + Unpin,
    Bincode: serde::de::DeserializeOwned,
    Prost: prost::Message + Default,
{
    loop {
        if let Some(msg) = crate::protocol::decode_wire_message::<Bincode, Prost>(read_buf)? {
            return Ok(Some(msg));
        }
        if !read_next_chunk(reader, read_buf).await? {
            return Ok(None);
        }
    }
}

async fn read_next_chunk<R>(reader: &mut R, read_buf: &mut BytesMut) -> Result<bool, IpcError>
where
    R: AsyncRead + Unpin,
{
    let mut tmp = [0u8; 4096];
    let n = reader.read(&mut tmp).await?;
    if n == 0 {
        if read_buf.is_empty() {
            return Ok(false);
        }
        return Err(IpcError::ConnectionClosed);
    }
    read_buf.extend_from_slice(&tmp[..n]);
    Ok(true)
}

async fn try_serve_backend_handle_probe<R, W>(
    reader: &mut R,
    writer: &mut W,
    read_buf: &mut BytesMut,
    daemon: &running_process::broker::backend_handle::DaemonProcess,
) -> Result<bool, IpcError>
where
    R: AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    ensure_buffered(reader, read_buf, 8).await?;
    if read_buf.is_empty() {
        return Ok(false);
    }

    let running_process_version = running_process::broker::protocol::ENVELOPE_VERSION;
    if read_buf[0] != running_process_version {
        return Ok(false);
    }

    let zccache_len = u32::from_le_bytes([read_buf[0], read_buf[1], read_buf[2], read_buf[3]]);
    let zccache_version = u32::from_le_bytes([read_buf[4], read_buf[5], read_buf[6], read_buf[7]]);
    if zccache_len >= 4
        && matches!(
            zccache_version,
            crate::protocol::BINCODE_PROTOCOL_VERSION | crate::protocol::PROST_PROTOCOL_VERSION
        )
    {
        return Ok(false);
    }

    let body_len =
        u32::from_le_bytes([read_buf[1], read_buf[2], read_buf[3], read_buf[4]]) as usize;
    if body_len > running_process::broker::protocol::MAX_FRAME_BYTES {
        return Err(IpcError::Endpoint(format!(
            "running-process BackendHandle probe frame too large: {body_len} bytes"
        )));
    }
    ensure_buffered(reader, read_buf, 5 + body_len).await?;

    // Decode from a peek so non-probe frames (e.g. the zccache FrameV1
    // request lane, which shares this framing) stay buffered for the
    // dispatching `recv_wire` decoder.
    let frame = running_process::broker::protocol::Frame::decode(&read_buf[5..5 + body_len])
        .map_err(|err| IpcError::Endpoint(format!("BackendHandle probe decode failed: {err}")))?;
    if !is_backend_handle_probe_request(&frame) {
        if frame.payload_protocol == crate::protocol::wire_frame::ZCCACHE_FRAME_PAYLOAD_PROTOCOL {
            return Ok(false);
        }
        return Err(IpcError::Endpoint(
            "unexpected running-process frame on zccache daemon endpoint".to_string(),
        ));
    }
    read_buf.advance(5 + body_len);

    let response = backend_handle_probe_response(&frame, daemon)?;
    write_running_process_frame(writer, &response).await?;
    Ok(true)
}

async fn ensure_buffered<R>(
    reader: &mut R,
    read_buf: &mut BytesMut,
    min_len: usize,
) -> Result<(), IpcError>
where
    R: AsyncRead + Unpin,
{
    while read_buf.len() < min_len {
        if !read_next_chunk(reader, read_buf).await? {
            if read_buf.is_empty() {
                return Ok(());
            }
            return Err(IpcError::ConnectionClosed);
        }
    }
    Ok(())
}

fn is_backend_handle_probe_request(frame: &running_process::broker::protocol::Frame) -> bool {
    use running_process::broker::backend_lifecycle::probe::BACKEND_HANDLE_PROBE_PAYLOAD_PROTOCOL;
    use running_process::broker::protocol::{FrameKind, PayloadEncoding};

    frame.envelope_version == 1
        && FrameKind::try_from(frame.kind) == Ok(FrameKind::Request)
        && frame.payload_protocol == BACKEND_HANDLE_PROBE_PAYLOAD_PROTOCOL
        && PayloadEncoding::try_from(frame.payload_encoding) == Ok(PayloadEncoding::None)
        && frame.payload.len() == 32
}

fn backend_handle_probe_response(
    request: &running_process::broker::protocol::Frame,
    daemon: &running_process::broker::backend_handle::DaemonProcess,
) -> Result<running_process::broker::protocol::Frame, IpcError> {
    use running_process::broker::backend_lifecycle::probe::BACKEND_HANDLE_PROBE_PAYLOAD_PROTOCOL;
    use running_process::broker::protocol::{Frame, FrameKind, PayloadEncoding};

    let mut payload = Vec::with_capacity(32 + 128);
    payload.extend_from_slice(&request.payload);
    daemon.to_proto().encode(&mut payload).map_err(|err| {
        IpcError::Endpoint(format!("BackendHandle identity encode failed: {err}"))
    })?;

    Ok(Frame {
        envelope_version: 1,
        kind: FrameKind::Response as i32,
        payload_protocol: BACKEND_HANDLE_PROBE_PAYLOAD_PROTOCOL,
        payload,
        request_id: request.request_id,
        payload_encoding: PayloadEncoding::None as i32,
        deadline_unix_ms: 0,
        traceparent: request.traceparent.clone(),
        tracestate: request.tracestate.clone(),
    })
}

async fn write_running_process_frame<W>(
    writer: &mut W,
    frame: &running_process::broker::protocol::Frame,
) -> Result<(), IpcError>
where
    W: tokio::io::AsyncWrite + Unpin,
{
    let mut body = Vec::new();
    frame.encode(&mut body).map_err(|err| {
        IpcError::Endpoint(format!("BackendHandle response encode failed: {err}"))
    })?;
    if body.len() > running_process::broker::protocol::MAX_FRAME_BYTES {
        return Err(IpcError::Endpoint(format!(
            "BackendHandle response frame too large: {} bytes",
            body.len()
        )));
    }
    writer
        .write_all(&[running_process::broker::protocol::ENVELOPE_VERSION])
        .await?;
    writer.write_all(&(body.len() as u32).to_le_bytes()).await?;
    writer.write_all(&body).await?;
    writer.flush().await?;
    Ok(())
}

// ── IpcListener ─────────────────────────────────────────────────────

/// Listens for incoming IPC connections.
pub struct IpcListener {
    inner: ListenerInner,
}

#[cfg(unix)]
struct ListenerInner {
    listener: tokio::net::UnixListener,
}

#[cfg(windows)]
struct ListenerInner {
    endpoint: String,
    /// Pool of pre-created pipe instances waiting for clients.
    /// Multiple pending instances eliminate the busy window between
    /// accepting a connection and creating the next instance (Bug 4 fix).
    pool: std::collections::VecDeque<tokio::net::windows::named_pipe::NamedPipeServer>,
}

impl IpcListener {
    /// Bind to the given endpoint and start listening.
    pub fn bind(endpoint: &str) -> Result<Self, IpcError> {
        #[cfg(unix)]
        {
            // Remove stale socket if it exists
            let _ = std::fs::remove_file(endpoint);
            if let Some(parent) = std::path::Path::new(endpoint).parent() {
                std::fs::create_dir_all(parent)?;
            }
            let listener = tokio::net::UnixListener::bind(endpoint)?;
            Ok(Self {
                inner: ListenerInner { listener },
            })
        }
        #[cfg(windows)]
        {
            use std::collections::VecDeque;
            use tokio::net::windows::named_pipe::ServerOptions;

            // Pool sizing rationale (issue #666 follow-up, application-layer
            // back-pressure precondition): the pre-existing `.min(16)` cap
            // made the named-pipe accept queue the dominant bottleneck under
            // a ninja burst of ~670 parallel TUs. The OS layer would return
            // `ERROR_PIPE_BUSY` to clients before the daemon ever saw the
            // request, which aliased "daemon overloaded" with "daemon dead"
            // at the client. Raising the cap moves the bottleneck inside
            // the daemon where it can be expressed as an in-band
            // `Response::Backpressure` reply.
            let pool_size = std::env::var("ZCCACHE_PIPE_POOL_SIZE")
                .ok()
                .and_then(|s| s.parse::<usize>().ok())
                .unwrap_or_else(|| {
                    std::thread::available_parallelism()
                        .map(|n| n.get().saturating_mul(4))
                        .unwrap_or(64)
                        .clamp(16, 128)
                });

            let mut pool = VecDeque::with_capacity(pool_size);
            for i in 0..pool_size {
                let pipe = ServerOptions::new()
                    .first_pipe_instance(i == 0)
                    .create(endpoint)?;
                pool.push_back(pipe);
            }

            Ok(Self {
                inner: ListenerInner {
                    endpoint: endpoint.to_string(),
                    pool,
                },
            })
        }
    }

    /// Accept a new connection.
    ///
    /// On Unix, returns an `IpcConnection` wrapping a `UnixStream`.
    /// On Windows, returns an `IpcConnection` wrapping a `NamedPipeServer`.
    pub async fn accept(&mut self) -> Result<IpcConnection, IpcError> {
        #[cfg(unix)]
        {
            let (stream, _addr) = self.inner.listener.accept().await?;
            let (reader, writer) = tokio::io::split(stream);
            Ok(IpcConnection {
                reader,
                writer,
                read_buf: BytesMut::with_capacity(4096),
                recv_timeout: None,
                next_frame_request_id: 1,
            })
        }
        #[cfg(windows)]
        {
            self.accept_windows().await
        }
    }

    /// Windows named-pipe accept with pool-depletion recovery (issue #666).
    ///
    /// The pre-#666 implementation had two depletion paths:
    /// 1. `pool.pop_front().expect(...)` panicked if the pool ever emptied.
    /// 2. `ServerOptions::create(...)?` on the replacement leaked the popped
    ///    pipe slot on every failure, silently shrinking the pool.
    ///
    /// Combined, a sustained period of transient `create` errors (handle
    /// exhaustion under a 673-TU ninja burst, antivirus reentrancy, etc.)
    /// would shrink the pool toward zero and eventually wedge the daemon
    /// without crashing — every new client would queue on the dwindling
    /// number of remaining `pipe.connect().await` calls and see the
    /// CLI-side 300 s recv timeout.
    ///
    /// This implementation:
    /// - Replaces the `pop_front().expect()` panic with an emergency create.
    /// - Retries replacement creation with bounded backoff so a transient
    ///   OS hiccup does not consume a pool slot.
    /// - Treats a `connect()` failure as a normal accept retry rather than
    ///   propagating it to the daemon's main loop (which would drop the
    ///   slot the same way the old code did).
    #[cfg(windows)]
    async fn accept_windows(&mut self) -> Result<IpcConnection, IpcError> {
        loop {
            let pipe = match self.inner.pool.pop_front() {
                Some(p) => p,
                None => {
                    // Pool depleted — every prior `create` retry failed and
                    // the popped slot was never replaced. Try once more
                    // synchronously; failure here is a real OS problem and
                    // is propagated to the caller.
                    tracing::warn!(
                        endpoint = %self.inner.endpoint,
                        "named-pipe pool exhausted — attempting emergency create (issue #666)"
                    );
                    create_replacement_pipe(&self.inner.endpoint)?
                }
            };

            if let Err(e) = pipe.connect().await {
                // The popped pipe is now dropped. Replenish the pool with a
                // fresh instance (best-effort) and retry the accept so the
                // daemon's main loop never sees a transient connect glitch.
                tracing::warn!(
                    endpoint = %self.inner.endpoint,
                    error = %e,
                    "named-pipe connect failed — replenishing and retrying"
                );
                if let Ok(replacement) = create_replacement_pipe(&self.inner.endpoint) {
                    self.inner.pool.push_back(replacement);
                }
                continue;
            }

            // Connect succeeded. Try hard to create a replacement before we
            // return so the pool stays full. If replacement creation fails
            // even after retries, log loudly and proceed anyway — the next
            // accept will hit the pool-depleted branch above instead of
            // panicking.
            match create_replacement_pipe_with_retry(&self.inner.endpoint).await {
                Some(replacement) => self.inner.pool.push_back(replacement),
                None => tracing::error!(
                    endpoint = %self.inner.endpoint,
                    "named-pipe replacement create failed after retries — \
                     pool slot temporarily unfilled; next accept will recreate (issue #666)"
                ),
            }

            let (reader, writer) = tokio::io::split(pipe);
            return Ok(IpcConnection {
                reader,
                writer,
                read_buf: BytesMut::with_capacity(4096),
                recv_timeout: None,
                next_frame_request_id: 1,
            });
        }
    }

    /// Test-only: pop every pipe out of the pool to simulate a deeply
    /// depleted state. Used by `pool_recovers_from_full_depletion` to
    /// exercise the issue #666 emergency-create path.
    #[cfg(all(windows, test))]
    pub(crate) fn test_drain_pool(&mut self) -> usize {
        let drained = self.inner.pool.len();
        self.inner.pool.clear();
        drained
    }
}

/// Issue #666: synchronous replacement pipe creation. Used by the emergency
/// path when the pool is fully depleted.
#[cfg(windows)]
fn create_replacement_pipe(
    endpoint: &str,
) -> Result<tokio::net::windows::named_pipe::NamedPipeServer, IpcError> {
    use tokio::net::windows::named_pipe::ServerOptions;
    Ok(ServerOptions::new()
        .first_pipe_instance(false)
        .create(endpoint)?)
}

/// Issue #666: bounded-backoff retry around replacement creation. Returns
/// `None` only after the full retry budget is exhausted; in that case the
/// caller logs and proceeds without push-back so the pool slot is filled
/// lazily on the next accept.
///
/// The backoff is deliberately short (5 ms → 80 ms over 5 attempts, ~155 ms
/// total worst case) because we are blocking the daemon's accept loop while
/// retrying — long enough to ride out a brief OS handle spike, short enough
/// that the daemon doesn't appear wedged.
#[cfg(windows)]
async fn create_replacement_pipe_with_retry(
    endpoint: &str,
) -> Option<tokio::net::windows::named_pipe::NamedPipeServer> {
    const ATTEMPTS: u32 = 5;
    const INITIAL_DELAY_MS: u64 = 5;
    const MAX_DELAY_MS: u64 = 80;

    let mut delay_ms = INITIAL_DELAY_MS;
    for attempt in 0..ATTEMPTS {
        match create_replacement_pipe(endpoint) {
            Ok(p) => return Some(p),
            Err(e) => {
                tracing::warn!(
                    attempt = attempt + 1,
                    max_attempts = ATTEMPTS,
                    error = %e,
                    endpoint = %endpoint,
                    "named-pipe replacement create retry"
                );
                if attempt + 1 < ATTEMPTS {
                    tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
                    delay_ms = (delay_ms * 2).min(MAX_DELAY_MS);
                }
            }
        }
    }
    None
}

// ── Client connect ──────────────────────────────────────────────────

/// Connect to an IPC endpoint as a client.
///
/// On Unix, returns an `IpcConnection`. On Windows, returns an
/// `IpcClientConnection` (which has the same send/recv interface).
#[cfg(unix)]
pub async fn connect(endpoint: &str) -> Result<IpcConnection, IpcError> {
    let stream = tokio::net::UnixStream::connect(endpoint).await?;
    let (reader, writer) = tokio::io::split(stream);
    Ok(IpcConnection {
        reader,
        writer,
        read_buf: BytesMut::with_capacity(4096),
        recv_timeout: None,
        next_frame_request_id: 1,
    })
}

/// Connect to an IPC endpoint as a client.
///
/// Uses exponential backoff when the pipe is busy (ERROR_PIPE_BUSY = 231),
/// starting at 10ms and doubling up to 500ms per attempt, for a total of
/// ~30 seconds before giving up. This handles bursts from parallel build
/// systems that spawn hundreds of concurrent compilations.
#[cfg(windows)]
pub async fn connect(endpoint: &str) -> Result<IpcClientConnection, IpcError> {
    use tokio::net::windows::named_pipe::ClientOptions;

    const MAX_PIPE_BUSY_RETRIES: u32 = 50;
    const INITIAL_BACKOFF_MS: u64 = 10;
    const MAX_BACKOFF_MS: u64 = 500;

    let client = {
        let mut attempts = 0u32;
        let mut backoff_ms = INITIAL_BACKOFF_MS;
        loop {
            match ClientOptions::new().open(endpoint) {
                Ok(client) => break client,
                Err(e) if e.raw_os_error() == Some(231) => {
                    // ERROR_PIPE_BUSY = 231: all pipe instances are in use.
                    // This happens when many clients connect simultaneously
                    // (e.g. parallel compilation with 300+ source files).
                    attempts += 1;
                    if attempts >= MAX_PIPE_BUSY_RETRIES {
                        return Err(IpcError::Io(std::io::Error::new(
                            std::io::ErrorKind::ConnectionRefused,
                            format!(
                                "cannot connect to daemon at {endpoint}: \
                                 all pipe instances busy after {attempts} retries (~{:.0}s). \
                                 The daemon may be overloaded — reduce parallel compilation jobs \
                                 or restart the daemon with `zccache stop && zccache start`",
                                backoff_ms as f64 * attempts as f64 / 2000.0
                            ),
                        )));
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(backoff_ms)).await;
                    backoff_ms = (backoff_ms * 2).min(MAX_BACKOFF_MS);
                }
                Err(e) => return Err(IpcError::Io(e)),
            }
        }
    };

    let (reader, writer) = tokio::io::split(client);
    Ok(IpcClientConnection {
        reader,
        writer,
        read_buf: BytesMut::with_capacity(4096),
        recv_timeout: None,
        next_frame_request_id: 1,
    })
}

/// Generate a unique test endpoint name.
pub fn unique_test_endpoint() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let id = COUNTER.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();

    #[cfg(unix)]
    {
        format!("/tmp/zccache-test-{pid}-{id}.sock")
    }
    #[cfg(windows)]
    {
        format!(r"\\.\pipe\zccache-test-{pid}-{id}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{wire_prost::zccache_v1 as pb, DecodedWireMessage, Request, Response};

    #[tokio::test]
    async fn test_ping_pong() {
        let endpoint = unique_test_endpoint();
        let mut listener = IpcListener::bind(&endpoint).unwrap();

        let server = tokio::spawn(async move {
            let mut conn = listener.accept().await.unwrap();
            let msg: Option<Request> = conn.recv().await.unwrap();
            assert_eq!(msg, Some(Request::Ping));
            conn.send(&Response::Pong).await.unwrap();
        });

        let mut client = connect(&endpoint).await.unwrap();
        client.send(&Request::Ping).await.unwrap();
        let resp: Option<Response> = client.recv().await.unwrap();
        assert_eq!(resp, Some(Response::Pong));

        server.await.unwrap();
    }

    #[tokio::test]
    async fn test_multiple_messages() {
        let endpoint = unique_test_endpoint();
        let mut listener = IpcListener::bind(&endpoint).unwrap();

        let server = tokio::spawn(async move {
            let mut conn = listener.accept().await.unwrap();
            for _ in 0..5 {
                let msg: Option<Request> = conn.recv().await.unwrap();
                assert_eq!(msg, Some(Request::Ping));
                conn.send(&Response::Pong).await.unwrap();
            }
        });

        let mut client = connect(&endpoint).await.unwrap();
        for _ in 0..5 {
            client.send(&Request::Ping).await.unwrap();
            let resp: Option<Response> = client.recv().await.unwrap();
            assert_eq!(resp, Some(Response::Pong));
        }

        server.await.unwrap();
    }

    #[tokio::test]
    async fn recv_wire_accepts_bincode_request_on_live_ipc() {
        let endpoint = unique_test_endpoint();
        let mut listener = IpcListener::bind(&endpoint).unwrap();

        let server = tokio::spawn(async move {
            let mut conn = listener.accept().await.unwrap();
            let msg: Option<DecodedWireMessage<Request, pb::Request>> =
                conn.recv_wire().await.unwrap();
            assert_eq!(msg, Some(DecodedWireMessage::BincodeV15(Request::Ping)));
            conn.send(&Response::Pong).await.unwrap();
        });

        let mut client = connect(&endpoint).await.unwrap();
        client.send(&Request::Ping).await.unwrap();
        let resp: Option<Response> = client.recv().await.unwrap();
        assert_eq!(resp, Some(Response::Pong));

        server.await.unwrap();
    }

    #[tokio::test]
    async fn recv_wire_accepts_prost_request_on_live_ipc() {
        let endpoint = unique_test_endpoint();
        let mut listener = IpcListener::bind(&endpoint).unwrap();

        let server = tokio::spawn(async move {
            let mut conn = listener.accept().await.unwrap();
            let msg: Option<DecodedWireMessage<Request, pb::Request>> =
                conn.recv_wire().await.unwrap();
            match msg {
                Some(DecodedWireMessage::ProstV16(request)) => {
                    assert_eq!(request.request_id, "live-prost");
                    assert!(matches!(request.body, Some(pb::request::Body::Ping(_))));
                }
                other => panic!("expected prost request, got {other:?}"),
            }
            conn.send(&Response::Pong).await.unwrap();
        });

        let mut client = connect(&endpoint).await.unwrap();
        let request = pb::Request {
            body: Some(pb::request::Body::Ping(pb::Empty {})),
            request_id: "live-prost".to_string(),
        };
        client.send_prost(&request).await.unwrap();
        let resp: Option<Response> = client.recv().await.unwrap();
        assert_eq!(resp, Some(Response::Pong));

        server.await.unwrap();
    }

    #[tokio::test]
    async fn backend_handle_probe_detector_preserves_zccache_requests() {
        let endpoint = unique_test_endpoint();
        let daemon = crate::ipc::current_backend_identity(&endpoint).unwrap();
        let mut listener = IpcListener::bind(&endpoint).unwrap();

        let server = tokio::spawn(async move {
            let mut conn = listener.accept().await.unwrap();
            assert!(!conn.try_serve_backend_handle_probe(&daemon).await.unwrap());
            let msg: Option<Request> = conn.recv().await.unwrap();
            assert_eq!(msg, Some(Request::Ping));
            conn.send(&Response::Pong).await.unwrap();
        });

        let mut client = connect(&endpoint).await.unwrap();
        client.send(&Request::Ping).await.unwrap();
        let resp: Option<Response> = client.recv().await.unwrap();
        assert_eq!(resp, Some(Response::Pong));

        server.await.unwrap();
    }

    #[tokio::test]
    async fn backend_handle_probe_succeeds_on_direct_endpoint() {
        let endpoint = unique_test_endpoint();
        let daemon = crate::ipc::current_backend_identity(&endpoint).unwrap();
        let probe_endpoint = daemon.ipc_endpoint.clone();
        let expected_daemon = daemon.clone();
        let mut listener = IpcListener::bind(&endpoint).unwrap();

        let server = tokio::spawn(async move {
            let mut conn = listener.accept().await.unwrap();
            assert!(conn.try_serve_backend_handle_probe(&daemon).await.unwrap());
        });

        let (service_name, handle_endpoint) = tokio::task::spawn_blocking(move || {
            let handle =
                running_process::broker::backend_handle::BackendHandle::probe_with_service(
                    "zccache",
                    crate::core::VERSION,
                    &probe_endpoint,
                    &expected_daemon,
                )
                .unwrap();
            (
                handle.service_name.clone(),
                handle.daemon_process.ipc_endpoint.path.clone(),
            )
        })
        .await
        .unwrap();

        assert_eq!(service_name, "zccache");
        assert_eq!(
            handle_endpoint,
            crate::ipc::running_process_endpoint(&endpoint).path
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn test_connection_closed() {
        let endpoint = unique_test_endpoint();
        let mut listener = IpcListener::bind(&endpoint).unwrap();

        let server = tokio::spawn(async move {
            let _conn = listener.accept().await.unwrap();
            // Drop connection immediately
        });

        // Small delay to let server create pipe and start accepting
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        let mut client = connect(&endpoint).await.unwrap();
        // Give server time to accept and drop
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let resp: Result<Option<Response>, _> = client.recv().await;
        // Should get None (clean close) or ConnectionClosed or broken pipe
        match resp {
            Ok(None) => {}
            Err(IpcError::ConnectionClosed) => {}
            Err(IpcError::Io(_)) => {}
            other => panic!("unexpected result: {other:?}"),
        }

        server.await.unwrap();
    }

    /// Regression test for <https://github.com/zackees/zccache/issues/666>.
    ///
    /// The pre-#666 Windows accept path would `pop_front().expect(...)`-panic
    /// the moment the pool ever depleted. After the fix, a fully drained pool
    /// must recover via the emergency-create path on the next accept.
    #[cfg(windows)]
    #[tokio::test]
    async fn pool_recovers_from_full_depletion() {
        let endpoint = unique_test_endpoint();
        let mut listener = IpcListener::bind(&endpoint).unwrap();

        // Simulate the issue #666 wedge: pool is fully drained by repeated
        // replacement-create failures (modelled here by an explicit drain).
        let drained = listener.test_drain_pool();
        assert!(drained > 0, "fresh listener should have pre-created pipes");

        let server = tokio::spawn(async move {
            // accept() on a drained pool must NOT panic — it must take the
            // emergency-create path and serve the client.
            let mut conn = listener.accept().await.expect("accept after drain");
            let msg: Option<Request> = conn.recv().await.unwrap();
            assert_eq!(msg, Some(Request::Ping));
            conn.send(&Response::Pong).await.unwrap();
        });

        // The emergency create + connect handshake adds a few ms — give the
        // server room to set up before the client connects.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let mut client = connect(&endpoint).await.unwrap();
        client.send(&Request::Ping).await.unwrap();
        let resp: Option<Response> = client.recv().await.unwrap();
        assert_eq!(resp, Some(Response::Pong));

        server.await.unwrap();
    }

    #[tokio::test]
    async fn test_parallel_connections() {
        let endpoint = unique_test_endpoint();
        let mut listener = IpcListener::bind(&endpoint).unwrap();
        let n = 8;

        let server = tokio::spawn(async move {
            for _ in 0..n {
                let mut conn = listener.accept().await.unwrap();
                let msg: Option<Request> = conn.recv().await.unwrap();
                assert_eq!(msg, Some(Request::Ping));
                conn.send(&Response::Pong).await.unwrap();
            }
        });

        // Spawn N clients simultaneously to stress the pipe pool.
        let mut handles = Vec::new();
        let ep = endpoint.clone();
        for _ in 0..n {
            let ep = ep.clone();
            handles.push(tokio::spawn(async move {
                let mut client = connect(&ep).await.unwrap();
                client.send(&Request::Ping).await.unwrap();
                let resp: Option<Response> = client.recv().await.unwrap();
                assert_eq!(resp, Some(Response::Pong));
            }));
        }

        for h in handles {
            h.await.unwrap();
        }
        server.await.unwrap();
    }
}
