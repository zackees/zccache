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

use bytes::BytesMut;
use tokio::io::{AsyncWriteExt, ReadHalf, WriteHalf};

use super::error::IpcError;

mod framing;
mod probe;
#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;

use framing::{decode_response_wire, recv_bincode_loop, recv_wire_loop};

#[cfg(unix)]
pub use unix::connect;
#[cfg(windows)]
pub use windows::{connect, IpcClientConnection};

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

// ── IpcListener ─────────────────────────────────────────────────────

/// Listens for incoming IPC connections.
pub struct IpcListener {
    pub(super) inner: ListenerInner,
}

#[cfg(unix)]
pub(super) struct ListenerInner {
    listener: tokio::net::UnixListener,
}

#[cfg(windows)]
pub(super) struct ListenerInner {
    pub(super) endpoint: String,
    /// Pool of pre-created pipe instances waiting for clients.
    /// Multiple pending instances eliminate the busy window between
    /// accepting a connection and creating the next instance (Bug 4 fix).
    pub(super) pool: std::collections::VecDeque<tokio::net::windows::named_pipe::NamedPipeServer>,
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

            // Issue #774: the first pipe instance asserts namespace ownership
            // via `first_pipe_instance(true)`. After a hard-killed daemon
            // (`taskkill /F`), the pipe name can linger briefly in the OS
            // namespace while the kernel reaps the dead process's handles.
            // Retry the first bind with backoff to ride out that GC window
            // before declaring the namespace contested — the alternative is
            // the daemon failing to start and forcing a reboot. The
            // companion fix in `is_process_alive` (also #774) stops the CLI
            // from spinning against a still-referenced-but-terminated PID,
            // which is what allowed the orphan pipe to outlive the daemon
            // long enough to matter here.
            const FIRST_BIND_ATTEMPTS: u32 = 8;
            const FIRST_BIND_INITIAL_DELAY_MS: u64 = 20;
            const FIRST_BIND_MAX_DELAY_MS: u64 = 160;

            let mut pool = VecDeque::with_capacity(pool_size);
            let first_pipe = {
                let mut attempt = 0u32;
                let mut delay_ms = FIRST_BIND_INITIAL_DELAY_MS;
                loop {
                    match ServerOptions::new()
                        .first_pipe_instance(true)
                        .create(endpoint)
                    {
                        Ok(p) => break p,
                        Err(e) => {
                            attempt += 1;
                            if attempt >= FIRST_BIND_ATTEMPTS {
                                return Err(e.into());
                            }
                            tracing::warn!(
                                attempt,
                                max_attempts = FIRST_BIND_ATTEMPTS,
                                error = %e,
                                endpoint = %endpoint,
                                "first pipe instance bind failed; retrying after backoff (issue #774)"
                            );
                            std::thread::sleep(std::time::Duration::from_millis(delay_ms));
                            delay_ms = (delay_ms * 2).min(FIRST_BIND_MAX_DELAY_MS);
                        }
                    }
                }
            };
            pool.push_back(first_pipe);
            for _ in 1..pool_size {
                let pipe = ServerOptions::new()
                    .first_pipe_instance(false)
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
mod tests;
