//! Windows-side IPC: `IpcClientConnection`, client connect with
//! `ERROR_PIPE_BUSY` backoff, and the accept path for [`IpcListener`] with
//! pool-depletion recovery (issue #666).

use std::time::Duration;

use bytes::BytesMut;
use tokio::io::{AsyncWriteExt, ReadHalf, WriteHalf};
use tokio::net::windows::named_pipe::{NamedPipeClient, NamedPipeServer};

use crate::ipc::error::IpcError;

use super::framing::{decode_response_wire, recv_bincode_loop, recv_wire_loop};
use super::{IpcConnection, IpcListener};

const WINDOWS_PIPE_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Client-side IPC connection (Windows uses a different type).
pub struct IpcClientConnection {
    pub(super) reader: ReadHalf<NamedPipeClient>,
    pub(super) writer: WriteHalf<NamedPipeClient>,
    pub(super) read_buf: BytesMut,
    /// Optional default timeout for `recv`. See `IpcConnection::recv_timeout`.
    pub(super) recv_timeout: Option<Duration>,
    /// See `IpcConnection::next_frame_request_id`.
    pub(super) next_frame_request_id: u64,
}

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

// ── IpcListener Windows accept path ─────────────────────────────────

impl IpcListener {
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
    pub(super) async fn accept_windows(&mut self) -> Result<IpcConnection, IpcError> {
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

            match tokio::time::timeout(WINDOWS_PIPE_CONNECT_TIMEOUT, pipe.connect()).await {
                Ok(Ok(())) => {}
                Ok(Err(e)) => {
                    // The popped pipe is now dropped. Replenish the pool with a
                    // fresh instance (best-effort) and retry the accept so the
                    // daemon's main loop never sees a transient connect glitch.
                    tracing::warn!(
                        endpoint = %self.inner.endpoint,
                        error = %e,
                        "named-pipe connect failed - replenishing and retrying"
                    );
                    if let Ok(replacement) = create_replacement_pipe(&self.inner.endpoint) {
                        self.inner.pool.push_back(replacement);
                    }
                    continue;
                }
                Err(_) => {
                    // Drop the stalled pipe instance and keep the daemon accept
                    // loop moving on a fresh best-effort replacement.
                    tracing::warn!(
                        endpoint = %self.inner.endpoint,
                        timeout = ?WINDOWS_PIPE_CONNECT_TIMEOUT,
                        "named-pipe connect timed out - dropping pipe and retrying"
                    );
                    if let Ok(replacement) = create_replacement_pipe(&self.inner.endpoint) {
                        self.inner.pool.push_back(replacement);
                    }
                    continue;
                }
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
    #[cfg(test)]
    pub(crate) fn test_drain_pool(&mut self) -> usize {
        let drained = self.inner.pool.len();
        self.inner.pool.clear();
        drained
    }
}

/// Issue #666: synchronous replacement pipe creation. Used by the emergency
/// path when the pool is fully depleted.
pub(super) fn create_replacement_pipe(endpoint: &str) -> Result<NamedPipeServer, IpcError> {
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
async fn create_replacement_pipe_with_retry(endpoint: &str) -> Option<NamedPipeServer> {
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

/// Connect to an IPC endpoint as a client.
///
/// Uses exponential backoff when the pipe is busy (ERROR_PIPE_BUSY = 231),
/// starting at 10ms and doubling up to 500ms per attempt, for a total of
/// about 5 seconds before giving up. This handles bursts from parallel build
/// systems that spawn hundreds of concurrent compilations.
pub async fn connect(endpoint: &str) -> Result<IpcClientConnection, IpcError> {
    use tokio::net::windows::named_pipe::ClientOptions;

    const MAX_PIPE_BUSY_RETRIES: u32 = 50;
    const INITIAL_BACKOFF_MS: u64 = 10;
    const MAX_BACKOFF_MS: u64 = 500;

    let client = match tokio::time::timeout(WINDOWS_PIPE_CONNECT_TIMEOUT, async {
        let mut attempts = 0u32;
        let mut backoff_ms = INITIAL_BACKOFF_MS;
        let client = loop {
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
        };
        Ok(client)
    })
    .await
    {
        Ok(result) => result?,
        Err(_) => {
            return Err(IpcError::Io(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                format!(
                    "cannot connect to daemon at {endpoint}: connect timed out after {WINDOWS_PIPE_CONNECT_TIMEOUT:?}"
                ),
            )));
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
