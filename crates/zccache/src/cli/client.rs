//! Typed reusable daemon-client operations for in-process callers.
//!
//! The CLI command modules still own presentation and exit-code mapping. This
//! module owns request construction and response decoding for callers such as
//! the Python extension and soldr integrations that should not re-encode the
//! daemon protocol themselves.

use crate::core::NormalizedPath;
use std::path::Path;

use super::{connect_client, ensure_daemon, resolve_endpoint, run_async};

#[derive(Debug, Clone)]
pub struct SessionStartResponse {
    pub session_id: String,
    pub journal_path: Option<String>,
}

pub fn client_start(endpoint: Option<&str>) -> Result<(), String> {
    let endpoint = resolve_endpoint(endpoint);
    run_async(async move { ensure_daemon(&endpoint).await })
}

pub fn client_stop(endpoint: Option<&str>) -> Result<bool, String> {
    let endpoint = resolve_endpoint(endpoint);
    run_async(async move {
        let mut conn = match connect_client(&endpoint).await {
            Ok(c) => c,
            Err(_) => return Ok(false),
        };
        conn.send(&crate::protocol::Request::Shutdown)
            .await
            .map_err(|e| format!("failed to send to daemon: {e}"))?;
        match conn.recv::<crate::protocol::Response>().await {
            Ok(Some(crate::protocol::Response::ShuttingDown)) => Ok(true),
            Ok(Some(crate::protocol::Response::Error { message })) => Err(message),
            Ok(None) => Err("lost connection to daemon (no response received)".to_string()),
            Ok(Some(other)) => Err(format!("unexpected response from daemon: {other:?}")),
            Err(e) => Err(format!("broken connection to daemon: {e}")),
        }
    })
}

pub fn client_status(endpoint: Option<&str>) -> Result<crate::protocol::DaemonStatus, String> {
    let endpoint = resolve_endpoint(endpoint);
    run_async(async move {
        let mut conn = connect_client(&endpoint)
            .await
            .map_err(|e| format!("daemon not running at {endpoint}: {e}"))?;
        conn.send(&crate::protocol::Request::Status)
            .await
            .map_err(|e| format!("failed to send to daemon: {e}"))?;
        match conn.recv::<crate::protocol::Response>().await {
            Ok(Some(crate::protocol::Response::Status(status))) => Ok(status),
            Ok(Some(crate::protocol::Response::Error { message })) => Err(message),
            Ok(None) => Err("lost connection to daemon (no response received)".to_string()),
            Ok(Some(other)) => Err(format!("unexpected response from daemon: {other:?}")),
            Err(e) => Err(format!("broken connection to daemon: {e}")),
        }
    })
}

pub fn client_session_start(
    endpoint: Option<&str>,
    cwd: &Path,
    log_file: Option<&Path>,
    track_stats: bool,
    journal_path: Option<&Path>,
) -> Result<SessionStartResponse, String> {
    let endpoint = resolve_endpoint(endpoint);
    let cwd = cwd.to_path_buf();
    let log_file = log_file.map(NormalizedPath::from);
    let journal_path = journal_path.map(NormalizedPath::from);

    run_async(async move {
        ensure_daemon(&endpoint).await?;
        let mut conn = connect_client(&endpoint)
            .await
            .map_err(|e| format!("cannot connect to daemon at {endpoint}: {e}"))?;
        conn.send(&crate::protocol::Request::SessionStart {
            client_pid: std::process::id(),
            working_dir: cwd.into(),
            log_file,
            track_stats,
            journal_path,
            profile: false,
            private_daemon: None,
        })
        .await
        .map_err(|e| format!("failed to send to daemon: {e}"))?;

        match conn.recv::<crate::protocol::Response>().await {
            Ok(Some(crate::protocol::Response::SessionStarted {
                session_id,
                journal_path,
            })) => Ok(SessionStartResponse {
                session_id,
                journal_path: journal_path.map(|p| p.display().to_string()),
            }),
            Ok(Some(crate::protocol::Response::Error { message })) => Err(message),
            Ok(None) => Err("lost connection to daemon (no response received)".to_string()),
            Ok(Some(other)) => Err(format!("unexpected response from daemon: {other:?}")),
            Err(e) => Err(format!("broken connection to daemon: {e}")),
        }
    })
}

/// End a session; daemon-unreachable is treated as a successful no-op.
///
/// Thin `String`-error wrapper around [`session_end_idempotent`]. All in-process
/// callers (Python bindings, soldr, future tools) route through here, so the
/// idempotency contract that #151 / #159 established for the CLI subprocess
/// path applies equally to library users. Without this, soldr's at-exit
/// `zccache session-end` from `rust-plan save` fails Windows CI with
/// "cannot connect to daemon at \\.\pipe\zccache-..." when the daemon already
/// exited; every workspace test passed but teardown failed.
pub fn client_session_end(
    endpoint: Option<&str>,
    session_id: &str,
) -> Result<Option<crate::protocol::SessionStats>, String> {
    let endpoint = resolve_endpoint(endpoint);
    session_end_idempotent(&endpoint, session_id).map_err(|e| e.to_string())
}

/// Is this connect-time error a "daemon process is gone entirely" error?
///
/// The conservative set: `NotFound` (Unix socket missing, Windows pipe
/// missing), `ConnectionRefused` (Unix socket exists but no listener;
/// Windows backoff helper synthesizes this when all pipe instances are
/// permanently busy), and `BrokenPipe` (race: pipe vanished between
/// open and use). Other errors (`TimedOut`, protocol mismatches, etc.)
/// are NOT daemon-gone; they should still fail loudly.
///
/// `IpcError::Timeout` is explicitly **NOT** in the unreachable set. A
/// timed-out recv means we connected successfully but the peer did not
/// respond in the configured window; that's either a hung daemon (a
/// real fault) or a per-call budget that was too tight (caller error).
/// Either way: propagate, don't silently swallow.
///
/// Used by `session_end_idempotent` (issue #159) and the CLI's
/// `cmd_session_end` (issue #150 / #151) to map "the daemon already
/// died" connect-time failures onto a success no-op. Other request
/// types keep their existing strict error semantics.
#[must_use]
pub fn is_daemon_unreachable_err(err: &crate::ipc::IpcError) -> bool {
    use std::io::ErrorKind;
    match err {
        crate::ipc::IpcError::Io(io) => matches!(
            io.kind(),
            ErrorKind::NotFound | ErrorKind::ConnectionRefused | ErrorKind::BrokenPipe
        ),
        _ => false,
    }
}

/// End a session, treating a vanished daemon as success.
///
/// This is the shared library entry point for ending a session. It is
/// the contract used by the CLI's `zccache session-end <uuid>`
/// subcommand AND by any in-process caller (e.g. soldr's at-exit
/// `rust-plan save`); both must agree on what "the daemon already
/// died" means.
///
/// # Return shape
///
/// - `Ok(Some(stats))`: daemon was reached and returned stats for the
///   session.
/// - `Ok(None)`: daemon was reached but returned no stats (session
///   was tracked without stats), OR the daemon was unreachable at
///   connect time. Both are no-ops from the caller's perspective:
///   the session is implicitly ended when the daemon dies (see #137
///   for the daemon-side mirror), and a caller that just wants to
///   "end the session, don't care if the daemon is still alive"
///   should treat both as success.
/// - `Err(IpcError)`: anything else: timeouts, protocol mismatches,
///   send/recv mid-conversation failures, daemon error responses.
///   These are real faults and must be surfaced.
///
/// # Why a separate function
///
/// Issue #159: soldr was failing Windows CI on every main commit
/// because its in-process session-end (called from `rust-plan save`)
/// did not share code with `cmd_session_end`, so #151's
/// connect-failure idempotency only applied to the CLI subprocess
/// path. Promoting this contract to the library lets all callers,
/// current and future, share the same behavior.
pub fn session_end_idempotent(
    endpoint: &str,
    session_id: &str,
) -> Result<Option<crate::protocol::SessionStats>, crate::ipc::IpcError> {
    let endpoint = endpoint.to_string();
    let session_id = session_id.to_string();

    // Build a dedicated current-thread runtime. Can't use the existing
    // `run_async` helper because its `Output = Result<T, String>` shape
    // doesn't compose with our `Result<_, IpcError>` return type.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| {
            crate::ipc::IpcError::Endpoint(format!("failed to create tokio runtime: {e}"))
        })?;

    runtime.block_on(async move {
        let mut conn = match connect_client(&endpoint).await {
            Ok(c) => c,
            Err(e) => {
                if is_daemon_unreachable_err(&e) {
                    eprintln!(
                        "session-end: daemon unreachable at {endpoint}, treating session {session_id} as ended"
                    );
                    return Ok(None);
                }
                return Err(e);
            }
        };

        conn.send(&crate::protocol::Request::SessionEnd {
            session_id: session_id.clone(),
        })
        .await?;

        match conn.recv::<crate::protocol::Response>().await? {
            Some(crate::protocol::Response::SessionEnded { stats }) => Ok(stats),
            Some(crate::protocol::Response::Error { message }) => Err(
                crate::ipc::IpcError::Endpoint(format!("session-end failed: {message}")),
            ),
            None => Err(crate::ipc::IpcError::ConnectionClosed),
            Some(other) => Err(crate::ipc::IpcError::Endpoint(format!(
                "unexpected response from daemon: {other:?}"
            ))),
        }
    })
}

pub fn client_session_stats(
    endpoint: Option<&str>,
    session_id: &str,
) -> Result<Option<crate::protocol::SessionStats>, String> {
    let endpoint = resolve_endpoint(endpoint);
    let session_id = session_id.to_string();
    run_async(async move {
        let mut conn = connect_client(&endpoint)
            .await
            .map_err(|e| format!("cannot connect to daemon at {endpoint}: {e}"))?;
        conn.send(&crate::protocol::Request::SessionStats {
            session_id: session_id.clone(),
        })
        .await
        .map_err(|e| format!("failed to send to daemon: {e}"))?;

        match conn.recv::<crate::protocol::Response>().await {
            Ok(Some(crate::protocol::Response::SessionStatsResult { stats })) => Ok(stats),
            Ok(Some(crate::protocol::Response::Error { message })) => Err(message),
            Ok(None) => Err("lost connection to daemon (no response received)".to_string()),
            Ok(Some(other)) => Err(format!("unexpected response from daemon: {other:?}")),
            Err(e) => Err(format!("broken connection to daemon: {e}")),
        }
    })
}

#[derive(Debug, Clone)]
pub struct FingerprintCheckResponse {
    pub decision: String,
    pub reason: Option<String>,
    pub changed_files: Vec<String>,
}

pub fn fingerprint_check(
    endpoint: Option<&str>,
    cache_file: &Path,
    cache_type: &str,
    root: &Path,
    extensions: &[String],
    include_globs: &[String],
    exclude: &[String],
) -> Result<FingerprintCheckResponse, String> {
    let endpoint = resolve_endpoint(endpoint);
    let cache_file = cache_file.to_path_buf();
    let cache_type = cache_type.to_string();
    let root = root.to_path_buf();
    let extensions = extensions.to_vec();
    let include_globs = include_globs.to_vec();
    let exclude = exclude.to_vec();

    run_async(async move {
        ensure_daemon(&endpoint).await?;
        let mut conn = connect_client(&endpoint)
            .await
            .map_err(|e| format!("cannot connect to daemon at {endpoint}: {e}"))?;

        conn.send(&crate::protocol::Request::FingerprintCheck {
            cache_file: cache_file.into(),
            cache_type,
            root: root.into(),
            extensions,
            include_globs,
            exclude,
        })
        .await
        .map_err(|e| format!("failed to send to daemon: {e}"))?;

        match conn.recv::<crate::protocol::Response>().await {
            Ok(Some(crate::protocol::Response::FingerprintCheckResult {
                decision,
                reason,
                changed_files,
            })) => Ok(FingerprintCheckResponse {
                decision,
                reason,
                changed_files,
            }),
            Ok(Some(crate::protocol::Response::Error { message })) => Err(message),
            Ok(None) => Err("lost connection to daemon (no response received)".to_string()),
            Ok(Some(other)) => Err(format!("unexpected response from daemon: {other:?}")),
            Err(e) => Err(format!("broken connection to daemon: {e}")),
        }
    })
}

pub fn fingerprint_mark_success(endpoint: Option<&str>, cache_file: &Path) -> Result<(), String> {
    fingerprint_mark(endpoint, cache_file, true)
}

pub fn fingerprint_mark_failure(endpoint: Option<&str>, cache_file: &Path) -> Result<(), String> {
    fingerprint_mark(endpoint, cache_file, false)
}

fn fingerprint_mark(
    endpoint: Option<&str>,
    cache_file: &Path,
    success: bool,
) -> Result<(), String> {
    let endpoint = resolve_endpoint(endpoint);
    let cache_file = cache_file.to_path_buf();
    run_async(async move {
        ensure_daemon(&endpoint).await?;
        let mut conn = connect_client(&endpoint)
            .await
            .map_err(|e| format!("cannot connect to daemon at {endpoint}: {e}"))?;
        let request = if success {
            crate::protocol::Request::FingerprintMarkSuccess {
                cache_file: cache_file.into(),
            }
        } else {
            crate::protocol::Request::FingerprintMarkFailure {
                cache_file: cache_file.into(),
            }
        };
        conn.send(&request)
            .await
            .map_err(|e| format!("failed to send to daemon: {e}"))?;
        match conn.recv::<crate::protocol::Response>().await {
            Ok(Some(crate::protocol::Response::FingerprintAck)) => Ok(()),
            Ok(Some(crate::protocol::Response::Error { message })) => Err(message),
            Ok(None) => Err("lost connection to daemon (no response received)".to_string()),
            Ok(Some(other)) => Err(format!("unexpected response from daemon: {other:?}")),
            Err(e) => Err(format!("broken connection to daemon: {e}")),
        }
    })
}

pub fn fingerprint_invalidate(endpoint: Option<&str>, cache_file: &Path) -> Result<(), String> {
    let endpoint = resolve_endpoint(endpoint);
    let cache_file = cache_file.to_path_buf();
    run_async(async move {
        ensure_daemon(&endpoint).await?;
        let mut conn = connect_client(&endpoint)
            .await
            .map_err(|e| format!("cannot connect to daemon at {endpoint}: {e}"))?;
        conn.send(&crate::protocol::Request::FingerprintInvalidate {
            cache_file: cache_file.into(),
        })
        .await
        .map_err(|e| format!("failed to send to daemon: {e}"))?;
        match conn.recv::<crate::protocol::Response>().await {
            Ok(Some(crate::protocol::Response::FingerprintAck)) => Ok(()),
            Ok(Some(crate::protocol::Response::Error { message })) => Err(message),
            Ok(None) => Err("lost connection to daemon (no response received)".to_string()),
            Ok(Some(other)) => Err(format!("unexpected response from daemon: {other:?}")),
            Err(e) => Err(format!("broken connection to daemon: {e}")),
        }
    })
}
