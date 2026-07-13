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
    ClientConfigError(String),
}

#[cfg(unix)]
pub async fn connect_client(
    endpoint: &str,
) -> Result<crate::ipc::IpcConnection, crate::ipc::IpcError> {
    let mut conn = crate::ipc::connect_daemon(endpoint).await?;
    conn.set_recv_timeout(crate::ipc::DEFAULT_CLIENT_RECV_TIMEOUT);
    Ok(conn)
}

#[cfg(windows)]
pub async fn connect_client(
    endpoint: &str,
) -> Result<crate::ipc::IpcClientConnection, crate::ipc::IpcError> {
    let mut conn = crate::ipc::connect_daemon(endpoint).await?;
    conn.set_recv_timeout(crate::ipc::DEFAULT_CLIENT_RECV_TIMEOUT);
    Ok(conn)
}

async fn check_daemon_version(endpoint: &str) -> VersionCheck {
    match crate::ipc::daemon_control_roundtrip(
        endpoint,
        crate::ipc::DaemonControlRequest::Status,
        Some(super::status_probe_timeout()),
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

async fn spawn_and_wait(
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
    // Issue #952: single-flight the spawn across a client herd. A
    // -j16 cold start used to produce 16+ spawn-attempts within
    // milliseconds — each losing client paid a fork/exec plus lockfile
    // contention that delayed the winner's bind by seconds. Exactly
    // one client wins the slot and spawns; the rest park directly on
    // the ready-wait below.
    let spawn_slot = acquire_spawn_slot();
    let meta = crate::core::lifecycle::client_meta(crate::core::VERSION);
    if spawn_slot.is_some() {
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
                // #755 acceptance #4: distinguishes fbuild's bundled
                // binary from a PyPI install when both share an endpoint.
                "client_version": meta["client_version"],
                "client_binary_path": meta["client_binary_path"],
            }),
        );
        spawn_daemon(endpoint)?;
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

    // The slot guard must survive until the daemon is READY: releasing
    // right after spawn would let a late-arriving client win a second
    // slot before the daemon binds its lockfile.
    let wait_result = wait_for_daemon_ready(endpoint).await;
    drop(spawn_slot);
    wait_result?;

    // #755 acceptance #2: emit the linked daemon-died + pipe-handover
    // pair so the takeover lineage is reconstructable from a single
    // `grep`. Best-effort — if the new daemon's PID isn't visible
    // post-ready (lockfile race) we skip; the regular `spawn` line
    // still records the new daemon's identity.
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

/// Issue #952: RAII guard for the single-flight spawn slot. Removes the
/// slot file on drop so the next cold start can win a fresh slot.
pub(crate) struct SpawnSlotGuard {
    path: std::path::PathBuf,
}

impl Drop for SpawnSlotGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// How long a spawn slot may exist before another client treats it as
/// abandoned (winner crashed between slot-create and daemon bind).
/// Generous relative to a healthy spawn (~1-5s) but short enough that a
/// crashed winner doesn't wedge the herd for long — the parked losers'
/// ready-wait grace is 10s, so one stale window later a new winner
/// spawns.
const SPAWN_SLOT_STALE: std::time::Duration = std::time::Duration::from_secs(20);

/// Issue #952: try to become the one client that spawns the daemon.
///
/// Winner: atomically creates `<daemon-lock>.spawn` (`create_new`) and
/// gets a guard that removes it once the daemon is ready (or the spawn
/// failed). Losers get `None` and park on the ready-wait. A slot older
/// than [`SPAWN_SLOT_STALE`] is treated as abandoned and reclaimed.
/// Fail-open: if the filesystem refuses the arbitration entirely
/// (permissions, exotic tmpfs), the caller behaves as the winner —
/// worst case is the pre-#952 thundering herd, never a lost spawn.
pub(crate) fn acquire_spawn_slot() -> Option<SpawnSlotGuard> {
    let lock_path = crate::ipc::lock_file_path();
    let slot_path = std::path::PathBuf::from(format!("{}.spawn", lock_path.display()));
    acquire_spawn_slot_at(slot_path, SPAWN_SLOT_STALE)
}

/// Path-parameterized core of [`acquire_spawn_slot`], split out so the
/// arbitration logic is unit-testable without touching the process-
/// global endpoint/lockfile config.
fn acquire_spawn_slot_at(
    slot_path: std::path::PathBuf,
    stale_after: std::time::Duration,
) -> Option<SpawnSlotGuard> {
    if let Some(parent) = slot_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    for attempt in 0..2 {
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&slot_path)
        {
            Ok(mut file) => {
                use std::io::Write as _;
                let _ = writeln!(file, "{}", std::process::id());
                return Some(SpawnSlotGuard { path: slot_path });
            }
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                let stale = std::fs::metadata(&slot_path)
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|modified| modified.elapsed().ok())
                    .is_some_and(|age| age > stale_after);
                if stale && attempt == 0 {
                    let _ = std::fs::remove_file(&slot_path);
                    continue;
                }
                return None;
            }
            // Unexpected fs error: fail open (spawn without a guard).
            Err(_) => {
                return Some(SpawnSlotGuard {
                    path: std::path::PathBuf::new(),
                });
            }
        }
    }
    None
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
async fn stop_stale_daemon(endpoint: &str) -> Option<u32> {
    let _ = crate::ipc::daemon_control_roundtrip(
        endpoint,
        crate::ipc::DaemonControlRequest::Shutdown,
        None,
    )
    .await;
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    let killed_pid = if let Some(pid) = crate::ipc::check_running_daemon() {
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

    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    killed_pid
}

pub async fn ensure_daemon(endpoint: &str) -> Result<(), String> {
    // Issue #982: under the host no-spawn guard a reachable,
    // version-compatible daemon may still be used, but every other
    // outcome — including the stale-daemon replace paths, which would
    // stop the old daemon before respawning — fails here, BEFORE
    // anything is stopped or killed.
    if crate::core::config::daemon_spawn_disabled() {
        return match check_daemon_version(endpoint).await {
            VersionCheck::Ok | VersionCheck::DaemonNewer => Ok(()),
            _ => Err(crate::core::config::no_spawn_error("zccache-daemon")),
        };
    }
    match check_daemon_version(endpoint).await {
        VersionCheck::Ok | VersionCheck::DaemonNewer => return Ok(()),
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
                    let killed_pid = stop_stale_daemon(endpoint).await;
                    return spawn_and_wait(
                        endpoint,
                        crate::core::lifecycle::REASON_REPLACED_STALE_VERSION,
                        killed_pid,
                    )
                    .await;
                }
                VersionCheck::CommError => {
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
            "daemon process {pid} exists but not accepting connections after retrying"
        ));
    }

    spawn_and_wait(endpoint, crate::core::lifecycle::REASON_INITIAL_START, None).await
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

/// File name the daemon binary is deployed under. The daemon runs from a copy
/// of the CLI (self) placed under the versioned cache dir with the daemon's own
/// name, so argv[0] dispatch (#998) routes the copy to the daemon and
/// `verify_pid_exe_stem(pid, "zccache-daemon")` (zccache-ipc) recognizes it.
fn deployed_daemon_file_name() -> &'static str {
    if cfg!(windows) {
        "zccache-daemon.exe"
    } else {
        "zccache-daemon"
    }
}

/// Path the daemon binary is materialized to:
/// `<versioned cache dir>/zccache-daemon[.exe]` — e.g.
/// `~/.zccache/v<VERSION>/zccache-daemon.exe`.
///
/// Stable, version-rooted, using the daemon's own name (issue #999). Because
/// each installed version owns its own `v<VERSION>/` directory, a stale copy
/// from an older install can never masquerade as a newer one — this is the
/// structural fix for the #760 "soft-shadow" downgrade the old random-name
/// `runtime-binaries/` copies allowed.
#[must_use]
pub fn deployed_daemon_path() -> NormalizedPath {
    crate::core::config::daemon_state_dir().join(deployed_daemon_file_name())
}

/// Materialize the daemon binary at [`deployed_daemon_path`] by copying
/// `source` — the running CLI (`current_exe()`), which contains the daemon
/// via argv[0] dispatch.
///
/// **Idempotent**: if the destination already exists with a size matching the
/// source it is reused unchanged, so N concurrent same-version CLIs converge
/// on one file with no repeated multi-MB copies. **Atomic**: the copy lands on
/// a temp name in the same directory and is `rename`d into place, so no reader
/// ever executes a torn binary; a concurrent materializer that wins the rename
/// is tolerated (we drop our temp and use theirs).
pub fn materialize_daemon_exe(source: &Path) -> Result<std::path::PathBuf, std::io::Error> {
    let dest = deployed_daemon_path().as_path().to_path_buf();
    materialize_daemon_exe_to(source, &dest)
}

/// Test seam for [`materialize_daemon_exe`]: materialize `source` at `dest`.
pub fn materialize_daemon_exe_to(
    source: &Path,
    dest: &Path,
) -> Result<std::path::PathBuf, std::io::Error> {
    // Idempotency + completeness gate: an existing dest whose size matches the
    // source is a good copy of this same-version binary — reuse it untouched.
    // A partial (crashed mid-copy) file has a different size and is replaced.
    if let (Ok(dm), Ok(sm)) = (std::fs::metadata(dest), std::fs::metadata(source)) {
        if dm.is_file() && dm.len() == sm.len() {
            return Ok(dest.to_path_buf());
        }
    }
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // Temp name in the SAME dir so the finalizing rename stays on one
    // filesystem (atomic). Unique per process so racing materializers don't
    // clobber each other's temp.
    let rand_id: u32 = std::process::id()
        ^ std::time::UNIX_EPOCH
            .elapsed()
            .unwrap_or_default()
            .subsec_nanos();
    let tmp = dest.with_file_name(format!("zccache-daemon.tmp.{rand_id}"));
    std::fs::copy(source, &tmp)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755));
    }
    match std::fs::rename(&tmp, dest) {
        Ok(()) => Ok(dest.to_path_buf()),
        Err(e) => {
            // A concurrent materializer may have won the rename (or Windows is
            // refusing to replace a dest another process just created). Drop
            // our temp; if a usable dest now exists, use it.
            let _ = std::fs::remove_file(&tmp);
            if dest.is_file() {
                Ok(dest.to_path_buf())
            } else {
                Err(e)
            }
        }
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
    let dir = crate::core::config::daemon_state_dir().join(DAEMON_SPAWN_LOGS_SUBDIR);
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
    let dir = crate::core::config::daemon_state_dir().join(DAEMON_SPAWN_LOGS_SUBDIR);
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

pub fn spawn_daemon(endpoint: &str) -> Result<(), String> {
    // Issue #982: backstop for the host no-spawn guard — refuse before
    // `materialize_daemon_exe` copies anything, so a guarded run leaves zero
    // daemon artifacts on disk.
    if crate::core::config::daemon_spawn_disabled() {
        return Err(crate::core::config::no_spawn_error("zccache-daemon"));
    }
    // GC old spawn logs (the runtime-binaries dir is gone — the daemon binary
    // is now a single stable version-rooted copy, pruned per-version by
    // `zccache clear`, #1005).
    gc_log_directory();

    // #999: the daemon is a copy of THIS binary (the CLI, which contains the
    // daemon via argv[0] dispatch) placed at the stable version-rooted path.
    // Copying from the install path means the install path is never
    // file-locked by a running daemon (the daemon runs from the copy), so
    // `pip install --upgrade zccache` / `rm -rf <project>` still succeed
    // (issue #134). Fall back to spawning the current exe in place if the
    // copy fails — the daemon's own `unlock_exe()` then handles the rename.
    let self_exe = std::env::current_exe()
        .map_err(|e| format!("cannot resolve current executable to deploy daemon: {e}"))?;
    let bin_owned: std::path::PathBuf;
    // `spawned_as_daemon` is true when we run the materialized copy, whose
    // argv[0] file stem is `zccache-daemon` so #998's dispatch routes it to
    // the daemon. On the fallback we run THIS exe in place (argv[0] =
    // `zccache`), which dispatches to the CLI — so we must enter the daemon
    // via the explicit `daemon-run` escape hatch instead.
    let (spawn_bin, spawned_as_daemon): (&Path, bool) = match materialize_daemon_exe(&self_exe) {
        Ok(p) => {
            bin_owned = p;
            (&bin_owned, true)
        }
        Err(_) => (self_exe.as_path(), false),
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
    // On the fallback (running this exe in place), route into the daemon via
    // the argv[0]-independent `daemon-run` escape hatch (#998); the
    // materialized copy needs no subcommand because argv[0] already selects
    // the daemon.
    if !spawned_as_daemon {
        cmd.arg("daemon-run");
    }
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

    // -- acquire_spawn_slot_at (issue #952 single-flight arbiter) ----------

    #[test]
    fn spawn_slot_first_caller_wins_second_parks() {
        let dir = tempfile::tempdir().unwrap();
        let slot = dir.path().join("daemon.lock.spawn");
        let winner = acquire_spawn_slot_at(slot.clone(), Duration::from_secs(20));
        assert!(winner.is_some(), "first caller must win the slot");
        assert!(
            acquire_spawn_slot_at(slot.clone(), Duration::from_secs(20)).is_none(),
            "second caller must park while the slot is held"
        );
        drop(winner);
        assert!(
            acquire_spawn_slot_at(slot, Duration::from_secs(20)).is_some(),
            "slot must be reusable after the winner's guard drops"
        );
    }

    #[test]
    fn spawn_slot_stale_holder_is_reclaimed() {
        let dir = tempfile::tempdir().unwrap();
        let slot = dir.path().join("daemon.lock.spawn");
        std::fs::write(&slot, "12345\n").unwrap();
        // A zero staleness window means any existing slot is abandoned;
        // sleep a few ms so the file's mtime age is strictly positive.
        std::thread::sleep(Duration::from_millis(20));
        let reclaimed = acquire_spawn_slot_at(slot, Duration::from_millis(0));
        assert!(
            reclaimed.is_some(),
            "an abandoned slot older than the staleness window must be reclaimed"
        );
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
