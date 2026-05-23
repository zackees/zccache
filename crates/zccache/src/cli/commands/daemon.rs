//! Daemon lifecycle: start, stop, version probing, ensure-running, binary discovery.

use std::process::ExitCode;
use crate::core::NormalizedPath;

use super::util::{connect, resolve_endpoint, run_async};

pub(crate) enum VersionCheck {
    Ok,
    /// Daemon is newer than client — safe to proceed.
    DaemonNewer {
        daemon_ver: String,
    },
    /// Daemon is older than client — must restart.
    DaemonOlder {
        daemon_ver: String,
    },
    /// Could not connect to the daemon at all.
    Unreachable,
    /// Connected but could not complete the version exchange (protocol mismatch, etc.).
    CommError,
}

/// Connect to the daemon and compare its version to ours.
pub(crate) async fn check_daemon_version(endpoint: &str) -> VersionCheck {
    let mut conn = match connect(endpoint).await {
        Ok(c) => c,
        Err(_) => return VersionCheck::Unreachable,
    };
    if conn.send(&crate::protocol::Request::Status).await.is_err() {
        return VersionCheck::CommError;
    }
    match conn.recv::<crate::protocol::Response>().await {
        Ok(Some(crate::protocol::Response::Status(s))) => {
            if s.version == crate::core::VERSION {
                return VersionCheck::Ok;
            }
            let client_ver = crate::core::version::current();
            match crate::core::version::Version::parse(&s.version) {
                Some(daemon_ver) => match daemon_ver.cmp(&client_ver) {
                    std::cmp::Ordering::Equal => VersionCheck::Ok,
                    std::cmp::Ordering::Greater => VersionCheck::DaemonNewer {
                        daemon_ver: s.version,
                    },
                    std::cmp::Ordering::Less => VersionCheck::DaemonOlder {
                        daemon_ver: s.version,
                    },
                },
                // Unparseable daemon version → treat as older (safe default)
                None => VersionCheck::DaemonOlder {
                    daemon_ver: s.version,
                },
            }
        }
        _ => VersionCheck::CommError,
    }
}

/// Spawn a new daemon and wait for it to become ready.
pub(crate) async fn spawn_and_wait(endpoint: &str, reason: &str) -> Result<(), String> {
    let daemon_bin = find_daemon_binary().ok_or("cannot find zccache-daemon binary")?;
    tracing::debug!(?daemon_bin, %endpoint, reason, "spawning daemon");
    // Record *why* the CLI is about to spawn a daemon so an operator
    // can correlate each CLI decision with the resulting daemon PID
    // by parsing the single `daemon-lifecycle.log`. See zccache#323
    // for the diagnostic gap that motivated this.
    crate::core::lifecycle::write_event(
        crate::core::lifecycle::EVENT_SPAWN_ATTEMPT,
        serde_json::json!({
            "reason": reason,
            "endpoint": endpoint,
            "client_pid": std::process::id(),
        }),
    );
    super::super::spawn_daemon(&daemon_bin, endpoint)?;

    for _ in 0..100 {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        if connect(endpoint).await.is_ok() {
            return Ok(());
        }
    }
    Err("daemon started but not accepting connections after 10s".to_string())
}

/// Stop a stale daemon that is unreachable or version-incompatible.
///
/// Attempts graceful shutdown via IPC first, then falls back to force-killing
/// the process via the lock file PID. Waits for the endpoint to be released.
pub(crate) async fn stop_stale_daemon(endpoint: &str) {
    // Try graceful shutdown via IPC
    if let Ok(mut conn) = connect(endpoint).await {
        let _ = conn.send(&crate::protocol::Request::Shutdown).await;
        // Give it a moment to process the shutdown
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }

    // Force-kill via lock file PID if the daemon is still alive
    if let Some(pid) = crate::ipc::check_running_daemon() {
        tracing::debug!(pid, "force-killing stale daemon process");
        if crate::ipc::force_kill_process(pid).is_ok() {
            for _ in 0..50 {
                if !crate::ipc::is_process_alive(pid) {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        }
        crate::ipc::remove_lock_file();
    }

    // Wait briefly for the endpoint (named pipe / socket) to be fully released
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
}

/// Ensure the daemon is running **and version-compatible**.
///
/// Version checking is asymmetric: a newer daemon is accepted (it's
/// backward-compatible), but an older daemon triggers a hard error
/// telling the user to run `zccache stop` first.
///
/// Handles concurrent calls gracefully: when multiple processes race to start
/// the daemon, only one wins the bind. The losers detect this and connect to
/// the winning daemon instead of failing.
pub(crate) async fn ensure_daemon(endpoint: &str) -> Result<(), String> {
    // Fast path: connect + version check
    match check_daemon_version(endpoint).await {
        VersionCheck::Ok => return Ok(()),
        VersionCheck::DaemonNewer { daemon_ver } => {
            tracing::debug!(
                daemon_ver,
                client_ver = crate::core::VERSION,
                "daemon is newer than client, proceeding"
            );
            return Ok(());
        }
        VersionCheck::DaemonOlder { daemon_ver } => {
            tracing::info!(
                daemon_ver,
                client_ver = crate::core::VERSION,
                "daemon is older than client, auto-recovering"
            );
            stop_stale_daemon(endpoint).await;
            return spawn_and_wait(
                endpoint,
                crate::core::lifecycle::REASON_REPLACED_STALE_VERSION,
            )
            .await;
        }
        VersionCheck::CommError => {
            tracing::info!("cannot communicate with daemon, auto-recovering");
            stop_stale_daemon(endpoint).await;
            return spawn_and_wait(
                endpoint,
                crate::core::lifecycle::REASON_REPLACED_COMM_ERROR,
            )
            .await;
        }
        VersionCheck::Unreachable => {
            // Fall through to lock-file check / spawn
        }
    }

    // Check lock file for a running daemon we just can't reach yet
    if let Some(pid) = crate::ipc::check_running_daemon() {
        for _ in 0..20 {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            match check_daemon_version(endpoint).await {
                VersionCheck::Ok => return Ok(()),
                VersionCheck::DaemonNewer { daemon_ver } => {
                    tracing::debug!(
                        daemon_ver,
                        client_ver = crate::core::VERSION,
                        "daemon is newer than client, proceeding"
                    );
                    return Ok(());
                }
                VersionCheck::DaemonOlder { daemon_ver } => {
                    tracing::info!(
                        daemon_ver,
                        client_ver = crate::core::VERSION,
                        "daemon is older than client during startup, auto-recovering"
                    );
                    stop_stale_daemon(endpoint).await;
                    return spawn_and_wait(
                        endpoint,
                        crate::core::lifecycle::REASON_REPLACED_STALE_VERSION,
                    )
                    .await;
                }
                VersionCheck::CommError => {
                    tracing::info!(
                        "cannot communicate with daemon during startup, auto-recovering"
                    );
                    stop_stale_daemon(endpoint).await;
                    return spawn_and_wait(
                        endpoint,
                        crate::core::lifecycle::REASON_REPLACED_COMM_ERROR,
                    )
                    .await;
                }
                VersionCheck::Unreachable => continue,
            }
        }
        return Err(format!(
            "daemon process {pid} exists but not accepting connections"
        ));
    }

    // No daemon running — spawn one
    spawn_and_wait(endpoint, crate::core::lifecycle::REASON_INITIAL_START).await
}

/// Find the daemon binary. Looks next to the CLI binary first, then on PATH.
pub(crate) fn find_daemon_binary() -> Option<NormalizedPath> {
    let name = if cfg!(windows) {
        "zccache-daemon.exe"
    } else {
        "zccache-daemon"
    };

    // Look next to the CLI binary
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let candidate = dir.join(name);
            if candidate.exists() {
                return Some(candidate.into());
            }
        }
    }

    // Fall back to PATH
    which_on_path(name)
}

/// Simple PATH lookup (no external crate needed).
/// On Windows, also tries appending `.exe` if the name has no extension.
pub(crate) fn which_on_path(name: &str) -> Option<NormalizedPath> {
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate.into());
        }
        // On Windows, try with .exe suffix
        #[cfg(windows)]
        if std::path::Path::new(name).extension().is_none() {
            let with_exe = dir.join(format!("{name}.exe"));
            if with_exe.is_file() {
                return Some(with_exe.into());
            }
        }
    }
    None
}

pub(crate) async fn cmd_start(endpoint: &str) -> ExitCode {
    match ensure_daemon(endpoint).await {
        Ok(()) => {
            eprintln!("daemon running at {endpoint}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("failed to start daemon: {e}");
            ExitCode::FAILURE
        }
    }
}

pub(crate) async fn cmd_stop(endpoint: &str) -> ExitCode {
    let mut conn = match connect(endpoint).await {
        Ok(c) => c,
        Err(_) => {
            let Some(pid) = crate::ipc::check_running_daemon() else {
                eprintln!("daemon not running at {endpoint}");
                // No daemon — but the index file might still be there from a
                // crashed prior run. Probe once so callers (CI tar) can rely
                // on the lock being gone after `zccache stop` returns.
                wait_for_daemon_teardown(endpoint).await;
                return ExitCode::SUCCESS;
            };

            match crate::ipc::force_kill_process(pid) {
                Ok(()) => {
                    for _ in 0..50 {
                        if !crate::ipc::is_process_alive(pid) {
                            crate::ipc::remove_lock_file();
                            eprintln!(
                                "daemon process {pid} terminated after IPC connection failed"
                            );
                            wait_for_daemon_teardown(endpoint).await;
                            return ExitCode::SUCCESS;
                        }
                        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    }
                    eprintln!(
                        "zccache: sent termination to daemon process {pid}, but it did not exit"
                    );
                    return ExitCode::FAILURE;
                }
                Err(e) => {
                    eprintln!(
                        "zccache: cannot connect to daemon at {endpoint}, and failed to kill \
                         locked process {pid}: {e}"
                    );
                    return ExitCode::FAILURE;
                }
            }
        }
    };

    if let Err(e) = conn.send(&crate::protocol::Request::Shutdown).await {
        eprintln!("zccache[err][S]: failed to send to daemon: {e}");
        return ExitCode::FAILURE;
    }
    let recv_result = match conn.recv().await {
        Ok(r) => r,
        Err(e) => {
            eprintln!("zccache[err][R]: broken connection to daemon: {e}");
            return ExitCode::FAILURE;
        }
    };
    match recv_result {
        Some(crate::protocol::Response::ShuttingDown) => {
            // The daemon acknowledges `Shutdown` immediately and continues
            // teardown asynchronously. On Windows the redb index lock is held
            // until the daemon process actually exits and `Drop` fires. Wait
            // for the IPC endpoint to drop and for `index.redb` to be
            // openable (i.e. no exclusive share lock) so callers like the CI
            // post-step tar do not race the daemon. See issue #182.
            wait_for_daemon_teardown(endpoint).await;
            eprintln!("daemon stopped");
            ExitCode::SUCCESS
        }
        None => {
            eprintln!("zccache[err][R]: lost connection to daemon (no response). Often a daemon-CLI protocol version mismatch — try `zccache stop`");
            ExitCode::FAILURE
        }
        Some(other) => {
            eprintln!("zccache[err][U]: unexpected response from daemon: {other:?}");
            ExitCode::FAILURE
        }
    }
}

/// Default cap on how long `zccache stop` will wait after the daemon ACKs
/// `Shutdown` for the IPC endpoint to disappear and `index.redb` to become
/// openable. Overridable with `ZCCACHE_STOP_TIMEOUT_SECS`.
const STOP_WAIT_DEFAULT_SECS: u64 = 10;
/// Poll cadence inside the bounded wait loop.
const STOP_WAIT_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(100);

/// Returns the bounded total wait duration for `zccache stop`, honoring
/// `ZCCACHE_STOP_TIMEOUT_SECS` if it parses as a non-negative `u64`.
fn stop_wait_timeout() -> std::time::Duration {
    let secs = std::env::var("ZCCACHE_STOP_TIMEOUT_SECS")
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(STOP_WAIT_DEFAULT_SECS);
    std::time::Duration::from_secs(secs)
}

/// Poll until the IPC endpoint is unreachable. Emits a warning on timeout
/// but never fails the caller — the worst case is that the caller (e.g. CI
/// cache tar) sees the same error it would have seen without this wait.
///
/// The legacy redb-era version of this routine also waited for the index
/// file's exclusive share lock to drop on Windows. With the bincode blob
/// there is no file lock — `flush()` writes via temp+rename, holding the
/// file handle only briefly during the rename — so endpoint reachability
/// is the only signal we need.
pub(crate) async fn wait_for_daemon_teardown(endpoint: &str) {
    let deadline = std::time::Instant::now() + stop_wait_timeout();
    loop {
        if !is_ipc_endpoint_reachable(endpoint).await {
            return;
        }
        if std::time::Instant::now() >= deadline {
            eprintln!(
                "zccache: timed out waiting for daemon endpoint to disappear after stop; \
                 continuing anyway. set ZCCACHE_STOP_TIMEOUT_SECS to override."
            );
            return;
        }
        tokio::time::sleep(STOP_WAIT_POLL_INTERVAL).await;
    }
}

/// True if a fresh `connect()` to the daemon IPC endpoint succeeds.
async fn is_ipc_endpoint_reachable(endpoint: &str) -> bool {
    connect(endpoint).await.is_ok()
}

// Trampolines for top-level flags / `start`/`stop` so the dispatch
// match in `cli::mod` doesn't need its own runtime plumbing.
pub(crate) fn run_start() -> ExitCode {
    let endpoint = resolve_endpoint(None);
    run_async(cmd_start(&endpoint))
}

pub(crate) fn run_stop() -> ExitCode {
    let endpoint = resolve_endpoint(None);
    run_async(cmd_stop(&endpoint))
}
