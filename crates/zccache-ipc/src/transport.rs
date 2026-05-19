//! IPC transport layer.
//!
//! Provides platform-abstracted IPC using named pipes on Windows
//! and Unix domain sockets on Unix. Messages are length-prefixed
//! bincode via `zccache-protocol`.

use std::time::Duration;

use bytes::BytesMut;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::error::IpcError;

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
/// Five minutes is intentionally generous. If a real workload exceeds it,
/// switch that single call site to `recv_with_timeout` with a longer
/// budget rather than bumping the const — the const is also the upper
/// bound on hung-daemon detection latency for `ensure_daemon`'s
/// auto-recovery path, and shortening it is the easier fix once the
/// daemon side is fast enough.
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
}

/// Client-side IPC connection (Windows uses a different type).
#[cfg(windows)]
pub struct IpcClientConnection {
    reader: tokio::io::ReadHalf<tokio::net::windows::named_pipe::NamedPipeClient>,
    writer: tokio::io::WriteHalf<tokio::net::windows::named_pipe::NamedPipeClient>,
    read_buf: BytesMut,
    /// Optional default timeout for `recv`. See `IpcConnection::recv_timeout`.
    recv_timeout: Option<Duration>,
}

// ── IpcConnection impl (server-side on Windows, both on Unix) ───────

impl IpcConnection {
    /// Send a serializable message over the connection.
    pub async fn send<T: serde::Serialize>(&mut self, msg: &T) -> Result<(), IpcError> {
        let buf = zccache_protocol::encode_message(msg)?;
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

    /// The recv read loop, factored out so both `recv` and
    /// `recv_with_timeout` share the same implementation. Always
    /// unbounded — the wrapping methods add the deadline.
    async fn recv_loop<T: serde::de::DeserializeOwned>(&mut self) -> Result<Option<T>, IpcError> {
        loop {
            if let Some(msg) = zccache_protocol::decode_message::<T>(&mut self.read_buf)? {
                return Ok(Some(msg));
            }
            let mut tmp = [0u8; 4096];
            let n = self.reader.read(&mut tmp).await?;
            if n == 0 {
                if self.read_buf.is_empty() {
                    return Ok(None);
                }
                return Err(IpcError::ConnectionClosed);
            }
            self.read_buf.extend_from_slice(&tmp[..n]);
        }
    }
}

// ── IpcClientConnection (Windows client-side) ───────────────────────

#[cfg(windows)]
impl IpcClientConnection {
    /// Send a serializable message over the connection.
    pub async fn send<T: serde::Serialize>(&mut self, msg: &T) -> Result<(), IpcError> {
        let buf = zccache_protocol::encode_message(msg)?;
        self.writer.write_all(&buf).await?;
        self.writer.flush().await?;
        Ok(())
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

    async fn recv_loop<T: serde::de::DeserializeOwned>(&mut self) -> Result<Option<T>, IpcError> {
        loop {
            if let Some(msg) = zccache_protocol::decode_message::<T>(&mut self.read_buf)? {
                return Ok(Some(msg));
            }
            let mut tmp = [0u8; 4096];
            let n = self.reader.read(&mut tmp).await?;
            if n == 0 {
                if self.read_buf.is_empty() {
                    return Ok(None);
                }
                return Err(IpcError::ConnectionClosed);
            }
            self.read_buf.extend_from_slice(&tmp[..n]);
        }
    }
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

            let pool_size = std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(4)
                .min(16);

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
            })
        }
        #[cfg(windows)]
        {
            use tokio::net::windows::named_pipe::ServerOptions;

            // Pop the front pipe and wait for a client to connect.
            let pipe = self
                .inner
                .pool
                .pop_front()
                .expect("pipe pool must not be empty");
            pipe.connect().await?;

            // Create a replacement instance and push to the back.
            let replacement = ServerOptions::new()
                .first_pipe_instance(false)
                .create(&self.inner.endpoint)?;
            self.inner.pool.push_back(replacement);

            let (reader, writer) = tokio::io::split(pipe);
            Ok(IpcConnection {
                reader,
                writer,
                read_buf: BytesMut::with_capacity(4096),
                recv_timeout: None,
            })
        }
    }
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
    use zccache_protocol::{Request, Response};

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
