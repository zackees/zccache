//! IPC transport layer for zccache.
//!
//! Provides platform-abstracted IPC between CLI/compiler wrapper
//! and the daemon, using Unix domain sockets on Unix and named
//! pipes on Windows.

#![allow(clippy::missing_errors_doc)]

pub(crate) use zccache_platform as platform;

pub mod broker;
pub mod error;
mod full_family;
pub mod manifest;
pub mod probe;
pub mod transport;

pub use broker::{
    connect_daemon, connect_daemon_with_route, to_running_process_endpoint, BrokerRefusal,
    DaemonConnectRoute,
};
pub use error::IpcError;
#[cfg(test)]
pub(crate) use full_family::full_family_roundtrip_with_selection;
pub use full_family::{
    full_family_roundtrip, full_family_roundtrip_classified, full_family_wire_mismatch_error,
    FullFamilyFailurePhase, FullFamilyRoundtripFailure,
};
pub use manifest::{publish_manifest, publish_manifest_in, publish_service_definition};
pub use transport::IpcClientConnection;
pub use transport::{
    connect, unique_test_endpoint, IpcConnection, IpcListener, DEFAULT_CLIENT_RECV_TIMEOUT,
};

use zccache_core::NormalizedPath;
use zccache_protocol::{self as protocol, wire_prost, Response};

type ClientConnection = IpcConnection;

/// Daemon control requests that may opt into the v16 prost migration slice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DaemonControlRequest {
    /// Health check.
    Ping,
    /// Request daemon status/statistics.
    Status,
    /// Request daemon shutdown.
    Shutdown,
    /// Clear all caches.
    Clear,
}

impl DaemonControlRequest {
    #[must_use]
    fn to_protocol_request(self) -> protocol::Request {
        match self {
            Self::Ping => protocol::Request::Ping,
            Self::Status => protocol::Request::Status,
            Self::Shutdown => protocol::Request::Shutdown,
            Self::Clear => protocol::Request::Clear,
        }
    }
}

/// Send a `ReleaseWorktreeHandles` request to the daemon and receive its
/// response. Wraps the wire dispatch the same way [`daemon_control_roundtrip`]
/// does for the four parameterless control requests, but plumbs the
/// path-carrying request directly since it cannot be a `Copy` enum variant.
///
/// This is the `zccache release-handles --path X` plumbing (#694 Phase 2 — the
/// standalone CLI surface that ships value to direct-zccache consumers
/// independently of the router architecture).
///
/// # Errors
///
/// Returns the IPC error from wire selection or the selected send/receive
/// path. Invalid `ZCCACHE_DAEMON_WIRE` values are rejected.
pub async fn daemon_release_worktree_handles_roundtrip(
    endpoint: &str,
    path: std::path::PathBuf,
    recv_timeout: Option<std::time::Duration>,
) -> Result<Option<Response>, IpcError> {
    let selection = wire_prost::client_wire_selection_from_env().map_err(IpcError::Endpoint)?;
    let request = protocol::Request::ReleaseWorktreeHandles {
        path: zccache_core::NormalizedPath::new(path),
    };

    if broker::broker_lane_active() {
        let (mut conn, route) = connect_control_client_with_route(endpoint).await?;
        if matches!(route, DaemonConnectRoute::Broker { .. }) {
            let request = wire_prost::supported_control_request_to_prost(&request)
                .map_err(IpcError::Endpoint)?;
            conn.send_frame_v1_request(&request).await?;
            return recv_control_wire_response(&mut conn, recv_timeout).await;
        }
        drop(conn);
    }

    // Mirror the `daemon_control_roundtrip_with_selection` dispatch pattern,
    // inlined here because `Request::ReleaseWorktreeHandles` carries a path
    // (and so cannot route through the `Copy` `DaemonControlRequest` enum).
    match selection.preferred_format() {
        wire_prost::WireFormat::BincodeV15 => send_bincode(endpoint, &request, recv_timeout).await,
        wire_prost::WireFormat::FrameV1 => send_frame(endpoint, &request, recv_timeout).await,
        wire_prost::WireFormat::ProstV16 => {
            match send_prost(endpoint, &request, recv_timeout).await {
                Ok(response) => Ok(response),
                Err(err)
                    if selection.allows_bincode_fallback()
                        && full_family_wire_mismatch_error(&err) =>
                {
                    send_bincode(endpoint, &request, recv_timeout).await
                }
                Err(err) => Err(err),
            }
        }
    }
}

async fn send_bincode(
    endpoint: &str,
    request: &protocol::Request,
    recv_timeout: Option<std::time::Duration>,
) -> Result<Option<Response>, IpcError> {
    let mut conn = connect_control_client(endpoint).await?;
    conn.send(request).await?;
    recv_control_response(&mut conn, recv_timeout).await
}

async fn send_prost(
    endpoint: &str,
    request: &protocol::Request,
    recv_timeout: Option<std::time::Duration>,
) -> Result<Option<Response>, IpcError> {
    let mut conn = connect_control_client(endpoint).await?;
    let prost_req =
        wire_prost::supported_control_request_to_prost(request).map_err(IpcError::Endpoint)?;
    conn.send_prost(&prost_req).await?;
    conn.recv_response_for_wire_with_timeout(
        recv_timeout.unwrap_or(DEFAULT_CLIENT_RECV_TIMEOUT),
        wire_prost::WireFormat::ProstV16,
    )
    .await
}

async fn send_frame(
    endpoint: &str,
    request: &protocol::Request,
    recv_timeout: Option<std::time::Duration>,
) -> Result<Option<Response>, IpcError> {
    let mut conn = connect_control_client(endpoint).await?;
    let prost_req =
        wire_prost::supported_control_request_to_prost(request).map_err(IpcError::Endpoint)?;
    conn.send_frame_v1_request(&prost_req).await?;
    recv_control_wire_response(&mut conn, recv_timeout).await
}

/// Send a daemon control request and receive its response.
///
/// Only `Ping`, `Status`, `Shutdown`, and `Clear` are eligible for the v16
/// prost client path. Unset/`auto` `ZCCACHE_DAEMON_WIRE` prefers prost and
/// retries as bincode only when the response lane proves an old daemon rejected
/// framing. EOF/I/O and application errors never trigger replay.
///
/// # Errors
///
/// Returns the IPC error from the selected send/receive path, or an endpoint
/// error when `ZCCACHE_DAEMON_WIRE` is invalid.
pub async fn daemon_control_roundtrip(
    endpoint: &str,
    request: DaemonControlRequest,
    recv_timeout: Option<std::time::Duration>,
) -> Result<Option<Response>, IpcError> {
    let selection = wire_prost::client_wire_selection_from_env().map_err(IpcError::Endpoint)?;
    daemon_control_roundtrip_with_selection(endpoint, request, recv_timeout, selection).await
}

async fn daemon_control_roundtrip_with_selection(
    endpoint: &str,
    request: DaemonControlRequest,
    recv_timeout: Option<std::time::Duration>,
    selection: wire_prost::ClientWireSelection,
) -> Result<Option<Response>, IpcError> {
    // Issue #720 Phase 1: when the broker lane carries the connection, the data
    // wire itself must be the version-checked 0x7A63 FrameV1 envelope. Connect
    // once through the broker route; if it was actually taken, send the control
    // request over FrameV1 to the broker-resolved endpoint (the daemon serve
    // loop already auto-detects it) with no bincode fallback — the broker lane
    // is version-authoritative. A direct/fallback route keeps the existing
    // env-selected behavior byte-for-byte.
    if broker::broker_lane_active() {
        let (mut conn, route) = connect_control_client_with_route(endpoint).await?;
        if matches!(route, DaemonConnectRoute::Broker { .. }) {
            let request = request.to_protocol_request();
            let request = wire_prost::supported_control_request_to_prost(&request)
                .map_err(IpcError::Endpoint)?;
            conn.send_frame_v1_request(&request).await?;
            return recv_control_wire_response(&mut conn, recv_timeout).await;
        }
        // Broker requested but silently fell back to direct: drop the probe
        // connection and use the env-selected path below unchanged.
        drop(conn);
    }

    match selection.preferred_format() {
        wire_prost::WireFormat::BincodeV15 => {
            send_bincode_control(endpoint, request, recv_timeout).await
        }
        // Forced-only lane (`ZCCACHE_DAEMON_WIRE=frame`): no bincode
        // fallback, the caller asked for the Frame envelope explicitly.
        wire_prost::WireFormat::FrameV1 => {
            send_frame_control(endpoint, request, recv_timeout).await
        }
        wire_prost::WireFormat::ProstV16 => {
            match send_prost_control(endpoint, request, recv_timeout).await {
                Ok(response) => Ok(response),
                Err(err)
                    if selection.allows_bincode_fallback()
                        && full_family_wire_mismatch_error(&err) =>
                {
                    send_bincode_control(endpoint, request, recv_timeout).await
                }
                Err(err) => Err(err),
            }
        }
    }
}

async fn connect_control_client(endpoint: &str) -> Result<ClientConnection, IpcError> {
    let mut conn = connect_daemon(endpoint).await?;
    conn.set_recv_timeout(DEFAULT_CLIENT_RECV_TIMEOUT);
    Ok(conn)
}

/// Like [`connect_control_client`] but reports the broker route taken, so the
/// control roundtrip can pick the version-checked FrameV1 wire when the broker
/// lane actually carries the connection (issue #720 Phase 1).
async fn connect_control_client_with_route(
    endpoint: &str,
) -> Result<(ClientConnection, DaemonConnectRoute), IpcError> {
    let (mut conn, route) = connect_daemon_with_route(endpoint).await?;
    conn.set_recv_timeout(DEFAULT_CLIENT_RECV_TIMEOUT);
    Ok((conn, route))
}

async fn send_bincode_control(
    endpoint: &str,
    request: DaemonControlRequest,
    recv_timeout: Option<std::time::Duration>,
) -> Result<Option<Response>, IpcError> {
    let mut conn = connect_control_client(endpoint).await?;
    let request = request.to_protocol_request();
    conn.send(&request).await?;
    recv_control_response(&mut conn, recv_timeout).await
}

async fn send_prost_control(
    endpoint: &str,
    request: DaemonControlRequest,
    recv_timeout: Option<std::time::Duration>,
) -> Result<Option<Response>, IpcError> {
    let mut conn = connect_control_client(endpoint).await?;
    let request = request.to_protocol_request();
    let request =
        wire_prost::supported_control_request_to_prost(&request).map_err(IpcError::Endpoint)?;
    conn.send_prost(&request).await?;
    conn.recv_response_for_wire_with_timeout(
        recv_timeout.unwrap_or(DEFAULT_CLIENT_RECV_TIMEOUT),
        wire_prost::WireFormat::ProstV16,
    )
    .await
}

async fn send_frame_control(
    endpoint: &str,
    request: DaemonControlRequest,
    recv_timeout: Option<std::time::Duration>,
) -> Result<Option<Response>, IpcError> {
    let mut conn = connect_control_client(endpoint).await?;
    let request = request.to_protocol_request();
    let request =
        wire_prost::supported_control_request_to_prost(&request).map_err(IpcError::Endpoint)?;
    conn.send_frame_v1_request(&request).await?;
    recv_control_wire_response(&mut conn, recv_timeout).await
}

async fn recv_control_response(
    conn: &mut ClientConnection,
    recv_timeout: Option<std::time::Duration>,
) -> Result<Option<Response>, IpcError> {
    match recv_timeout {
        Some(timeout) => conn.recv_with_timeout(timeout).await,
        None => conn.recv().await,
    }
}

async fn recv_control_wire_response(
    conn: &mut ClientConnection,
    recv_timeout: Option<std::time::Duration>,
) -> Result<Option<Response>, IpcError> {
    let response: Option<protocol::DecodedWireMessage<Response, wire_prost::zccache_v1::Response>> =
        match recv_timeout {
            Some(timeout) => conn.recv_wire_with_timeout(timeout).await?,
            None => conn.recv_wire().await?,
        };

    match response {
        Some(protocol::DecodedWireMessage::BincodeV15(response)) => Ok(Some(response)),
        Some(
            protocol::DecodedWireMessage::ProstV16(response)
            | protocol::DecodedWireMessage::FrameV1 {
                message: response, ..
            },
        ) => wire_prost::supported_control_response_from_prost(response)
            .map(Some)
            .map_err(|message| {
                IpcError::Protocol(protocol::ProtocolError::Deserialization(message))
            }),
        None => Ok(None),
    }
}

/// Returns the platform-specific default IPC endpoint path.
///
/// - Linux: `$XDG_RUNTIME_DIR/zccache/sock` or `/tmp/zccache-$USER/sock`
/// - macOS: `/tmp/zccache-$USER/sock`
/// - Windows: `\\.\pipe\zccache-{username}`
///
/// If `ZCCACHE_CACHE_DIR` is set, the endpoint is derived from that cache root
/// so independently managed cache roots get independent daemon instances.
/// If `ZCCACHE_DAEMON_NAMESPACE` is also set, the sanitized namespace is folded
/// into the endpoint while the unset/default namespace keeps the historical
/// endpoint unchanged.
#[must_use]
pub fn default_endpoint() -> String {
    let namespace = zccache_core::config::daemon_namespace();
    if let Some(cache_dir) = normalized_override_root() {
        return endpoint_for_cache_dir(cache_dir.as_path(), namespace.as_deref());
    }

    let username =
        crate::platform::ipc::current_user_name().unwrap_or_else(|| String::from("unknown"));
    crate::platform::ipc::Endpoint::select(
        default_file_endpoint(namespace.as_deref()),
        pipe_name(&username, namespace.as_deref()),
    )
    .to_string()
}

pub fn endpoint_for_cache_dir(cache_dir: &std::path::Path, namespace: Option<&str>) -> String {
    let direct = cache_dir.join(daemon_socket_name(namespace));
    let direct = direct.to_string_lossy();
    let file_path = if crate::platform::ipc::Endpoint::file_path_is_portable(&direct) {
        direct.into_owned()
    } else {
        compact_cache_dir_endpoint(cache_dir, namespace)
    };
    let suffix = zccache_core::stable_path_id(cache_dir);
    crate::platform::ipc::Endpoint::select(file_path, pipe_name(&suffix, namespace)).to_string()
}

fn compact_cache_dir_endpoint(cache_dir: &std::path::Path, namespace: Option<&str>) -> String {
    // Endpoint is a Unix socket path; return it as a `String` directly so
    // we don't round-trip through `PathBuf` only to immediately convert
    // back via `to_string_lossy`. The previous shape was the only
    // `ban_std_pathbuf` lint hit in this file.
    let cache_id = zccache_core::stable_path_id(cache_dir);
    format!("/tmp/zccache-{cache_id}-{}", daemon_socket_name(namespace))
}

/// Derive a platform IPC endpoint for a portable private daemon name.
///
/// When `cache_dir` is supplied the endpoint is rooted in that cache identity;
/// otherwise it follows the default runtime/tmp/pipe location while folding
/// the sanitized daemon name into the endpoint.
#[must_use]
pub fn endpoint_for_private_daemon_name(
    cache_dir: Option<&std::path::Path>,
    daemon_name: &str,
) -> String {
    let namespace = zccache_core::config::sanitize_daemon_namespace(daemon_name)
        .unwrap_or_else(|| zccache_core::config::DEV_DAEMON_NAMESPACE.to_string());
    if let Some(cache_dir) = cache_dir {
        return endpoint_for_cache_dir(cache_dir, Some(&namespace));
    }

    let username =
        crate::platform::ipc::current_user_name().unwrap_or_else(|| String::from("unknown"));
    crate::platform::ipc::Endpoint::select(
        default_file_endpoint(Some(&namespace)),
        pipe_name(&username, Some(&namespace)),
    )
    .to_string()
}

/// Returns the path for the daemon lock file.
#[must_use]
pub fn lock_file_path() -> NormalizedPath {
    let namespace = zccache_core::config::daemon_namespace();
    if let Some(cache_dir) = normalized_override_root() {
        return cache_dir.join(lock_file_name(namespace.as_deref()));
    }

    let file_lock = {
        let endpoint = default_endpoint();
        // Endpoint paths always live inside a directory, but the denied
        // `expect_used` lint fires when clippy compiles this cfg(unix)
        // arm (it was landed from a host where clippy never saw it —
        // caught during soldr#1286 docker-linux verification).
        let dir = std::path::Path::new(&endpoint)
            .parent()
            .unwrap_or_else(|| std::path::Path::new("/tmp"));
        dir.join(lock_file_name(namespace.as_deref()))
    };
    let windows_lock =
        { zccache_core::config::default_cache_dir().join(lock_file_name(namespace.as_deref())) };
    crate::platform::ipc::select_host_text(
        file_lock.to_string_lossy().into_owned(),
        windows_lock.to_string_lossy().into_owned(),
    )
    .into()
}

/// The `v<VERSION>` tag folded into every endpoint + lock name (#1004 / #694
/// Phase 1). Without it, two installed zccache versions contend for one
/// socket/pipe/lock and "resolve" the conflict by kill-and-replace ping-pong
/// (the #755 lifecycle-log herds). With it, each version gets a distinct
/// front door, so coexisting versions never fight — kill-and-replace becomes a
/// same-version-only rare path.
///
/// Standalone-only: embedded hosts (soldr/fbuild) use synthetic `embedded:`
/// endpoints and never bind IPC, so they are unaffected.
fn version_tag() -> String {
    zccache_core::config::versioned_subdir()
}

fn socket_name(namespace: Option<&str>) -> String {
    let v = version_tag();
    match namespace {
        Some(ns) => format!("sock-{ns}-{v}"),
        None => format!("sock-{v}"),
    }
}

fn daemon_socket_name(namespace: Option<&str>) -> String {
    let v = version_tag();
    match namespace {
        Some(ns) => format!("daemon-{ns}-{v}.sock"),
        None => format!("daemon-{v}.sock"),
    }
}

fn pipe_name(base: &str, namespace: Option<&str>) -> String {
    let base = zccache_core::config::sanitize_ipc_component(base)
        .unwrap_or_else(|| String::from("unknown"));
    let v = version_tag();
    match namespace {
        Some(ns) => format!("zccache-{base}-{ns}-{v}"),
        None => format!("zccache-{base}-{v}"),
    }
}

fn default_file_endpoint(namespace: Option<&str>) -> String {
    if let Some(runtime_dir) = crate::platform::host::runtime_dir() {
        return format!("{runtime_dir}/zccache/{}", socket_name(namespace));
    }
    let user = crate::platform::host::current_user().unwrap_or_else(|| String::from("unknown"));
    format!("/tmp/zccache-{user}/{}", socket_name(namespace))
}

fn lock_file_name(namespace: Option<&str>) -> String {
    let v = version_tag();
    match namespace {
        Some(ns) => format!("daemon-{ns}-{v}.lock"),
        None => format!("daemon-{v}.lock"),
    }
}

/// Write the daemon PID to the lock file.
///
/// Creates parent directories if needed.
pub fn write_lock_file(pid: u32) -> Result<(), std::io::Error> {
    let path = lock_file_path();
    if let Some(parent) = path.parent() {
        // #1171: same directory family as the socket endpoint.
        zccache_core::config::create_dir_all_private(parent)?;
    }
    std::fs::write(&path, pid.to_string())
}

/// Read the daemon PID from the lock file, if it exists and is valid.
#[must_use]
pub fn read_lock_file_pid() -> Option<u32> {
    std::fs::read_to_string(lock_file_path())
        .ok()
        .and_then(|s| s.trim().parse().ok())
}

/// Remove the lock file.
pub fn remove_lock_file() {
    let _ = std::fs::remove_file(lock_file_path());
}

/// Retire a stale native endpoint without exposing its host representation.
pub fn retire_endpoint(endpoint: &str) -> std::io::Result<()> {
    crate::platform::ipc::Endpoint::from_native(endpoint).retire()
}

/// Path where the daemon records the identity consumed by
/// [`running_process::broker::protocol_v2::backend_handle::BackendHandle`].
#[must_use]
pub fn backend_identity_path() -> NormalizedPath {
    let namespace = zccache_core::config::daemon_namespace();
    if let Some(cache_dir) = normalized_override_root() {
        return cache_dir.join(backend_identity_file_name(namespace.as_deref()));
    }
    zccache_core::config::default_cache_dir().join(backend_identity_file_name(namespace.as_deref()))
}

/// #1003 — the single normalized cache identity for daemon ownership.
///
/// When the user pins a cache dir, `--cache-dir /foo` and
/// `--cache-dir /foo/v<version>` must resolve to the SAME daemon (same
/// endpoint, lock, and backend identity), or a second client is rejected as a
/// cache-dir mismatch (#770 / #771). Route the override through
/// `effective_cache_root_from_top_level` (idempotent — it won't double-append
/// the version) so both forms converge on one effective versioned root, and
/// derive the endpoint, lock, and backend-identity from THAT. Returns `None`
/// when no override is set (the default runtime/tmp/pipe location is used).
fn normalized_override_root() -> Option<NormalizedPath> {
    zccache_core::config::cache_dir_override()
        .map(|top| zccache_core::config::effective_cache_root_from_top_level(&top))
}

fn backend_identity_file_name(namespace: Option<&str>) -> String {
    match namespace {
        Some(ns) => format!("daemon-{ns}.running-process.json"),
        None => "daemon.running-process.json".to_string(),
    }
}

/// Convert zccache's direct daemon endpoint to the running-process endpoint
/// tuple used by `BackendHandle`.
///
/// Slice 24 of zccache#782: migrated to the `protocol_v2::backend_handle`
/// namespace (upstream PR #527). The underlying type is identical to v1's
/// per the coexistence re-export design — no behaviour change.
#[must_use]
pub fn running_process_endpoint(
    endpoint: &str,
) -> running_process::broker::protocol_v2::backend_handle::Endpoint {
    running_process::broker::protocol_v2::backend_handle::Endpoint {
        namespace_id: zccache_core::config::daemon_namespace_label(),
        path: running_process_endpoint_path(endpoint),
    }
}

fn running_process_endpoint_path(endpoint: &str) -> String {
    crate::platform::ipc::Endpoint::from_native(endpoint).to_running_process()
}

/// Build the current process identity that a zccache daemon exposes to
/// `BackendHandle` probes.
///
/// Slice 24 of zccache#782: migrated to the `protocol_v2::backend_handle`
/// namespace.
///
/// ISSUE-601 / #1511: the identity is keyed by endpoint and cached for the
/// process lifetime. The first call builds the same running-process wire
/// identity by streaming both required hashes in one pass instead of
/// `DaemonProcess::current_process`, whose `fs::read` temporarily allocated the
/// full daemon executable. First-call errors still bubble up; only successful
/// values are inserted, so a transient failure retries on the next call.
pub fn current_backend_identity(
    endpoint: &str,
) -> Result<
    running_process::broker::protocol_v2::backend_handle::DaemonProcess,
    running_process::broker::protocol_v2::backend_handle::IdentityError,
> {
    use std::sync::LazyLock;
    static IDENTITY_CACHE: LazyLock<
        dashmap::DashMap<
            String,
            std::sync::Arc<running_process::broker::protocol_v2::backend_handle::DaemonProcess>,
        >,
    > = LazyLock::new(dashmap::DashMap::new);

    if let Some(cached) = IDENTITY_CACHE.get(endpoint) {
        return Ok((**cached).clone());
    }

    let identity = current_process_identity_streaming(running_process_endpoint(endpoint))?;
    IDENTITY_CACHE.insert(endpoint.to_string(), std::sync::Arc::new(identity.clone()));
    Ok(identity)
}

const IDENTITY_HASH_BUFFER_BYTES: usize = 64 * 1024;

fn current_process_identity_streaming(
    ipc_endpoint: running_process::broker::protocol::Endpoint,
) -> Result<
    running_process::broker::protocol_v2::backend_handle::DaemonProcess,
    running_process::broker::protocol_v2::backend_handle::IdentityError,
> {
    use running_process::broker::protocol_v2::backend_handle::{DaemonProcess, IdentityError};

    let exe_path = std::env::current_exe().map_err(IdentityError::CurrentExe)?;
    let (exe_hash, legacy_exe_sha256) = executable_hashes_streaming(&exe_path)?;
    let started_at_unix_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0);

    Ok(DaemonProcess {
        pid: std::process::id(),
        exe_path,
        exe_hash,
        legacy_exe_sha256,
        boot_id: running_process::broker::host_identity::current().boot_id,
        ipc_endpoint,
        started_at_unix_ms,
        idle_timeout_secs: None,
    })
}

fn executable_hashes_streaming(path: &std::path::Path) -> std::io::Result<([u8; 32], [u8; 32])> {
    use sha2::{Digest as _, Sha256};
    use std::io::Read as _;

    let mut file = std::fs::File::open(path)?;
    let mut blake3 = blake3::Hasher::new();
    let mut sha256 = Sha256::new();
    let mut buffer = [0_u8; IDENTITY_HASH_BUFFER_BYTES];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        blake3.update(&buffer[..read]);
        sha256.update(&buffer[..read]);
    }
    Ok((*blake3.finalize().as_bytes(), sha256.finalize().into()))
}

/// Persist the daemon identity used by future `BackendHandle` probes.
///
/// Slice 24 of zccache#782: migrated to the `protocol_v2::backend_handle`
/// namespace.
pub fn write_backend_identity(
    daemon: &running_process::broker::protocol_v2::backend_handle::DaemonProcess,
) -> Result<(), std::io::Error> {
    let path = backend_identity_path();
    if let Some(parent) = path.parent() {
        // #1171: same directory family as the socket endpoint.
        zccache_core::config::create_dir_all_private(parent)?;
    }
    let json = serde_json::to_vec_pretty(daemon)
        .map_err(|err| std::io::Error::other(format!("serialize backend identity: {err}")))?;
    std::fs::write(path, json)
}

/// Read the persisted daemon identity, if one is recorded and parseable.
///
/// #1161: the identity has been *written* on every daemon start for a long
/// time, and nothing read it back except `probe_backend_handle`'s inline load.
/// Kill decisions verified only "PID is alive and its exe stem is
/// `zccache-daemon`" — which a recycled PID belonging to a *different*
/// zccache-daemon satisfies, so auto-recovery could kill an unrelated live
/// instance.
///
/// `DaemonProcess` already carries what distinguishes instances:
/// `started_at_unix_ms` (PID reuse within a boot) and `boot_id` (across
/// boots). Exposing the read is what lets a kill be bound to the instance the
/// caller actually failed to talk to.
///
/// `None` means "nothing recorded, or unreadable" — deliberately *not*
/// "matches anything". See [`daemon_identity_matches`].
#[must_use]
pub fn read_backend_identity(
) -> Option<running_process::broker::protocol_v2::backend_handle::DaemonProcess> {
    std::fs::read(backend_identity_path())
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
}

/// Is the daemon recorded on disk right now the same *instance* as `expected`?
///
/// Compares PID **and** start time **and** boot id. PID alone is not identity:
/// the OS reuses PIDs, aggressively so on Windows, and the exe-stem check that
/// guarded this before is satisfied by any `zccache-daemon` — including one
/// serving a different namespace.
///
/// Returns `false` when nothing is recorded. That is the safe direction for a
/// kill gate: refusing costs one clear error about a daemon that is already
/// not answering, while permitting costs killing a live daemon that was never
/// the one at fault. Note this deliberately differs from
/// [`verify_pid_exe_stem`], whose `None => true` fallback is about *reading an
/// exe path* on platforms that cannot — not about authorising a kill.
#[must_use]
pub fn daemon_identity_matches(
    expected: &running_process::broker::protocol_v2::backend_handle::DaemonProcess,
) -> bool {
    let Some(current) = read_backend_identity() else {
        return false;
    };
    current.pid == expected.pid
        && current.started_at_unix_ms == expected.started_at_unix_ms
        && current.boot_id == expected.boot_id
}

/// Load and actively verify the daemon identity through `BackendHandle`.
///
/// Slice 24 of zccache#782: migrated to the `protocol_v2::backend_handle`
/// namespace.
#[must_use]
pub fn probe_backend_handle(
    endpoint: &str,
) -> Option<running_process::broker::protocol_v2::backend_handle::BackendHandle> {
    let daemon = read_backend_identity()?;
    let endpoint = running_process_endpoint(endpoint);
    running_process::broker::protocol_v2::backend_handle::BackendHandle::probe_with_service(
        "zccache",
        zccache_core::VERSION,
        &endpoint,
        &daemon,
    )
    .ok()
}

/// Broker escape hatch shared with the running-process rollout plan.
pub const RUNNING_PROCESS_DISABLE_ENV: &str = "RUNNING_PROCESS_DISABLE";

#[must_use]
pub fn running_process_disabled() -> bool {
    std::env::var(RUNNING_PROCESS_DISABLE_ENV).is_ok_and(|value| value == "1")
}

/// Forcefully terminate a process by PID.
///
/// This is intended as a last-resort escape hatch when the daemon is no longer
/// reachable over IPC, so graceful shutdown is not possible.
pub fn force_kill_process(pid: u32) -> Result<(), std::io::Error> {
    crate::platform::process::terminate::force(pid)
}

/// Check if a process with the given PID is actually running.
///
/// On Windows this is stricter than "the kernel still has a process object for
/// this PID": the function returns `false` for a terminated process whose
/// process object is being kept alive by some other handle holder (Task
/// Manager, Process Explorer, a sibling tool that called `OpenProcess` for
/// monitoring, etc.). Plain `OpenProcess` success is *not* sufficient because
/// the object can outlive the actual process by an arbitrary amount of time;
/// see issue #774 where this caused `taskkill /F` on `zccache-daemon` to leave
/// the CLI looping against a dead PID until manual cleanup.
///
/// We disambiguate with `WaitForSingleObject(handle, 0)`: the process object
/// becomes signaled at termination, so `WAIT_TIMEOUT` (still waiting) is the
/// unambiguous "actually running" signal. Using `WaitForSingleObject` rather
/// than `GetExitCodeProcess` also sidesteps the documented Windows wart where
/// a process that genuinely exited with code 259 is indistinguishable from
/// one that is still running.
#[must_use]
pub fn is_process_alive(pid: u32) -> bool {
    crate::platform::process::inspect::is_alive(pid)
}

/// Probe whether a daemon is **already serving** at `endpoint`. Returns
/// `true` iff **all** of the following hold:
///
/// 1. The lock file records a PID.
/// 2. That PID is alive AND its executable is `zccache-daemon` (defends
///    against recycled PIDs — see [`verify_daemon_pid`]).
/// 3. We can complete an IPC connect to `endpoint` within `timeout`.
///
/// **Why this exists** (issue #640): on Windows, parallel `ninja -jN`
/// builds create a thundering-herd race where every newly-spawned
/// daemon pays the 3+ s depgraph-load cost BEFORE attempting to bind
/// the named pipe. Second-wave daemons (those spawned after a
/// previous daemon has already won the bind and registered its lock
/// file) can short-circuit here without paying the load cost — they
/// see the live PID + working endpoint and exit 0 cleanly. First-wave
/// daemons (the initial cohort racing for the bind before anyone has
/// registered) still go through the existing bind error-discrimination
/// path landed in #639.
///
/// The connection returned by `connect` is dropped immediately on a
/// successful probe — we are only verifying that the other end is
/// accepting, not exchanging any application-level message. Returning
/// the connection would make this function harder to use (callers
/// can't drop it without an explicit shutdown handshake) and the
/// extra round-trip is wasted work for the common case where the
/// caller is the daemon itself and is about to exit.
///
/// `timeout` caps the worst case. Pick a value that's small relative
/// to the cost we're avoiding (3+ s depgraph load) but large enough
/// to absorb normal connect latency under load (typically <50 ms on
/// a local pipe).
pub async fn probe_existing_daemon(endpoint: &str, timeout: std::time::Duration) -> bool {
    let Some(pid) = read_lock_file_pid() else {
        return false;
    };
    // Don't probe ourselves — the post-fork daemon's own PID could be
    // recorded in the lock file by a sibling racing-init thread under
    // pathological conditions; treating self as "another daemon" would
    // be a deadlock.
    if pid == std::process::id() {
        return false;
    }
    if !verify_daemon_pid(pid) {
        return false;
    }
    // RUNNING_PROCESS_DISABLE=1 is the upstream broker rollout escape hatch:
    // skip the BackendHandle probe but keep the existing direct IPC fallback.
    if !running_process_disabled() && probe_backend_handle(endpoint).is_some() {
        return true;
    }
    match tokio::time::timeout(timeout, crate::connect(endpoint)).await {
        Ok(Ok(_conn)) => true,
        // Connection refused, pipe not yet listening, or any other IPC error:
        // treat as "no live daemon" and let the caller proceed with full init.
        Ok(Err(_)) | Err(_) => false,
    }
}

/// Returns true if `pid` exists **and** its executable looks like a zccache
/// daemon. Defends against stale `daemon.lock` files where the recorded PID has
/// been recycled by an unrelated process — typical when a CI runner restores a
/// cache directory containing a lock file from a prior, abruptly-terminated
/// run. Without this check, [`check_running_daemon`] would mis-identify the
/// recycled PID as our daemon and callers like `zccache stop` would
/// `force_kill_process` an arbitrary system process. See issue #132.
#[must_use]
pub fn verify_daemon_pid(pid: u32) -> bool {
    verify_pid_exe_stem(pid, "zccache-daemon")
}

/// Generic version of [`verify_daemon_pid`]: confirms `pid` is alive and its
/// executable filename (without `.exe`) matches `expected_stem`. Used by
/// callers that own a different daemon binary (e.g. the download daemon).
#[must_use]
pub fn verify_pid_exe_stem(pid: u32, expected_stem: &str) -> bool {
    if !is_process_alive(pid) {
        return false;
    }
    match crate::platform::process::inspect::executable_path(pid) {
        // Got an exe path — only trust the PID if it points at our daemon.
        Some(exe) => exe_stem_matches(&exe, expected_stem),
        // Platform doesn't support reading the exe path. Fall back to the
        // existing alive-only behavior so we don't regress on those platforms.
        None => true,
    }
}

fn exe_stem_matches(path: &std::path::Path, expected_stem: &str) -> bool {
    let Some(name) = path.file_name() else {
        return false;
    };
    let name = name.to_string_lossy();
    let stem = name.strip_suffix(".exe").unwrap_or(&name);
    stem == expected_stem
}

/// Check if a daemon is already running. Returns the PID if alive.
#[must_use]
pub fn check_running_daemon() -> Option<u32> {
    let pid = read_lock_file_pid()?;
    if verify_daemon_pid(pid) {
        Some(pid)
    } else {
        // Stale lock file — clean up. The PID may be dead, or may belong to
        // an unrelated process that recycled the lock file's PID (issue #132).
        remove_lock_file();
        let endpoint = crate::platform::ipc::Endpoint::from_native(default_endpoint());
        let _ = endpoint.retire();
        None
    }
}

/// Shared test-only environment-variable coordination for the `ipc` module
/// tree. Every test that mutates process env vars must hold [`ENV_LOCK`]
/// (directly or through a guard) so unit tests in sibling modules cannot
/// race each other's env mutations.
#[cfg(test)]
pub(crate) mod test_env {
    use std::ffi::OsString;
    use std::sync::{Mutex, MutexGuard};

    pub(crate) static ENV_LOCK: Mutex<()> = Mutex::new(());

    /// Guard that sets/unsets a batch of env vars under the shared lock and
    /// restores the previous values on drop.
    pub(crate) struct EnvVarGuard {
        _lock: MutexGuard<'static, ()>,
        saved: Vec<(&'static str, Option<OsString>)>,
    }

    impl EnvVarGuard {
        pub(crate) fn set_all(vars: &[(&'static str, Option<String>)]) -> Self {
            let lock = ENV_LOCK
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let saved = vars
                .iter()
                .map(|(key, _)| (*key, std::env::var_os(key)))
                .collect();
            for (key, value) in vars {
                match value {
                    Some(value) => std::env::set_var(key, value),
                    None => std::env::remove_var(key),
                }
            }
            Self { _lock: lock, saved }
        }

        pub(crate) fn unset_all(keys: &[&'static str]) -> Self {
            let vars: Vec<(&'static str, Option<String>)> =
                keys.iter().map(|key| (*key, None)).collect();
            Self::set_all(&vars)
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            for (key, value) in &self.saved {
                match value {
                    Some(value) => std::env::set_var(key, value),
                    None => std::env::remove_var(key),
                }
            }
        }
    }
}

#[cfg(test)]
#[path = "mod_tests.rs"]
mod tests;
