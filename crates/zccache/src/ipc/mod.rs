//! IPC transport layer for zccache.
//!
//! Provides platform-abstracted IPC between CLI/compiler wrapper
//! and the daemon, using Unix domain sockets on Unix and named
//! pipes on Windows.

#![allow(clippy::missing_errors_doc)]

pub mod broker;
pub mod error;
pub mod manifest;
pub mod transport;

pub use broker::{
    connect_daemon, connect_daemon_with_route, to_running_process_endpoint, BrokerRefusal,
    DaemonConnectRoute, ZCCACHE_BROKER_CONNECT_ENV,
};
pub use error::IpcError;
pub use manifest::{publish_manifest, publish_manifest_in};
#[cfg(windows)]
pub use transport::IpcClientConnection;
pub use transport::{
    connect, unique_test_endpoint, IpcConnection, IpcListener, DEFAULT_CLIENT_RECV_TIMEOUT,
};

use crate::core::NormalizedPath;
use crate::protocol::{self, wire_prost, Response};

#[cfg(unix)]
const MAX_PORTABLE_UNIX_SOCKET_PATH_BYTES: usize = 100;

#[cfg(unix)]
type ClientConnection = IpcConnection;
#[cfg(windows)]
type ClientConnection = IpcClientConnection;

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

/// Send a daemon control request and receive its response.
///
/// Only `Ping`, `Status`, `Shutdown`, and `Clear` are eligible for the v16
/// prost client path. Unset/`auto` `ZCCACHE_DAEMON_WIRE` prefers prost, then
/// retries the same control request as v15 bincode if an older daemon clearly
/// rejects the v16 frame or closes the connection after the mismatch. Compile,
/// session, artifact lookup/store, fingerprint, and download-daemon requests do
/// not route through this helper and remain v15 bincode.
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
                Ok(Some(Response::Error { message }))
                    if selection.allows_bincode_fallback()
                        && control_wire_mismatch_message(&message) =>
                {
                    send_bincode_control(endpoint, request, recv_timeout).await
                }
                Ok(None) if selection.allows_bincode_fallback() => {
                    send_bincode_control(endpoint, request, recv_timeout).await
                }
                Ok(response) => Ok(response),
                Err(err)
                    if selection.allows_bincode_fallback() && control_wire_mismatch_error(&err) =>
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
    recv_control_wire_response(&mut conn, recv_timeout).await
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

fn control_wire_mismatch_error(err: &IpcError) -> bool {
    match err {
        IpcError::Protocol(protocol::ProtocolError::VersionMismatch { .. })
        | IpcError::ConnectionClosed => true,
        IpcError::Io(io) => matches!(
            io.kind(),
            std::io::ErrorKind::BrokenPipe
                | std::io::ErrorKind::ConnectionReset
                | std::io::ErrorKind::UnexpectedEof
        ),
        IpcError::Protocol(_) | IpcError::Endpoint(_) | IpcError::Timeout(_) => false,
    }
}

fn control_wire_mismatch_message(message: &str) -> bool {
    let message = message.to_ascii_lowercase();
    message.contains("protocol version mismatch")
        || message.contains("protocol_version")
        || (message.contains("expected v15") && message.contains("received v16"))
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
    let namespace = crate::core::config::daemon_namespace();
    if let Some(cache_dir) = crate::core::config::cache_dir_override() {
        return endpoint_for_cache_dir(&cache_dir, namespace.as_deref());
    }

    #[cfg(unix)]
    {
        if let Ok(runtime_dir) = std::env::var("XDG_RUNTIME_DIR") {
            return format!(
                "{runtime_dir}/zccache/{}",
                socket_name(namespace.as_deref())
            );
        }
        let user = std::env::var("USER").unwrap_or_else(|_| String::from("unknown"));
        format!("/tmp/zccache-{user}/{}", socket_name(namespace.as_deref()))
    }
    #[cfg(windows)]
    {
        let username = std::env::var("USERNAME").unwrap_or_else(|_| String::from("unknown"));
        pipe_name(&username, namespace.as_deref())
    }
}

pub fn endpoint_for_cache_dir(cache_dir: &std::path::Path, namespace: Option<&str>) -> String {
    #[cfg(unix)]
    {
        let direct = cache_dir.join(daemon_socket_name(namespace));
        let direct = direct.to_string_lossy();
        if direct.len() <= MAX_PORTABLE_UNIX_SOCKET_PATH_BYTES {
            return direct.into_owned();
        }

        compact_cache_dir_endpoint(cache_dir, namespace)
    }
    #[cfg(windows)]
    {
        let suffix = crate::core::stable_path_id(cache_dir);
        pipe_name(&suffix, namespace)
    }
}

#[cfg(unix)]
fn compact_cache_dir_endpoint(cache_dir: &std::path::Path, namespace: Option<&str>) -> String {
    // Endpoint is a Unix socket path; return it as a `String` directly so
    // we don't round-trip through `PathBuf` only to immediately convert
    // back via `to_string_lossy`. The previous shape was the only
    // `ban_std_pathbuf` lint hit in this file.
    let cache_id = crate::core::stable_path_id(cache_dir);
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
    let namespace = crate::core::config::sanitize_daemon_namespace(daemon_name)
        .unwrap_or_else(|| crate::core::config::DEV_DAEMON_NAMESPACE.to_string());
    if let Some(cache_dir) = cache_dir {
        return endpoint_for_cache_dir(cache_dir, Some(&namespace));
    }

    #[cfg(unix)]
    {
        if let Ok(runtime_dir) = std::env::var("XDG_RUNTIME_DIR") {
            return format!("{runtime_dir}/zccache/{}", socket_name(Some(&namespace)));
        }
        let user = std::env::var("USER").unwrap_or_else(|_| String::from("unknown"));
        format!("/tmp/zccache-{user}/{}", socket_name(Some(&namespace)))
    }
    #[cfg(windows)]
    {
        let username = std::env::var("USERNAME").unwrap_or_else(|_| String::from("unknown"));
        pipe_name(&username, Some(&namespace))
    }
}

/// Returns the path for the daemon lock file.
#[must_use]
pub fn lock_file_path() -> NormalizedPath {
    let namespace = crate::core::config::daemon_namespace();
    if let Some(cache_dir) = crate::core::config::cache_dir_override() {
        return cache_dir.join(lock_file_name(namespace.as_deref()));
    }

    #[cfg(unix)]
    {
        let endpoint = default_endpoint();
        let dir = std::path::Path::new(&endpoint)
            .parent()
            .expect("endpoint should have parent directory");
        dir.join(lock_file_name(namespace.as_deref())).into()
    }
    #[cfg(windows)]
    {
        crate::core::config::default_cache_dir().join(lock_file_name(namespace.as_deref()))
    }
}

#[cfg(unix)]
fn socket_name(namespace: Option<&str>) -> String {
    match namespace {
        Some(ns) => format!("sock-{ns}"),
        None => "sock".to_string(),
    }
}

#[cfg(unix)]
fn daemon_socket_name(namespace: Option<&str>) -> String {
    match namespace {
        Some(ns) => format!("daemon-{ns}.sock"),
        None => "daemon.sock".to_string(),
    }
}

#[cfg(windows)]
fn pipe_name(base: &str, namespace: Option<&str>) -> String {
    let base = crate::core::config::sanitize_ipc_component(base)
        .unwrap_or_else(|| String::from("unknown"));
    match namespace {
        Some(ns) => format!(r"\\.\pipe\zccache-{base}-{ns}"),
        None => format!(r"\\.\pipe\zccache-{base}"),
    }
}

fn lock_file_name(namespace: Option<&str>) -> String {
    match namespace {
        Some(ns) => format!("daemon-{ns}.lock"),
        None => "daemon.lock".to_string(),
    }
}

/// Write the daemon PID to the lock file.
///
/// Creates parent directories if needed.
pub fn write_lock_file(pid: u32) -> Result<(), std::io::Error> {
    let path = lock_file_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
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

/// Path where the daemon records the identity consumed by
/// `running_process::broker::backend_handle::BackendHandle`.
#[must_use]
pub fn backend_identity_path() -> NormalizedPath {
    let namespace = crate::core::config::daemon_namespace();
    if let Some(cache_dir) = crate::core::config::cache_dir_override() {
        return cache_dir.join(backend_identity_file_name(namespace.as_deref()));
    }
    crate::core::config::default_cache_dir().join(backend_identity_file_name(namespace.as_deref()))
}

fn backend_identity_file_name(namespace: Option<&str>) -> String {
    match namespace {
        Some(ns) => format!("daemon-{ns}.running-process.json"),
        None => "daemon.running-process.json".to_string(),
    }
}

/// Convert zccache's direct daemon endpoint to the running-process endpoint
/// tuple used by `BackendHandle`.
#[must_use]
pub fn running_process_endpoint(endpoint: &str) -> running_process::broker::protocol::Endpoint {
    running_process::broker::protocol::Endpoint {
        namespace_id: crate::core::config::daemon_namespace_label(),
        path: running_process_endpoint_path(endpoint),
    }
}

#[cfg(windows)]
fn running_process_endpoint_path(endpoint: &str) -> String {
    endpoint
        .strip_prefix(r"\\.\pipe\")
        .unwrap_or(endpoint)
        .to_string()
}

#[cfg(unix)]
fn running_process_endpoint_path(endpoint: &str) -> String {
    endpoint.to_string()
}

/// Build the current process identity that a zccache daemon exposes to
/// `BackendHandle` probes.
pub fn current_backend_identity(
    endpoint: &str,
) -> Result<
    running_process::broker::backend_handle::DaemonProcess,
    running_process::broker::backend_lifecycle::identity::IdentityError,
> {
    running_process::broker::backend_handle::DaemonProcess::current_process(
        running_process_endpoint(endpoint),
        None,
    )
}

/// Persist the daemon identity used by future `BackendHandle` probes.
pub fn write_backend_identity(
    daemon: &running_process::broker::backend_handle::DaemonProcess,
) -> Result<(), std::io::Error> {
    let path = backend_identity_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_vec_pretty(daemon)
        .map_err(|err| std::io::Error::other(format!("serialize backend identity: {err}")))?;
    std::fs::write(path, json)
}

/// Load and actively verify the daemon identity through `BackendHandle`.
#[must_use]
pub fn probe_backend_handle(
    endpoint: &str,
) -> Option<running_process::broker::backend_handle::BackendHandle> {
    let daemon = std::fs::read(backend_identity_path())
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())?;
    let endpoint = running_process_endpoint(endpoint);
    running_process::broker::backend_handle::BackendHandle::probe_with_service(
        "zccache",
        crate::core::VERSION,
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
    #[cfg(unix)]
    {
        // SAFETY: kill is called with a PID provided by the caller and a fixed
        // signal value. No pointers are involved.
        extern "C" {
            fn kill(pid: i32, sig: i32) -> i32;
        }
        const SIGKILL: i32 = 9;
        let rc = unsafe { kill(pid as i32, SIGKILL) };
        if rc == 0 {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error())
        }
    }
    #[cfg(windows)]
    {
        // windows-sys defines CloseHandle/OpenProcess/TerminateProcess with
        // HANDLE/BOOL newtypes; our local extern uses the underlying isize/i32
        // for ergonomics. Same ABI, different signature in the type-system,
        // so the linker accepts both but rustc warns. -D warnings on CI
        // promotes the warn to error.
        #[allow(clashing_extern_declarations)]
        extern "system" {
            fn OpenProcess(access: u32, inherit: i32, pid: u32) -> isize;
            fn TerminateProcess(handle: isize, exit_code: u32) -> i32;
            fn CloseHandle(handle: isize) -> i32;
        }
        const PROCESS_TERMINATE: u32 = 0x0001;
        const SYNCHRONIZE: u32 = 0x0010_0000;
        unsafe {
            let handle = OpenProcess(PROCESS_TERMINATE | SYNCHRONIZE, 0, pid);
            if handle == 0 {
                return Err(std::io::Error::last_os_error());
            }
            let result = TerminateProcess(handle, 1);
            let err = if result == 0 {
                Some(std::io::Error::last_os_error())
            } else {
                None
            };
            CloseHandle(handle);
            match err {
                Some(err) => Err(err),
                None => Ok(()),
            }
        }
    }
}

/// Check if a process with the given PID is alive.
#[must_use]
pub fn is_process_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        // SAFETY: kill(pid, 0) is a standard POSIX call that checks process
        // existence without sending any signal.
        extern "C" {
            fn kill(pid: i32, sig: i32) -> i32;
        }
        unsafe { kill(pid as i32, 0) == 0 }
    }
    #[cfg(windows)]
    {
        // See CloseHandle note in force_kill_process above.
        #[allow(clashing_extern_declarations)]
        extern "system" {
            fn OpenProcess(access: u32, inherit: i32, pid: u32) -> isize;
            fn CloseHandle(handle: isize) -> i32;
        }
        const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
        unsafe {
            let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if handle != 0 {
                CloseHandle(handle);
                true
            } else {
                false
            }
        }
    }
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
    match tokio::time::timeout(timeout, crate::ipc::connect(endpoint)).await {
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
    match daemon_exe_for_pid(pid) {
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

#[cfg(target_os = "linux")]
fn daemon_exe_for_pid(pid: u32) -> Option<NormalizedPath> {
    std::fs::read_link(format!("/proc/{pid}/exe"))
        .ok()
        .map(NormalizedPath::from)
}

#[cfg(target_os = "macos")]
fn daemon_exe_for_pid(pid: u32) -> Option<NormalizedPath> {
    // `proc_pidpath` from libproc (`libSystem.dylib`) — same one
    // `ps`/`lsof` use under the hood. Available on macOS 10.5+.
    //
    // PROC_PIDPATHINFO_MAXSIZE is documented as 4 * MAXPATHLEN (= 4096)
    // in `<sys/proc_info.h>`. Allocate exactly that and let the call
    // tell us how many bytes it wrote.
    const PROC_PIDPATHINFO_MAXSIZE: usize = 4096;

    extern "C" {
        fn proc_pidpath(pid: i32, buf: *mut std::ffi::c_void, bufsize: u32) -> i32;
    }

    let mut buf = vec![0u8; PROC_PIDPATHINFO_MAXSIZE];
    // SAFETY: pid is a u32 from the caller, buf is a freshly-allocated
    // Vec we own. bufsize matches the allocation size. proc_pidpath
    // returns the number of bytes written (>0) or -1 on error and is
    // tolerant of stale PIDs (returns ESRCH).
    let written = unsafe { proc_pidpath(pid as i32, buf.as_mut_ptr().cast(), buf.len() as u32) };
    if written <= 0 {
        // EPERM (process belongs to another user), ESRCH (pid gone), etc.
        // Don't trust the PID — recycled-PID defense fires.
        return None;
    }
    buf.truncate(written as usize);
    let s = std::str::from_utf8(&buf).ok()?;
    Some(NormalizedPath::from(std::path::PathBuf::from(s)))
}

#[cfg(windows)]
fn daemon_exe_for_pid(pid: u32) -> Option<NormalizedPath> {
    // See CloseHandle note in force_kill_process above.
    #[allow(clashing_extern_declarations)]
    extern "system" {
        fn OpenProcess(access: u32, inherit: i32, pid: u32) -> isize;
        fn CloseHandle(handle: isize) -> i32;
        fn QueryFullProcessImageNameW(
            handle: isize,
            flags: u32,
            buffer: *mut u16,
            size: *mut u32,
        ) -> i32;
    }
    const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;

    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if handle == 0 {
            return None;
        }
        let mut buf = vec![0u16; 32_768];
        let mut size = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(handle, 0, buf.as_mut_ptr(), &mut size);
        CloseHandle(handle);
        if ok == 0 {
            return None;
        }
        use std::os::windows::ffi::OsStringExt;
        let os = std::ffi::OsString::from_wide(&buf[..size as usize]);
        Some(NormalizedPath::new(&os))
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
fn daemon_exe_for_pid(_pid: u32) -> Option<NormalizedPath> {
    None
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
        #[cfg(unix)]
        {
            // Also remove stale socket on Unix
            let endpoint = default_endpoint();
            let _ = std::fs::remove_file(&endpoint);
        }
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
mod tests {
    use super::test_env::ENV_LOCK;
    use super::*;
    use std::ffi::OsString;
    use std::sync::MutexGuard;

    struct EnvGuard {
        _lock: MutexGuard<'static, ()>,
        previous_cache_dir: Option<OsString>,
        previous_namespace: Option<OsString>,
        previous_running_process_disable: Option<OsString>,
    }

    impl EnvGuard {
        fn set_cache_dir(value: &std::path::Path) -> Self {
            let lock = ENV_LOCK
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let previous_cache_dir = std::env::var_os(crate::core::config::CACHE_DIR_ENV);
            let previous_namespace = std::env::var_os(crate::core::config::DAEMON_NAMESPACE_ENV);
            let previous_running_process_disable = std::env::var_os(RUNNING_PROCESS_DISABLE_ENV);
            std::env::set_var(crate::core::config::CACHE_DIR_ENV, value);
            std::env::remove_var(crate::core::config::DAEMON_NAMESPACE_ENV);
            Self {
                _lock: lock,
                previous_cache_dir,
                previous_namespace,
                previous_running_process_disable,
            }
        }

        fn set_cache_dir_and_namespace(value: &std::path::Path, namespace: &str) -> Self {
            let lock = ENV_LOCK
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let previous_cache_dir = std::env::var_os(crate::core::config::CACHE_DIR_ENV);
            let previous_namespace = std::env::var_os(crate::core::config::DAEMON_NAMESPACE_ENV);
            let previous_running_process_disable = std::env::var_os(RUNNING_PROCESS_DISABLE_ENV);
            std::env::set_var(crate::core::config::CACHE_DIR_ENV, value);
            std::env::set_var(crate::core::config::DAEMON_NAMESPACE_ENV, namespace);
            Self {
                _lock: lock,
                previous_cache_dir,
                previous_namespace,
                previous_running_process_disable,
            }
        }

        fn isolate_running_process_disable() -> Self {
            let lock = ENV_LOCK
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let previous_cache_dir = std::env::var_os(crate::core::config::CACHE_DIR_ENV);
            let previous_namespace = std::env::var_os(crate::core::config::DAEMON_NAMESPACE_ENV);
            let previous_running_process_disable = std::env::var_os(RUNNING_PROCESS_DISABLE_ENV);
            std::env::remove_var(RUNNING_PROCESS_DISABLE_ENV);
            Self {
                _lock: lock,
                previous_cache_dir,
                previous_namespace,
                previous_running_process_disable,
            }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match &self.previous_cache_dir {
                Some(value) => std::env::set_var(crate::core::config::CACHE_DIR_ENV, value),
                None => std::env::remove_var(crate::core::config::CACHE_DIR_ENV),
            }
            match &self.previous_namespace {
                Some(value) => std::env::set_var(crate::core::config::DAEMON_NAMESPACE_ENV, value),
                None => std::env::remove_var(crate::core::config::DAEMON_NAMESPACE_ENV),
            }
            match &self.previous_running_process_disable {
                Some(value) => std::env::set_var(RUNNING_PROCESS_DISABLE_ENV, value),
                None => std::env::remove_var(RUNNING_PROCESS_DISABLE_ENV),
            }
        }
    }

    fn test_daemon_status(endpoint: &str) -> crate::protocol::DaemonStatus {
        crate::protocol::DaemonStatus {
            version: crate::core::VERSION.to_string(),
            daemon_namespace: "test".to_string(),
            endpoint: endpoint.to_string(),
            private_daemon: crate::protocol::PrivateDaemonStatus::shared(),
            artifact_count: 0,
            cache_size_bytes: 0,
            metadata_entries: 0,
            uptime_secs: 1,
            cache_hits: 0,
            cache_misses: 0,
            total_compilations: 0,
            non_cacheable: 0,
            compile_errors: 0,
            compile_errors_cached: 0,
            time_saved_ms: 0,
            total_links: 0,
            link_hits: 0,
            link_misses: 0,
            link_non_cacheable: 0,
            dep_graph_contexts: 0,
            dep_graph_files: 0,
            sessions_total: 0,
            sessions_active: 0,
            cache_dir: std::env::temp_dir().into(),
            dep_graph_version: crate::depgraph::DEPGRAPH_VERSION,
            dep_graph_disk_size: 0,
            dep_graph_persisted: false,
        }
    }

    #[tokio::test]
    async fn daemon_control_roundtrip_auto_prefers_prost_for_status() {
        let endpoint = unique_test_endpoint();
        let mut listener = IpcListener::bind(&endpoint).unwrap();
        let expected_endpoint = endpoint.clone();

        let server = tokio::spawn(async move {
            let mut conn = listener.accept().await.unwrap();
            let msg: Option<
                crate::protocol::DecodedWireMessage<
                    crate::protocol::Request,
                    crate::protocol::wire_prost::zccache_v1::Request,
                >,
            > = conn.recv_wire().await.unwrap();
            match msg {
                Some(crate::protocol::DecodedWireMessage::ProstV16(request)) => {
                    assert_eq!(request.request_id, "control-status");
                    assert!(matches!(
                        request.body,
                        Some(crate::protocol::wire_prost::zccache_v1::request::Body::Status(_))
                    ));
                    let response = Response::Status(test_daemon_status(&expected_endpoint));
                    let response = wire_prost::supported_control_response_to_prost(
                        &response,
                        &request.request_id,
                    )
                    .unwrap();
                    conn.send_prost(&response).await.unwrap();
                }
                other => panic!("expected prost status request, got {other:?}"),
            }
        });

        let response = daemon_control_roundtrip_with_selection(
            &endpoint,
            DaemonControlRequest::Status,
            None,
            wire_prost::ClientWireSelection::Auto,
        )
        .await
        .unwrap();

        match response {
            Some(Response::Status(status)) => assert_eq!(status.endpoint, endpoint),
            other => panic!("expected Status response, got {other:?}"),
        }

        server.await.unwrap();
    }

    #[tokio::test]
    async fn daemon_control_roundtrip_auto_prefers_prost_for_clear() {
        let endpoint = unique_test_endpoint();
        let mut listener = IpcListener::bind(&endpoint).unwrap();

        let server = tokio::spawn(async move {
            let mut conn = listener.accept().await.unwrap();
            let msg: Option<
                crate::protocol::DecodedWireMessage<
                    crate::protocol::Request,
                    crate::protocol::wire_prost::zccache_v1::Request,
                >,
            > = conn.recv_wire().await.unwrap();
            match msg {
                Some(crate::protocol::DecodedWireMessage::ProstV16(request)) => {
                    assert_eq!(request.request_id, "control-clear");
                    assert!(matches!(
                        request.body,
                        Some(crate::protocol::wire_prost::zccache_v1::request::Body::Clear(_))
                    ));
                    let response = Response::Cleared {
                        artifacts_removed: 1,
                        metadata_cleared: 2,
                        dep_graph_contexts_cleared: 3,
                        on_disk_bytes_freed: 4,
                    };
                    let response = wire_prost::supported_control_response_to_prost(
                        &response,
                        &request.request_id,
                    )
                    .unwrap();
                    conn.send_prost(&response).await.unwrap();
                }
                other => panic!("expected prost clear request, got {other:?}"),
            }
        });

        let response = daemon_control_roundtrip_with_selection(
            &endpoint,
            DaemonControlRequest::Clear,
            None,
            wire_prost::ClientWireSelection::Auto,
        )
        .await
        .unwrap();

        match response {
            Some(Response::Cleared {
                artifacts_removed,
                metadata_cleared,
                dep_graph_contexts_cleared,
                on_disk_bytes_freed,
            }) => {
                assert_eq!(artifacts_removed, 1);
                assert_eq!(metadata_cleared, 2);
                assert_eq!(dep_graph_contexts_cleared, 3);
                assert_eq!(on_disk_bytes_freed, 4);
            }
            other => panic!("expected Cleared response, got {other:?}"),
        }

        server.await.unwrap();
    }

    #[tokio::test]
    async fn daemon_control_roundtrip_bincode_selection_stays_v15_for_status() {
        let endpoint = unique_test_endpoint();
        let mut listener = IpcListener::bind(&endpoint).unwrap();
        let expected_endpoint = endpoint.clone();

        let server = tokio::spawn(async move {
            let mut conn = listener.accept().await.unwrap();
            let request: Option<crate::protocol::Request> = conn.recv().await.unwrap();
            assert_eq!(request, Some(crate::protocol::Request::Status));
            conn.send(&Response::Status(test_daemon_status(&expected_endpoint)))
                .await
                .unwrap();
        });

        let response = daemon_control_roundtrip_with_selection(
            &endpoint,
            DaemonControlRequest::Status,
            None,
            wire_prost::ClientWireSelection::BincodeV15,
        )
        .await
        .unwrap();

        match response {
            Some(Response::Status(status)) => assert_eq!(status.endpoint, endpoint),
            other => panic!("expected bincode Status response, got {other:?}"),
        }

        server.await.unwrap();
    }

    #[tokio::test]
    async fn daemon_control_roundtrip_auto_falls_back_to_bincode_for_old_daemon() {
        let endpoint = unique_test_endpoint();
        let mut listener = IpcListener::bind(&endpoint).unwrap();
        let expected_endpoint = endpoint.clone();

        let server = tokio::spawn(async move {
            let mut first = listener.accept().await.unwrap();
            let err = first
                .recv::<crate::protocol::Request>()
                .await
                .expect_err("v16 prost request must not decode as v15 bincode");
            assert!(matches!(
                err,
                IpcError::Protocol(crate::protocol::ProtocolError::VersionMismatch {
                    expected: crate::protocol::BINCODE_PROTOCOL_VERSION,
                    received: crate::protocol::PROST_PROTOCOL_VERSION,
                })
            ));
            first
                .send(&Response::Error {
                    message: "protocol version mismatch: expected v15, received v16".to_string(),
                })
                .await
                .unwrap();

            let mut second = listener.accept().await.unwrap();
            let request: Option<crate::protocol::Request> = second.recv().await.unwrap();
            assert_eq!(request, Some(crate::protocol::Request::Status));
            second
                .send(&Response::Status(test_daemon_status(&expected_endpoint)))
                .await
                .unwrap();
        });

        let response = daemon_control_roundtrip_with_selection(
            &endpoint,
            DaemonControlRequest::Status,
            None,
            wire_prost::ClientWireSelection::Auto,
        )
        .await
        .unwrap();

        match response {
            Some(Response::Status(status)) => assert_eq!(status.endpoint, endpoint),
            other => panic!("expected fallback Status response, got {other:?}"),
        }

        server.await.unwrap();
    }

    #[test]
    fn cache_dir_override_moves_endpoint_and_lock_file() {
        let root = tempfile::tempdir().unwrap();
        let cache_dir = root.path().join("zc");
        let _env = EnvGuard::set_cache_dir(&cache_dir);

        let endpoint = default_endpoint();
        #[cfg(unix)]
        assert_eq!(
            endpoint,
            cache_dir.join("daemon.sock").to_string_lossy().into_owned()
        );
        #[cfg(windows)]
        {
            assert!(endpoint.starts_with(r"\\.\pipe\zccache-"));
            assert!(endpoint.ends_with(&crate::core::stable_path_id(&cache_dir)));
        }

        assert_eq!(lock_file_path(), cache_dir.join("daemon.lock"));
    }

    #[test]
    fn different_cache_roots_get_different_endpoints() {
        let a = NormalizedPath::from("/tmp/zccache-a");
        let b = NormalizedPath::from("/tmp/zccache-b");
        assert_ne!(
            endpoint_for_cache_dir(&a, None),
            endpoint_for_cache_dir(&b, None)
        );
    }

    #[test]
    fn daemon_namespace_moves_endpoint_and_lock_file() {
        let root = tempfile::tempdir().unwrap();
        let cache_dir = root.path().join("zc");
        let _env = EnvGuard::set_cache_dir_and_namespace(&cache_dir, "soldr-dev");

        let endpoint = default_endpoint();
        #[cfg(unix)]
        assert_eq!(
            endpoint,
            cache_dir
                .join("daemon-soldr-dev.sock")
                .to_string_lossy()
                .into_owned()
        );
        #[cfg(windows)]
        {
            assert!(endpoint.starts_with(r"\\.\pipe\zccache-"));
            assert!(endpoint.ends_with("-soldr-dev"));
            assert!(endpoint.contains(&crate::core::stable_path_id(&cache_dir)));
        }

        assert_eq!(lock_file_path(), cache_dir.join("daemon-soldr-dev.lock"));
    }

    #[test]
    fn same_cache_root_different_daemon_namespaces_do_not_share_identity() {
        let root = tempfile::tempdir().unwrap();
        let cache_dir = root.path().join("zc");

        let (endpoint_a, lock_a) = {
            let _env = EnvGuard::set_cache_dir_and_namespace(&cache_dir, "soldr-dev-a");
            (default_endpoint(), lock_file_path())
        };
        let (endpoint_b, lock_b) = {
            let _env = EnvGuard::set_cache_dir_and_namespace(&cache_dir, "soldr-dev-b");
            (default_endpoint(), lock_file_path())
        };

        assert_ne!(endpoint_a, endpoint_b);
        assert_ne!(lock_a, lock_b);
    }

    #[test]
    fn running_process_disable_requires_exact_one() {
        let _env = EnvGuard::isolate_running_process_disable();

        assert!(!running_process_disabled());

        std::env::set_var(RUNNING_PROCESS_DISABLE_ENV, "true");
        assert!(!running_process_disabled());

        std::env::set_var(RUNNING_PROCESS_DISABLE_ENV, "1");
        assert!(running_process_disabled());
    }

    #[test]
    fn private_daemon_name_derives_endpoint_from_cache_root() {
        let root = tempfile::tempdir().unwrap();
        let cache_dir = root.path().join("zc");
        let endpoint = endpoint_for_private_daemon_name(Some(&cache_dir), "soldr dev");

        #[cfg(unix)]
        assert_eq!(
            endpoint,
            cache_dir
                .join("daemon-soldr_dev.sock")
                .to_string_lossy()
                .into_owned()
        );
        #[cfg(windows)]
        {
            assert!(endpoint.starts_with(r"\\.\pipe\zccache-"));
            assert!(endpoint.ends_with("-soldr_dev"));
            assert!(endpoint.contains(&crate::core::stable_path_id(&cache_dir)));
        }
    }

    #[cfg(windows)]
    #[test]
    fn pipe_name_keeps_safe_username_endpoint_unchanged() {
        assert_eq!(pipe_name("zackees", None), r"\\.\pipe\zccache-zackees");
    }

    #[cfg(windows)]
    #[test]
    fn pipe_name_sanitizes_username_spaces() {
        let endpoint = pipe_name("Zach Vorhies", None);
        assert!(endpoint.starts_with(r"\\.\pipe\zccache-Zach_Vorhies-"));
        assert!(!endpoint.contains(' '));
    }

    #[cfg(unix)]
    #[test]
    fn cache_dir_endpoint_falls_back_to_short_unix_socket_path() {
        let root = tempfile::tempdir().unwrap();
        let cache_dir = root
            .path()
            .join("this")
            .join("is")
            .join("a")
            .join("deep")
            .join("private")
            .join("zccache")
            .join("cache")
            .join("directory")
            .join("that")
            .join("would")
            .join("exceed")
            .join("sockaddr_un")
            .join("path")
            .join("limits");

        let endpoint = endpoint_for_cache_dir(&cache_dir, Some("soldr-dev"));

        assert!(
            endpoint.len() <= MAX_PORTABLE_UNIX_SOCKET_PATH_BYTES,
            "endpoint too long: {endpoint}"
        );
        assert!(endpoint.starts_with("/tmp/zccache-"));
        assert!(endpoint.contains(&crate::core::stable_path_id(&cache_dir)));
        assert!(endpoint.ends_with("-daemon-soldr-dev.sock"));
    }

    /// On macOS, `daemon_exe_for_pid` must reject a PID whose
    /// executable is something other than `zccache-daemon`. Until
    /// `proc_pidpath` was wired up, this returned `None` and
    /// `verify_pid_exe_stem` fell back to alive-only — which meant a
    /// recycled PID in `daemon.lock` could keep the CLI talking to a
    /// random process on a shared CI runner. This test would have
    /// failed before that fix.
    #[cfg(target_os = "macos")]
    #[test]
    fn recycled_pid_is_rejected_on_macos() {
        use std::process::Stdio;

        // `/bin/sleep 60` — guaranteed-alive, not zccache-daemon.
        let mut sleeper = std::process::Command::new("sleep")
            .arg("60")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn /bin/sleep");
        let pid = sleeper.id();

        let exe = daemon_exe_for_pid(pid);
        let verified = verify_pid_exe_stem(pid, "zccache-daemon");

        // Clean up before assertions so a panic doesn't orphan the child.
        let _ = sleeper.kill();
        let _ = sleeper.wait();

        let exe = exe.expect("proc_pidpath must succeed for an alive child");
        let basename = exe
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_owned();
        assert_eq!(
            basename, "sleep",
            "proc_pidpath should report `sleep` as the executable"
        );
        assert!(
            !verified,
            "verify_pid_exe_stem must reject a /bin/sleep PID even though it is alive"
        );
    }

    #[test]
    fn exe_stem_matches_strips_exe_suffix_and_compares_basename() {
        use std::path::Path;
        assert!(exe_stem_matches(
            Path::new("/usr/bin/zccache-daemon"),
            "zccache-daemon"
        ));
        // A different binary at the same PID must not be accepted.
        assert!(!exe_stem_matches(
            Path::new("/usr/bin/bash"),
            "zccache-daemon"
        ));
        assert!(!exe_stem_matches(
            Path::new("/usr/bin/zccache-daemon-x"),
            "zccache-daemon"
        ));
    }

    /// Windows-only: backslash-separated paths require the OS-native
    /// `Path::file_name` semantics. On Unix `\` is a regular filename
    /// character, so the same assertion would fail there (issue #143).
    #[cfg(windows)]
    #[test]
    fn exe_stem_matches_strips_exe_suffix_on_windows() {
        use std::path::Path;
        assert!(exe_stem_matches(
            Path::new(r"C:\bin\zccache-daemon.exe"),
            "zccache-daemon"
        ));
    }

    /// Regression test for issue #132: a stale `daemon.lock` restored from a
    /// CI cache can carry a PID that's been recycled by an unrelated process
    /// on a fresh runner. `check_running_daemon` must NOT report that process
    /// as our daemon — otherwise `zccache stop` would `force_kill_process`
    /// the unrelated process.
    ///
    /// We use the test's own PID, which is guaranteed alive but is clearly
    /// not zccache-daemon, then assert the lock file is treated as stale.
    #[test]
    fn stale_lock_with_recycled_pid_is_rejected() {
        let root = tempfile::tempdir().unwrap();
        let cache_dir = root.path().join("zc");
        let _env = EnvGuard::set_cache_dir(&cache_dir);

        let lock = lock_file_path();
        write_lock_file(std::process::id()).unwrap();
        assert!(lock.exists());

        // The test process is alive but is not zccache-daemon — must be rejected.
        // (On macOS we can't read the exe path, so this test relaxes there: see
        // `daemon_exe_for_pid` for the platform fallback.)
        #[cfg(any(target_os = "linux", windows))]
        {
            assert!(check_running_daemon().is_none());
            assert!(!lock.exists(), "stale lock file should have been removed");
        }
    }

    // ─── #640 probe_existing_daemon ───────────────────────────────────────

    #[tokio::test]
    async fn probe_returns_false_when_no_lock_file() {
        let cache = tempfile::tempdir().unwrap();
        let _env = EnvGuard::set_cache_dir(cache.path());
        // No lock file written.
        assert!(!probe_existing_daemon("anything", std::time::Duration::from_millis(50)).await);
    }

    #[tokio::test]
    async fn probe_returns_false_when_lock_file_records_self_pid() {
        let cache = tempfile::tempdir().unwrap();
        let _env = EnvGuard::set_cache_dir(cache.path());
        // Self-PID early-out: we must NEVER probe our own process —
        // otherwise a sibling racing-init thread that wrote our PID
        // into the lock file would cause us to deadlock waiting for
        // ourselves to accept.
        write_lock_file(std::process::id()).unwrap();
        assert!(!probe_existing_daemon("anything", std::time::Duration::from_millis(50)).await);
    }

    #[tokio::test]
    async fn probe_returns_false_when_lock_file_pid_is_not_a_daemon() {
        let cache = tempfile::tempdir().unwrap();
        let _env = EnvGuard::set_cache_dir(cache.path());
        // PID 1 exists everywhere (init / System) but is definitely not
        // zccache-daemon, so verify_daemon_pid rejects it. The probe
        // must short-circuit at the PID-verification step BEFORE
        // attempting any IPC connect — otherwise we'd waste the
        // timeout budget on a doomed handshake against init.
        write_lock_file(1).unwrap();
        let start = std::time::Instant::now();
        let result = probe_existing_daemon(
            "garbage-endpoint-that-could-never-exist",
            std::time::Duration::from_millis(500),
        )
        .await;
        let elapsed = start.elapsed();
        assert!(!result);
        // Short-circuit means we returned faster than the connect
        // timeout — proves we never attempted the connect.
        assert!(
            elapsed < std::time::Duration::from_millis(250),
            "probe should have short-circuited via verify_daemon_pid, \
             not waited for the connect timeout — elapsed {elapsed:?}"
        );
    }
}
