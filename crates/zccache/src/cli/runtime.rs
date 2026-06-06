//! Daemon lifecycle helpers used by the CLI library: connect to a running
//! daemon, version-check, spawn a fresh one, sanitize the per-launch binary
//! copy, garbage-collect stale runtime/log files.
//!
//! Extracted from `cli/mod.rs` in wave 6 of the zccache crate consolidation
//! (issue #365) to keep that file under the 1.5K-LOC `loc_guard` block
//! threshold. Re-exported from `cli/mod.rs` so the public path is unchanged.

use crate::core::NormalizedPath;
use std::path::Path;

pub fn run_async<T>(
    future: impl std::future::Future<Output = Result<T, String>>,
) -> Result<T, String> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("failed to create tokio runtime: {e}"))?
        .block_on(future)
}

#[derive(Debug)]
enum VersionCheck {
    Ok,
    Unreachable,
    DaemonOlder { daemon_ver: String },
    DaemonNewer,
    CommError,
}

#[cfg(unix)]
pub async fn connect_client(
    endpoint: &str,
) -> Result<crate::ipc::IpcConnection, crate::ipc::IpcError> {
    let mut conn = crate::ipc::connect(endpoint).await?;
    conn.set_recv_timeout(crate::ipc::DEFAULT_CLIENT_RECV_TIMEOUT);
    Ok(conn)
}

#[cfg(windows)]
pub async fn connect_client(
    endpoint: &str,
) -> Result<crate::ipc::IpcClientConnection, crate::ipc::IpcError> {
    let mut conn = crate::ipc::connect(endpoint).await?;
    conn.set_recv_timeout(crate::ipc::DEFAULT_CLIENT_RECV_TIMEOUT);
    Ok(conn)
}

async fn check_daemon_version(endpoint: &str) -> VersionCheck {
    let mut conn = match connect_client(endpoint).await {
        Ok(c) => c,
        Err(_) => return VersionCheck::Unreachable,
    };
    if conn.send(&crate::protocol::Request::Status).await.is_err() {
        return VersionCheck::CommError;
    }
    match conn
        .recv_with_timeout::<crate::protocol::Response>(super::status_probe_timeout())
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
                    std::cmp::Ordering::Greater => VersionCheck::DaemonNewer,
                    std::cmp::Ordering::Less => VersionCheck::DaemonOlder {
                        daemon_ver: s.version,
                    },
                },
                None => VersionCheck::DaemonOlder {
                    daemon_ver: s.version,
                },
            }
        }
        _ => VersionCheck::CommError,
    }
}

async fn spawn_and_wait(endpoint: &str, reason: &str) -> Result<(), String> {
    let daemon_bin = find_daemon_binary().ok_or("cannot find zccache-daemon binary")?;
    // Record *why* the CLI is about to spawn a daemon. Pairs with the
    // daemon-side "spawn" event so an operator can correlate each CLI
    // decision with the resulting daemon PID by parsing the single
    // `daemon-lifecycle.log`. Reasons: initial-start vs. one of the
    // replaced-* variants. This is the diagnostic gap zccache#323
    // identified — knowing 5 daemons spawned without knowing why
    // makes the root cause undebuggable.
    crate::core::lifecycle::write_event(
        crate::core::lifecycle::EVENT_SPAWN_ATTEMPT,
        serde_json::json!({
            "reason": reason,
            "endpoint": endpoint,
            "daemon_namespace": crate::core::config::daemon_namespace_label(),
            "client_pid": std::process::id(),
        }),
    );
    spawn_daemon(&daemon_bin, endpoint)?;

    wait_for_daemon_ready(endpoint).await
}

/// Tunables for [`wait_for_daemon_ready_with`]. Defaults match the contract
/// described in issue #673: keep waiting as long as a daemon process owns
/// the lockfile, treat absence-of-lockfile as a spawn failure after a short
/// grace period, and refuse to wait beyond a hard ceiling even with a live
/// daemon (the daemon may be wedged).
#[derive(Debug, Clone, Copy)]
pub(crate) struct AdaptiveWaitConfig {
    pub poll_interval: std::time::Duration,
    pub no_daemon_grace: std::time::Duration,
    pub hard_ceiling: std::time::Duration,
}

impl Default for AdaptiveWaitConfig {
    fn default() -> Self {
        Self {
            poll_interval: std::time::Duration::from_millis(100),
            // Matches the pre-#673 10s budget for the cold-start case where
            // the spawn itself fails before the daemon ever binds.
            no_daemon_grace: std::time::Duration::from_secs(10),
            // Safety net once a daemon has been observed alive. Issue #673
            // reports individual ERROR_PIPE_BUSY backoffs taking 5+ seconds
            // on Windows under a 32-deep thundering herd; 60 s gives the
            // accept queue room to drain before declaring the daemon wedged.
            hard_ceiling: std::time::Duration::from_secs(60),
        }
    }
}

/// Outcome of one poll of the adaptive ready-wait loop. Factored out so the
/// timing decisions can be unit-tested without touching the real clock,
/// filesystem lockfile, or IPC stack.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum WaitTick {
    /// Daemon is still coming up; sleep another `poll_interval` and try again.
    Pending,
    /// A daemon was alive but a hard wall-clock ceiling was hit — declare
    /// the daemon wedged so the caller can recover.
    HardCeilingHit { observed_pid: Option<u32> },
    /// Grace period elapsed without ever observing a daemon lockfile — the
    /// `spawn_daemon` call most likely failed silently.
    NoDaemonGracePassed,
    /// A daemon previously owned the lockfile but it has since vanished —
    /// the daemon crashed before draining its accept queue.
    DaemonExited { pid: u32 },
}

/// Pure decision function: given the wall-clock state and the current /
/// last-observed daemon lockfile PID, return what the wait loop should do
/// next. Unit-tested in `mod tests` below; production callers go through
/// [`wait_for_daemon_ready_with`].
pub(crate) fn classify_wait_tick(
    elapsed: std::time::Duration,
    daemon_pid: Option<u32>,
    last_observed_pid: Option<u32>,
    cfg: &AdaptiveWaitConfig,
) -> WaitTick {
    if let Some(pid) = daemon_pid {
        if elapsed >= cfg.hard_ceiling {
            return WaitTick::HardCeilingHit {
                observed_pid: Some(pid),
            };
        }
        return WaitTick::Pending;
    }
    if let Some(pid) = last_observed_pid {
        return WaitTick::DaemonExited { pid };
    }
    if elapsed >= cfg.no_daemon_grace {
        return WaitTick::NoDaemonGracePassed;
    }
    WaitTick::Pending
}

/// Poll the daemon endpoint until either the connect succeeds or one of the
/// adaptive failure modes (no-lockfile grace expired, observed daemon
/// exited, or hard wall-clock ceiling reached) fires. Used by both
/// `spawn_and_wait` call sites so they share a single timing contract.
///
/// Issue #673: replaces a flat 10 s, 100-iteration loop that expired under
/// thundering-herd builds even when the daemon was alive and just slow to
/// drain its Windows named-pipe accept queue.
pub async fn wait_for_daemon_ready(endpoint: &str) -> Result<(), String> {
    wait_for_daemon_ready_with(
        endpoint,
        crate::ipc::check_running_daemon,
        AdaptiveWaitConfig::default(),
    )
    .await
}

/// Test seam for [`wait_for_daemon_ready`]: caller injects the lockfile
/// check and timing config so unit tests can drive the loop without
/// touching the real daemon-lock file or sleeping for real seconds.
pub(crate) async fn wait_for_daemon_ready_with(
    endpoint: &str,
    daemon_alive_check: impl Fn() -> Option<u32>,
    cfg: AdaptiveWaitConfig,
) -> Result<(), String> {
    let start = std::time::Instant::now();
    let mut last_observed_pid: Option<u32> = None;
    loop {
        tokio::time::sleep(cfg.poll_interval).await;
        if connect_client(endpoint).await.is_ok() {
            return Ok(());
        }
        let elapsed = start.elapsed();
        let daemon_pid = daemon_alive_check();
        if daemon_pid.is_some() {
            last_observed_pid = daemon_pid;
        }
        match classify_wait_tick(elapsed, daemon_pid, last_observed_pid, &cfg) {
            WaitTick::Pending => continue,
            WaitTick::HardCeilingHit { observed_pid } => {
                let pid_str = observed_pid
                    .map(|p| p.to_string())
                    .unwrap_or_else(|| "<unknown>".to_string());
                return Err(format!(
                    "daemon process {pid_str} still not accepting connections after {}s (hard cap)",
                    cfg.hard_ceiling.as_secs()
                ));
            }
            WaitTick::NoDaemonGracePassed => {
                return Err(format!(
                    "no daemon lockfile observed within {}s of spawn (spawn likely failed)",
                    cfg.no_daemon_grace.as_secs()
                ));
            }
            WaitTick::DaemonExited { pid } => {
                return Err(format!(
                    "daemon process {pid} exited before accepting connections"
                ));
            }
        }
    }
}

/// Stop a stale daemon that is unreachable or version-incompatible.
async fn stop_stale_daemon(endpoint: &str) {
    if let Ok(mut conn) = connect_client(endpoint).await {
        let _ = conn.send(&crate::protocol::Request::Shutdown).await;
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }

    if let Some(pid) = crate::ipc::check_running_daemon() {
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

    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
}

pub async fn ensure_daemon(endpoint: &str) -> Result<(), String> {
    match check_daemon_version(endpoint).await {
        VersionCheck::Ok | VersionCheck::DaemonNewer => return Ok(()),
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
            return spawn_and_wait(endpoint, crate::core::lifecycle::REASON_REPLACED_COMM_ERROR)
                .await;
        }
        VersionCheck::Unreachable => {}
    }

    if let Some(pid) = crate::ipc::check_running_daemon() {
        let mut backoff = std::time::Duration::from_millis(100);
        for _ in 0..20 {
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(std::time::Duration::from_millis(500));
            match check_daemon_version(endpoint).await {
                VersionCheck::Ok | VersionCheck::DaemonNewer => return Ok(()),
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
            "daemon process {pid} exists but not accepting connections after retrying"
        ));
    }

    spawn_and_wait(endpoint, crate::core::lifecycle::REASON_INITIAL_START).await
}

fn find_daemon_binary() -> Option<NormalizedPath> {
    let name = if cfg!(windows) {
        "zccache-daemon.exe"
    } else {
        "zccache-daemon"
    };

    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let candidate = dir.join(name);
            if candidate.exists() {
                return Some(candidate.into());
            }
        }
    }

    which_on_path(name)
}

fn which_on_path(name: &str) -> Option<NormalizedPath> {
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate.into());
        }
        #[cfg(windows)]
        if Path::new(name).extension().is_none() {
            let with_exe = dir.join(format!("{name}.exe"));
            if with_exe.is_file() {
                return Some(with_exe.into());
            }
        }
    }
    None
}

/// Initialize spawn-lineage env vars on a command the CLI is about to spawn.
///
/// Mirrors the daemon-side propagation in `zccache_daemon::lineage` so that
/// any process attribution (orphan tracking, running-process scanners) sees
/// a consistent chain across CLI -> daemon -> compiler hops. The chain is
/// initialized with the CLI's PID, and the originator marker (used by
/// running-process for crash-resilient orphan discovery) is set to
/// `zccache-cli:<pid>` unless an outer tool has already claimed it.
#[cfg(not(windows))]
fn apply_cli_spawn_lineage(cmd: &mut std::process::Command) {
    for (k, v) in cli_spawn_lineage_env() {
        cmd.env(k, v);
    }
}

/// Compute the lineage env-var pairs the CLI sets on the daemon it
/// spawns. Returns the same overrides `apply_cli_spawn_lineage` writes
/// onto a `Command`, in a form usable by the Windows raw-spawn path
/// (which needs to build its own merged environment block).
fn cli_spawn_lineage_env() -> Vec<(String, String)> {
    const ENV_ORIGINATOR: &str = "RUNNING_PROCESS_ORIGINATOR";
    const ENV_LINEAGE: &str = "ZCCACHE_LINEAGE";
    const ENV_PARENT_PID: &str = "ZCCACHE_PARENT_PID";
    const ENV_CLIENT_PID: &str = "ZCCACHE_CLIENT_PID";

    let cli_pid = std::process::id();
    let mut out: Vec<(String, String)> = Vec::with_capacity(4);

    // Preserve any outer originator (e.g. the build tool was already wrapped
    // by running-process). Otherwise, claim the originator slot ourselves.
    if std::env::var(ENV_ORIGINATOR).is_err() {
        out.push((ENV_ORIGINATOR.to_string(), format!("zccache-cli:{cli_pid}")));
    }

    // Extend or initialize the chain with our PID.
    let chain = match std::env::var(ENV_LINEAGE) {
        Ok(existing)
            if existing
                .rsplit_once('>')
                .map_or(existing.as_str(), |(_, last)| last)
                != cli_pid.to_string() =>
        {
            format!("{existing}>{cli_pid}")
        }
        Ok(existing) => existing,
        Err(_) => cli_pid.to_string(),
    };
    out.push((ENV_LINEAGE.to_string(), chain));
    out.push((ENV_PARENT_PID.to_string(), cli_pid.to_string()));
    out.push((ENV_CLIENT_PID.to_string(), cli_pid.to_string()));
    out
}

/// Subdir of the zccache global cache directory where the CLI stores
/// per-launch copies of the daemon binary. The daemon runs from one of
/// these copies, never from the install path (e.g. `Scripts/zccache-daemon.exe`),
/// so `pip install --upgrade zccache` can always overwrite the install
/// path regardless of whether a daemon is alive. See issue #134.
const RUNTIME_BINARIES_SUBDIR: &str = "runtime-binaries";

/// Returns `<global_cache_dir>/runtime-binaries`.
#[must_use]
pub fn runtime_binaries_dir() -> NormalizedPath {
    crate::core::config::default_cache_dir().join(RUNTIME_BINARIES_SUBDIR)
}

/// Copy `canonical` (the daemon binary at its install location) to a unique
/// path inside [`runtime_binaries_dir`] and return the new path. The caller
/// then spawns from the returned path so the install location is never
/// file-locked by a running daemon.
///
/// On copy failure the caller should fall back to spawning `canonical`
/// directly; the in-place `unlock_exe()` in the daemon then handles the
/// lock removal as a fallback.
pub fn prepare_daemon_exe(canonical: &Path) -> Result<std::path::PathBuf, std::io::Error> {
    prepare_daemon_exe_in(canonical, runtime_binaries_dir().as_path())
}

/// Test seam for [`prepare_daemon_exe`]: copies `canonical` into `dir`
/// (which is created if missing) and returns the destination path.
pub fn prepare_daemon_exe_in(
    canonical: &Path,
    dir: &Path,
) -> Result<std::path::PathBuf, std::io::Error> {
    std::fs::create_dir_all(dir)?;

    // Per-launch unique name. PID alone is reused across reboots; xor with
    // the current nanos timestamp to keep collisions rare even when several
    // CLI processes spawn back-to-back.
    let rand_id: u32 = std::process::id()
        ^ std::time::UNIX_EPOCH
            .elapsed()
            .unwrap_or_default()
            .subsec_nanos();
    let extension = canonical.extension().and_then(|s| s.to_str()).unwrap_or("");
    let file_name = if extension.is_empty() {
        format!("zccache-daemon.{rand_id}")
    } else {
        format!("zccache-daemon.{rand_id}.{extension}")
    };
    let dest = dir.join(&file_name);
    std::fs::copy(canonical, &dest)?;
    Ok(dest)
}

/// Best-effort delete every entry in [`runtime_binaries_dir`]. On Windows
/// the kernel refuses to delete a file with an open handle, so files
/// belonging to a *currently running* daemon are silently skipped — no PID
/// tracking, no sidecar files. Cheap enough to call before every spawn.
pub fn gc_runtime_binaries() {
    gc_runtime_binaries_in(runtime_binaries_dir().as_path());
}

/// Test seam for [`gc_runtime_binaries`].
pub fn gc_runtime_binaries_in(dir: &Path) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let _ = std::fs::remove_file(entry.path());
    }
}

/// Subdir of the global cache directory where the daemon writes its own
/// stdout + stderr on every spawn. Each spawn gets a fresh file named
/// `daemon-spawn-{pid}-{nanos}.log` so concurrent CLI invocations don't
/// stomp each other. Errors that hit the daemon before its panic hook or
/// lifecycle log are alive land here — previously they went to `/dev/null`
/// on Unix and caused silent failures (notably the macOS regression that
/// motivated this change).
const DAEMON_SPAWN_LOGS_SUBDIR: &str = "logs";

/// Allocate a unique per-spawn log path under `{cache_dir}/logs/`.
/// The directory is created lazily; if creation fails we still hand back a
/// path — the daemon's own opener will see the error and fall back to
/// `Stdio::null` after warning.
fn allocate_daemon_spawn_log_path() -> std::path::PathBuf {
    let dir = crate::core::config::default_cache_dir().join(DAEMON_SPAWN_LOGS_SUBDIR);
    let _ = std::fs::create_dir_all(dir.as_path());
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let pid = std::process::id();
    let file_name = match crate::core::config::daemon_namespace() {
        Some(namespace) => format!("daemon-spawn-{namespace}-{pid}-{nanos}.log"),
        None => format!("daemon-spawn-{pid}-{nanos}.log"),
    };
    dir.as_path().join(file_name)
}

/// Default age cutoff for entries swept by [`gc_log_directory`]. Files
/// older than this are removed. Subdirectories are skipped (the daemon
/// doesn't create any under `logs/` today).
const LOG_GC_CUTOFF: std::time::Duration = std::time::Duration::from_secs(60 * 60 * 24);

/// Best-effort sweep of stale files in `{cache_dir}/logs/`.
///
/// Catches every log type that lands in this directory — not just
/// `daemon-spawn-*.log`. As of the issue-#323 fix this includes:
///   * `daemon-spawn-{pid}-{nanos}.log` (per-spawn daemon stdio
///     capture; CLI-owned)
///   * `daemon-lifecycle.log.1` (rotated lifecycle archive; the daemon
///     handles its own 1 MiB soft-cap but never garbage-collects the
///     archive, so it can sit on disk forever after the daemon exits)
///   * `daemon.log.*` (rotated event-log archives; the EventLogger
///     keeps N by count, this adds a time-based safety net for archives
///     left behind by daemons that exited before the next rotation)
///   * `compile_journal.jsonl.*` (rotated compile-journal archives;
///     same rationale)
///   * Anything else that may have accumulated here from past versions
///     or external tooling
///
/// The active `daemon-lifecycle.log` is intentionally *preserved* — a
/// long-idle daemon may go 24h between writes (spawn → next event),
/// and deleting it mid-life would erase the very history that #323
/// needed to diagnose the multi-spawn bug.
pub fn gc_log_directory() {
    let dir = crate::core::config::default_cache_dir().join(DAEMON_SPAWN_LOGS_SUBDIR);
    gc_log_directory_in(dir.as_path(), LOG_GC_CUTOFF);
}

/// Test seam for [`gc_log_directory`]. Sweeps stale files in `dir`
/// older than `cutoff`, preserving the active
/// `daemon-lifecycle.log` regardless of age.
pub fn gc_log_directory_in(dir: &Path, cutoff: std::time::Duration) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    let now = std::time::SystemTime::now();
    for entry in entries.flatten() {
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        // Skip the live lifecycle log: it's the one file that may sit
        // untouched between a daemon's `spawn` and `died-*` events.
        // Every other file in `logs/` either rotates often or is a
        // historical artifact safe to discard once old.
        if crate::core::lifecycle::is_live_lifecycle_log_name(&name) {
            continue;
        }
        let file_type = entry.file_type();
        if file_type.map(|t| !t.is_file()).unwrap_or(true) {
            continue;
        }
        let modified = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| now.duration_since(t).ok());
        if let Some(age) = modified {
            if age > cutoff {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
}

/// Back-compat alias for the broadened sweep. Earlier callers used
/// the spawn-log-only name; new code should use [`gc_log_directory`].
#[deprecated(note = "use gc_log_directory instead — sweeps the full logs/ directory")]
pub fn gc_daemon_spawn_logs() {
    gc_log_directory();
}

pub fn spawn_daemon(bin: &Path, endpoint: &str) -> Result<(), String> {
    // GC before the new spawn so neither dir grows unbounded across
    // crash-loop scenarios. Live daemons keep their open log file FDs;
    // GC only touches files older than the 24h cutoff and preserves
    // the active `daemon-lifecycle.log` regardless of age.
    gc_runtime_binaries();
    gc_log_directory();

    // Prefer to spawn from a relocated copy in the zccache global dir.
    // Fall back to the canonical install path if the copy fails — the
    // daemon's own `unlock_exe()` then handles the in-place rename.
    let bin_owned: std::path::PathBuf;
    let spawn_bin: &Path = match prepare_daemon_exe(bin) {
        Ok(p) => {
            bin_owned = p;
            &bin_owned
        }
        Err(_) => bin,
    };

    // Allocate a per-spawn log file path. Passed to the daemon via
    // `--log-file`; the daemon reopens its own stdout + stderr onto that
    // path early in startup. This replaces the previous Unix
    // `Stdio::null()` daemon spawn which made macOS dyld/gatekeeper
    // failures invisible (see PR #312 for full diagnosis).
    let log_path = allocate_daemon_spawn_log_path();
    let log_arg = log_path.to_string_lossy().into_owned();

    // Delegate the actual spawn to `running_process::spawn_daemon`
    // (renamed from `sanitized::spawn` in the 3.2 → 3.3 reshape — same
    // semantics, lives in the `spawn` module now and is re-exported at
    // the crate root). That helper handles both platform-specific quirks
    // the daemon hits:
    //  • Windows: STARTUPINFOEX + PROC_THREAD_ATTRIBUTE_HANDLE_LIST so
    //    grandparent pipe handles (e.g. Python's
    //    `subprocess.Popen(stdout=PIPE)` further up the chain) don't
    //    leak into the daemon and prevent EOF on the parent's read.
    //  • Unix: `setsid()` to detach from the controlling tty + close every
    //    fd > 2 between fork and exec so the same orphan-handle issue
    //    doesn't bite on macOS in particular.
    //
    // `DaemonChild` always opens NUL for its stdio at the spawn site;
    // the daemon then redirects its own stdout + stderr to `--log-file`
    // once it's running.
    let mut cmd = std::process::Command::new(spawn_bin);
    cmd.args([
        "--foreground",
        "--endpoint",
        endpoint,
        "--log-file",
        &log_arg,
    ]);
    #[cfg(not(windows))]
    apply_cli_spawn_lineage(&mut cmd);
    #[cfg(windows)]
    {
        // On Windows the sanitized spawn rebuilds the environment block
        // itself; pass our lineage overrides via `cmd.env(...)` so they
        // land in the merged block.
        for (k, v) in cli_spawn_lineage_env() {
            cmd.env(k, v);
        }
    }
    running_process::spawn_daemon(&mut cmd)
        .map(|_child| ())
        .map_err(|e| format!("failed to spawn daemon (sanitized): {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    fn cfg(grace_ms: u64, ceiling_ms: u64, poll_ms: u64) -> AdaptiveWaitConfig {
        AdaptiveWaitConfig {
            poll_interval: Duration::from_millis(poll_ms),
            no_daemon_grace: Duration::from_millis(grace_ms),
            hard_ceiling: Duration::from_millis(ceiling_ms),
        }
    }

    // Bogus endpoint that connect_client cannot bind to on either platform.
    // Unix: a nonexistent socket path. Windows: a nonexistent named pipe.
    fn dead_endpoint() -> &'static str {
        if cfg!(windows) {
            r"\\.\pipe\zccache-test-issue-673-dead"
        } else {
            "/tmp/zccache-test-issue-673-dead.sock"
        }
    }

    // -- classify_wait_tick (pure decision function) -----------------------

    #[test]
    fn pending_when_daemon_visible_and_below_hard_ceiling() {
        let c = cfg(1_000, 5_000, 100);
        let tick = classify_wait_tick(Duration::from_millis(500), Some(42), Some(42), &c);
        assert_eq!(tick, WaitTick::Pending);
    }

    #[test]
    fn hard_ceiling_hit_only_when_daemon_visible() {
        let c = cfg(1_000, 5_000, 100);
        let tick = classify_wait_tick(Duration::from_millis(5_000), Some(42), Some(42), &c);
        assert_eq!(
            tick,
            WaitTick::HardCeilingHit {
                observed_pid: Some(42)
            }
        );
    }

    #[test]
    fn daemon_exited_when_previously_observed_then_gone() {
        let c = cfg(1_000, 5_000, 100);
        let tick = classify_wait_tick(Duration::from_millis(200), None, Some(42), &c);
        assert_eq!(tick, WaitTick::DaemonExited { pid: 42 });
    }

    #[test]
    fn no_daemon_grace_passed_when_never_observed_and_grace_elapsed() {
        let c = cfg(1_000, 5_000, 100);
        let tick = classify_wait_tick(Duration::from_millis(1_000), None, None, &c);
        assert_eq!(tick, WaitTick::NoDaemonGracePassed);
    }

    #[test]
    fn pending_when_never_observed_but_grace_still_running() {
        let c = cfg(1_000, 5_000, 100);
        let tick = classify_wait_tick(Duration::from_millis(500), None, None, &c);
        assert_eq!(tick, WaitTick::Pending);
    }

    // -- wait_for_daemon_ready_with (drives the loop with mock predicate) --

    #[tokio::test(flavor = "current_thread")]
    async fn returns_grace_error_when_no_lockfile_ever_observed() {
        // Tight grace + ceiling so the test resolves in well under a second.
        let c = cfg(150, 5_000, 25);
        let err = wait_for_daemon_ready_with(dead_endpoint(), || None, c)
            .await
            .expect_err("no-daemon path must fail, not hang");
        assert!(
            err.contains("no daemon lockfile observed"),
            "wrong error: {err}"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn returns_hard_ceiling_error_when_daemon_visible_but_unreachable() {
        // Daemon always-alive (mock returns Some), but no real socket → IPC
        // connect keeps failing → we hit the hard ceiling.
        let c = cfg(5_000, 200, 25);
        let err = wait_for_daemon_ready_with(dead_endpoint(), || Some(12_345), c)
            .await
            .expect_err("hard ceiling path must fail, not hang");
        assert!(err.contains("hard cap"), "wrong error: {err}");
        assert!(err.contains("12345"), "PID should appear: {err}");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn returns_daemon_exited_error_when_lockfile_disappears() {
        // First poll observes the daemon, every subsequent poll says None.
        // The loop must exit with DaemonExited, not hit the grace timeout.
        let polls = Arc::new(AtomicU32::new(0));
        let c = cfg(10_000, 10_000, 25);
        let polls_for_check = Arc::clone(&polls);
        let err = wait_for_daemon_ready_with(
            dead_endpoint(),
            move || {
                let n = polls_for_check.fetch_add(1, Ordering::SeqCst);
                if n == 0 {
                    Some(99_999)
                } else {
                    None
                }
            },
            c,
        )
        .await
        .expect_err("daemon-exit path must fail, not hang");
        assert!(err.contains("exited"), "wrong error: {err}");
        assert!(err.contains("99999"), "PID should appear: {err}");
    }
}
