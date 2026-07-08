//! Daemon lifecycle: start, stop, version probing, ensure-running, binary discovery.

use crate::core::NormalizedPath;
use std::process::ExitCode;

use super::super::status_probe_timeout;
use super::util::{connect, resolve_endpoint, run_async, LOST_CONNECTION_MSG};

const DAEMON_PROFILE_ENV: &str = "ZCCACHE_DAEMON_PROFILE";
const TOKIO_CONSOLE_PROFILE: &str = "tokio-console";
const TOKIO_CONSOLE_BIND_ENV: &str = "TOKIO_CONSOLE_BIND";
const TOKIO_CONSOLE_OPEN_ENV: &str = "ZCCACHE_TOKIO_CONSOLE_OPEN";
const TOKIO_CONSOLE_DEFAULT_BIND: &str = "127.0.0.1:6669";
const PROFILE_START_REASON: &str = "tokio-console-profile-start";

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
    /// Client-side daemon wire configuration is invalid.
    ClientConfigError(String),
}

/// Connect to the daemon and compare its version to ours.
///
/// The Status recv is bounded by [`status_probe_timeout`] so that a wedged
/// daemon (alive socket, no response) surfaces as `CommError` in seconds
/// rather than the 5-minute global default. The caller's recovery path
/// (`ensure_daemon` → `stop_stale_daemon` → `spawn_and_wait`) then runs
/// promptly. See issue #554.
pub(crate) async fn check_daemon_version(endpoint: &str) -> VersionCheck {
    match crate::ipc::daemon_control_roundtrip(
        endpoint,
        crate::ipc::DaemonControlRequest::Status,
        Some(status_probe_timeout()),
    )
    .await
    {
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
        Err(crate::ipc::IpcError::Endpoint(message))
            if message.contains(crate::protocol::wire_prost::WIRE_FORMAT_ENV) =>
        {
            VersionCheck::ClientConfigError(message)
        }
        Err(err) if crate::cli::client::is_daemon_unreachable_err(&err) => {
            VersionCheck::Unreachable
        }
        _ => VersionCheck::CommError,
    }
}

/// Spawn a new daemon and wait for it to become ready.
///
/// `outbound_pid` is `Some(pid)` when this spawn is the second half of
/// a takeover orchestrated by `stop_stale_daemon` — the helper emits
/// the linked `daemon-died{reason: takeover}` + `pipe-handover` pair
/// once the new daemon's PID has been observed. `None` for a clean
/// initial-start (no predecessor to record).  Issue #755 acceptance #2.
pub(crate) async fn spawn_and_wait(
    endpoint: &str,
    reason: &str,
    outbound_pid: Option<u32>,
) -> Result<(), String> {
    // Issue #982: embedding hosts forbid standalone daemon spawns.
    // Checked before binary resolution so the refusal message is the
    // guard's, not a misleading "cannot find zccache-daemon binary".
    if crate::core::config::daemon_spawn_disabled() {
        return Err(crate::core::config::no_spawn_error("zccache-daemon"));
    }
    let daemon_bin = find_daemon_binary().ok_or("cannot find zccache-daemon binary")?;
    tracing::debug!(?daemon_bin, %endpoint, reason, "spawning daemon");
    // Issue #952: single-flight the spawn — same arbiter as the
    // runtime.rs spawn path. Exactly one client in a cold-start herd
    // spawns; the rest park on the ready-wait.
    let spawn_slot = crate::cli::runtime::acquire_spawn_slot();
    let meta = crate::core::lifecycle::client_meta(crate::core::VERSION);
    if spawn_slot.is_some() {
        // Record *why* the CLI is about to spawn a daemon so an operator
        // can correlate each CLI decision with the resulting daemon PID
        // by parsing the single `daemon-lifecycle.log`. See zccache#323
        // for the diagnostic gap that motivated this.
        crate::core::lifecycle::write_event(
            crate::core::lifecycle::EVENT_SPAWN_ATTEMPT,
            serde_json::json!({
                "reason": reason,
                "endpoint": endpoint,
                "daemon_namespace": crate::core::config::daemon_namespace_label(),
                "client_pid": std::process::id(),
                // #755 acceptance #4: see runtime.rs for rationale.
                "client_version": meta["client_version"],
                "client_binary_path": meta["client_binary_path"],
            }),
        );
        super::super::spawn_daemon(&daemon_bin, endpoint)?;
    } else {
        crate::core::lifecycle::write_event(
            crate::core::lifecycle::EVENT_SPAWN_PARKED,
            serde_json::json!({
                "reason": reason,
                "endpoint": endpoint,
                "daemon_namespace": crate::core::config::daemon_namespace_label(),
                "client_pid": std::process::id(),
                "client_version": meta["client_version"],
            }),
        );
    }

    // Adaptive wait keyed on the daemon-lifecycle lockfile PID (issue #673):
    // the previous 100-iteration / 10 s loop expired under thundering-herd
    // builds while individual ERROR_PIPE_BUSY backoffs were still in flight.
    // The shared helper polls past 10 s as long as a daemon owns the lockfile.
    // The slot guard lives until READY so a late client can't win a
    // second slot before the daemon binds (#952).
    let wait_result = super::super::wait_for_daemon_ready(endpoint).await;
    drop(spawn_slot);
    wait_result?;

    // #755 acceptance #2: emit linked daemon-died + pipe-handover events
    // for the takeover case. Best-effort: if we can't read the new
    // daemon's PID right after `wait_for_daemon_ready` (unlikely but
    // possible under thundering-herd lockfile contention) we skip the
    // linkage; the regular `spawn` line still records the new daemon.
    if let Some(killed_pid) = outbound_pid {
        if let Some(new_pid) = crate::ipc::check_running_daemon() {
            crate::core::lifecycle::emit_takeover_lifecycle_events(
                killed_pid,
                new_pid,
                crate::core::VERSION,
                endpoint,
            );
        }
    }
    Ok(())
}

/// Stop a stale daemon that is unreachable or version-incompatible.
///
/// Attempts graceful shutdown via IPC first, then falls back to force-killing
/// the process via the lock file PID. Waits for the endpoint to be released.
///
/// Returns `Some(pid)` with the killed daemon's PID when a force-kill
/// actually fired — the caller threads this through `spawn_and_wait`
/// so the linked daemon-died + pipe-handover events get an
/// `outbound_pid`. `None` means no live daemon was found to kill
/// (graceful shutdown succeeded, or no daemon was running). #755.
pub(crate) async fn stop_stale_daemon(endpoint: &str) -> Option<u32> {
    // Try graceful shutdown via IPC.
    let _ = crate::ipc::daemon_control_roundtrip(
        endpoint,
        crate::ipc::DaemonControlRequest::Shutdown,
        Some(status_probe_timeout()),
    )
    .await;
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    // Force-kill via lock file PID if the daemon is still alive
    let killed_pid = if let Some(pid) = crate::ipc::check_running_daemon() {
        tracing::debug!(pid, "force-killing stale daemon process");
        let kill_ok = crate::ipc::force_kill_process(pid).is_ok();
        if kill_ok {
            for _ in 0..50 {
                if !crate::ipc::is_process_alive(pid) {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        }
        crate::ipc::remove_lock_file();
        kill_ok.then_some(pid)
    } else {
        None
    };

    // Wait briefly for the endpoint (named pipe / socket) to be fully released
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    killed_pid
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
    // Issue #982: under the host no-spawn guard a reachable,
    // version-compatible daemon may still be used, but every other
    // outcome — including the stale-daemon replace paths, which would
    // stop the old daemon before respawning — fails here, BEFORE
    // anything is stopped or killed.
    if crate::core::config::daemon_spawn_disabled() {
        return match check_daemon_version(endpoint).await {
            VersionCheck::Ok | VersionCheck::DaemonNewer { .. } => Ok(()),
            _ => Err(crate::core::config::no_spawn_error("zccache-daemon")),
        };
    }
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
            let killed_pid = stop_stale_daemon(endpoint).await;
            return spawn_and_wait(
                endpoint,
                crate::core::lifecycle::REASON_REPLACED_STALE_VERSION,
                killed_pid,
            )
            .await;
        }
        VersionCheck::CommError => {
            tracing::info!("cannot communicate with daemon, auto-recovering");
            let killed_pid = stop_stale_daemon(endpoint).await;
            return spawn_and_wait(
                endpoint,
                crate::core::lifecycle::REASON_REPLACED_COMM_ERROR,
                killed_pid,
            )
            .await;
        }
        VersionCheck::ClientConfigError(message) => return Err(message),
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
                    let killed_pid = stop_stale_daemon(endpoint).await;
                    return spawn_and_wait(
                        endpoint,
                        crate::core::lifecycle::REASON_REPLACED_STALE_VERSION,
                        killed_pid,
                    )
                    .await;
                }
                VersionCheck::CommError => {
                    tracing::info!(
                        "cannot communicate with daemon during startup, auto-recovering"
                    );
                    let killed_pid = stop_stale_daemon(endpoint).await;
                    return spawn_and_wait(
                        endpoint,
                        crate::core::lifecycle::REASON_REPLACED_COMM_ERROR,
                        killed_pid,
                    )
                    .await;
                }
                VersionCheck::ClientConfigError(message) => return Err(message),
                VersionCheck::Unreachable => continue,
            }
        }
        return Err(format!(
            "daemon process {pid} exists but not accepting connections"
        ));
    }

    // No daemon running — spawn one
    spawn_and_wait(endpoint, crate::core::lifecycle::REASON_INITIAL_START, None).await
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

pub(crate) async fn cmd_profile_start(endpoint: &str, bind: Option<&str>, open: bool) -> ExitCode {
    let open = open || env_truthy(TOKIO_CONSOLE_OPEN_ENV);
    let bind = tokio_console_bind(bind);
    let env = profile_env_overrides(&bind, open);
    let _guard = ScopedEnv::apply(&env);

    let killed_pid = stop_stale_daemon(endpoint).await;
    if let Err(e) = spawn_and_wait(endpoint, PROFILE_START_REASON, killed_pid).await {
        eprintln!("failed to start tokio-console daemon profile: {e}");
        return ExitCode::FAILURE;
    }
    eprintln!("daemon running with tokio-console profile at {bind}");

    if open {
        if let Err(e) = launch_tokio_console(&bind) {
            eprintln!("{e}");
            return ExitCode::FAILURE;
        }
    }

    ExitCode::SUCCESS
}

pub(crate) fn tokio_console_bind(bind: Option<&str>) -> String {
    bind.map(str::to_string)
        .or_else(|| std::env::var(TOKIO_CONSOLE_BIND_ENV).ok())
        .unwrap_or_else(|| TOKIO_CONSOLE_DEFAULT_BIND.to_string())
}

pub(crate) fn profile_env_overrides(bind: &str, open: bool) -> Vec<(String, String)> {
    let mut env = vec![
        (
            DAEMON_PROFILE_ENV.to_string(),
            TOKIO_CONSOLE_PROFILE.to_string(),
        ),
        (TOKIO_CONSOLE_BIND_ENV.to_string(), bind.to_string()),
    ];
    if open {
        env.push((TOKIO_CONSOLE_OPEN_ENV.to_string(), "1".to_string()));
    }
    env
}

fn launch_tokio_console(bind: &str) -> Result<(), String> {
    let mut cmd = std::process::Command::new("tokio-console");
    #[cfg(windows)]
    cmd.args(["--lang", "en_US.UTF-8"]);
    cmd.arg(bind);
    cmd.spawn()
        .map(|_| {
            eprintln!("launched tokio-console {bind}");
        })
        .map_err(|e| {
            format!(
                "daemon profile is running at {bind}, but failed to launch `tokio-console`: {e}. \
                 Install it with `cargo install --locked tokio-console` and run `tokio-console {bind}`."
            )
        })
}

fn env_truthy(name: &str) -> bool {
    std::env::var(name).is_ok_and(|value| {
        let value = value.trim();
        !value.is_empty()
            && !matches!(
                value.to_ascii_lowercase().as_str(),
                "0" | "false" | "no" | "off" | "n"
            )
    })
}

struct ScopedEnv {
    previous: Vec<(String, Option<String>)>,
}

impl ScopedEnv {
    fn apply(overrides: &[(String, String)]) -> Self {
        let previous = overrides
            .iter()
            .map(|(key, value)| {
                let old = std::env::var(key).ok();
                std::env::set_var(key, value);
                (key.clone(), old)
            })
            .collect();
        Self { previous }
    }
}

impl Drop for ScopedEnv {
    fn drop(&mut self) {
        for (key, value) in self.previous.iter().rev() {
            match value {
                Some(value) => std::env::set_var(key, value),
                None => std::env::remove_var(key),
            }
        }
    }
}

pub(crate) async fn cmd_stop(endpoint: &str) -> ExitCode {
    let recv_result = match crate::ipc::daemon_control_roundtrip(
        endpoint,
        crate::ipc::DaemonControlRequest::Shutdown,
        None,
    )
    .await
    {
        Ok(response) => response,
        Err(e) if crate::cli::client::is_daemon_unreachable_err(&e) => {
            let raw_lock_pid = crate::ipc::read_lock_file_pid();
            let Some(pid) = crate::ipc::check_running_daemon().or(raw_lock_pid) else {
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
            eprintln!("{LOST_CONNECTION_MSG}");
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
