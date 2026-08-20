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

/// Identity of the daemon instance a client is about to talk to.
///
/// #1161: every kill must name the instance that actually failed, and the
/// only way to do that is to read the identity **before** the exchange. By
/// the time a request has failed, the lock file may already name a
/// replacement some other client spawned — killing "whoever is current" is
/// how one client's timeout becomes a kill chain through a `-j16` herd.
///
/// `None` means the identity could not be established, and every kill path
/// treats that as a refusal rather than as a wildcard.
#[must_use]
pub fn current_daemon_instance(
) -> Option<running_process::broker::protocol_v2::backend_handle::DaemonProcess> {
    crate::ipc::read_backend_identity()
}

#[derive(Debug)]
pub(crate) enum VersionCheck {
    Ok,
    Unreachable,
    DaemonOlder { daemon_ver: String },
    DaemonNewer,
    CommError,
    ClientConfigError(String),
}

pub async fn connect_client(
    endpoint: &str,
) -> Result<crate::ipc::IpcConnection, crate::ipc::IpcError> {
    let mut conn = crate::ipc::connect_daemon(endpoint).await?;
    conn.set_recv_timeout(crate::ipc::DEFAULT_CLIENT_RECV_TIMEOUT);
    Ok(conn)
}

pub(crate) async fn check_daemon_version(endpoint: &str) -> VersionCheck {
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
/// Does a short follow-up probe say the daemon is alive after all?
///
/// #1161 leg 2. `check_daemon_version` maps a `Status` timeout to
/// `CommError`, which the recovery path treated as "replace it". But a
/// timeout is not evidence of death — it is equally the signature of a
/// daemon busy serving a `-j16` burst. #753 already established the fix on
/// the wedge path: ask again, cheaply, and only escalate if that also fails.
/// This reuses that classifier rather than inventing a second policy.
///
/// `true` means "alive, just slow — leave it alone". Probe disabled via
/// `ZCCACHE_WEDGE_PROBE_BUDGET_MS=0` reads as `false`, preserving the
/// unconditional-replace behaviour for anyone A/B-testing against it.
async fn probe_says_daemon_is_merely_busy(endpoint: &str) -> bool {
    use crate::cli::commands::wrap::ipc::{
        classify_probe_outcome, probe_daemon_responsive, wedge_probe_budget, WedgeAction,
    };
    let Some(budget) = wedge_probe_budget() else {
        return false;
    };
    matches!(
        classify_probe_outcome(probe_daemon_responsive(endpoint, budget).await),
        WedgeAction::DowngradeNoKill
    )
}

/// How long a retiring daemon gets to finish its durable drain before the
/// stopper escalates to a force kill (#1161 leg 3).
///
/// Matched to the daemon's own `INDEX_WRITER_SHUTDOWN_DRAIN_TIMEOUT` (30 s,
/// `daemon/server/wal.rs`). The two must stay in step: a stopper budget below
/// the daemon's drain budget guarantees killing it mid-flush, which truncates
/// `index.bin` and costs a full recompile — the failure this leg exists to
/// stop. The previous value was 200 ms.
const GRACEFUL_DRAIN_BUDGET: std::time::Duration = std::time::Duration::from_secs(30);

/// How long to wait for the OS to reap a process we did force-kill. Short:
/// SIGKILL/TerminateProcess is not negotiable, so this only covers reaping
/// latency, not any work by the daemon.
const FORCE_KILL_REAP_BUDGET: std::time::Duration = std::time::Duration::from_secs(5);

/// Poll interval while waiting for a process to exit. Small enough that a
/// fast drain is not padded by the poll, coarse enough not to spin.
const PROCESS_EXIT_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(100);

/// Wait up to `budget` for `pid` to leave. `true` if it exited.
async fn wait_for_process_exit(pid: u32, budget: std::time::Duration) -> bool {
    wait_for_exit_while(budget, move || crate::ipc::is_process_alive(pid)).await
}

/// [`wait_for_process_exit`] against an injected liveness predicate.
///
/// The seam exists because "a PID that is reliably dead" is not portable and
/// "a PID that stays alive" is a race against the OS. `is_process_alive` is
/// `kill(pid, 0)` on unix and `OpenProcess` on Windows, and they disagree on
/// edge values — PID 0 reads alive on Linux and dead on Windows. Injecting the
/// predicate makes the *waiting* logic deterministic everywhere and leaves
/// `is_process_alive`, which has its own tests, as the only thing depending on
/// OS behaviour.
async fn wait_for_exit_while(budget: std::time::Duration, is_alive: impl Fn() -> bool) -> bool {
    let deadline = std::time::Instant::now() + budget;
    loop {
        if !is_alive() {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(PROCESS_EXIT_POLL_INTERVAL).await;
    }
}

/// Replace a daemon we failed to talk to.
///
/// `failed_instance` is the identity captured **before** the failed exchange.
/// #1161: without it this function re-read the lock file at kill time and
/// killed whoever was named there *now*. Under a `ninja -j16` burst that is a
/// kill chain — client A times out against a saturated-but-healthy daemon and
/// replaces it, client B arrives, reads the lock, and kills the freshly
/// spawned replacement A just created.
///
/// `None` means the caller could not establish which instance it was talking
/// to, and the kill is refused rather than aimed at whatever is current.
async fn stop_stale_daemon(
    endpoint: &str,
    failed_instance: Option<&running_process::broker::protocol_v2::backend_handle::DaemonProcess>,
) -> Option<u32> {
    stop_daemon_instance(endpoint, failed_instance, GRACEFUL_DRAIN_BUDGET).await
}

/// How long a *wedged* daemon gets before the kill lands.
///
/// A daemon that already missed its per-request budget and then failed a
/// follow-up responsiveness probe is not going to complete a 30 s durable
/// drain — waiting the graceful budget would just delay the failure the
/// fail-fast policy (#955) exists to surface immediately. The identity gate
/// still applies: fail-fast is about *how long we wait*, never about *whom we
/// kill*.
const WEDGE_DRAIN_BUDGET: std::time::Duration = std::time::Duration::from_millis(200);

/// Kill a daemon the wrapper found wedged — but only the instance it was
/// talking to.
///
/// `wedged_instance` must be the identity captured **before** the request that
/// wedged. `None` refuses the kill: under a build herd, "whoever the lock
/// names now" is frequently a healthy replacement another client just spawned.
pub async fn stop_wedged_daemon(
    endpoint: &str,
    wedged_instance: Option<&running_process::broker::protocol_v2::backend_handle::DaemonProcess>,
) -> Option<u32> {
    stop_daemon_instance(endpoint, wedged_instance, WEDGE_DRAIN_BUDGET).await
}

/// Deliberately replace the daemon currently serving `endpoint`.
///
/// Unlike the recovery paths, this is not triggered by a failure — the caller
/// wants a daemon with different settings (today: the tokio-console profile).
/// It is still identity-bound: the instance is read first and the stop refuses
/// if the lock has since moved to someone else, so a profile restart cannot
/// take out a replacement another client spawned in between.
pub(crate) async fn replace_running_daemon(endpoint: &str, reason: &str) -> Result<(), String> {
    let instance = current_daemon_instance();
    let killed_pid = stop_stale_daemon(endpoint, instance.as_ref()).await;
    spawn_and_wait(endpoint, reason, killed_pid).await
}

/// Shared body of [`stop_stale_daemon`] and [`stop_wedged_daemon`].
///
/// `drain_budget` is the only difference between them: the identity gate,
/// the forensics, and the escalation are identical, because "which daemon may
/// I kill" is not a question the caller's urgency gets to answer.
async fn stop_daemon_instance(
    endpoint: &str,
    failed_instance: Option<&running_process::broker::protocol_v2::backend_handle::DaemonProcess>,
    drain_budget: std::time::Duration,
) -> Option<u32> {
    // Gate before the Shutdown request, not just before the kill: asking an
    // innocent daemon to retire is itself the damage this issue is about.
    match failed_instance {
        Some(expected) if crate::ipc::daemon_identity_matches(expected) => {}
        Some(expected) => {
            tracing::warn!(
                expected_pid = expected.pid,
                expected_started_at_unix_ms = expected.started_at_unix_ms,
                current_pid = crate::ipc::read_backend_identity().map(|d| d.pid),
                "refusing to replace the daemon: the instance on disk is not the one that failed"
            );
            return None;
        }
        None => {
            tracing::warn!(
                "refusing to replace the daemon: no recorded identity for the failed instance; \
                 run `zccache stop` if a stale daemon is genuinely wedged"
            );
            return None;
        }
    }

    // The instance we verified above, not a fresh lock read. #1161 leg 1
    // gated on identity; re-reading the lock here would reopen the same
    // window on the kill itself.
    let outgoing_pid = failed_instance.map(|instance| instance.pid);

    let _ = crate::ipc::daemon_control_roundtrip(
        endpoint,
        crate::ipc::DaemonControlRequest::Shutdown,
        None,
    )
    .await;

    let pid = outgoing_pid?;

    // #1161 leg 3: this used to sleep 200 ms and then SIGKILL
    // unconditionally. The daemon's own shutdown drain is *30 s*
    // (`INDEX_WRITER_SHUTDOWN_DRAIN_TIMEOUT`), so the old grace was two
    // orders of magnitude short: it killed daemons mid-flush, truncating
    // `index.bin` and turning an orderly replacement into "recompile
    // everything". Wait for the drain the daemon is entitled to, and reserve
    // the fast kill for one that will not leave.
    let drained_cleanly = wait_for_process_exit(pid, drain_budget).await;
    if drained_cleanly {
        crate::ipc::remove_lock_file();
        // #1170 change 2, step 3: the lock file was never the whole of a dead
        // instance's state. Clear the rest here rather than leaving `<lock>
        // .spawn` to a 20 s staleness timer and the identity file to nothing
        // at all.
        super::recovery::clear_stale_daemon_state();
        // Return the pid even though nothing was killed: the caller uses it to
        // link old -> new in the takeover lifecycle events. Previously a
        // daemon that exited inside the 200 ms window returned `None` here and
        // its lineage was silently lost.
        return Some(pid);
    }

    // Loud on escalation, per the timeout-forensics convention: a durable
    // event as well as the log line, with the stage and how long we actually
    // waited, so "why was my warm daemon killed" is answerable afterwards.
    tracing::warn!(
        pid,
        waited_ms = drain_budget.as_millis() as u64,
        stage = "post-shutdown-drain",
        "daemon did not exit within its drain budget; escalating to force kill"
    );
    crate::core::lifecycle::write_event(
        crate::core::lifecycle::EVENT_DAEMON_DIED,
        serde_json::json!({
            "pid": pid,
            "reason": "drain_budget_exhausted",
            "stage": "post-shutdown-drain",
            "waited_ms": drain_budget.as_millis() as u64,
            "endpoint": endpoint,
        }),
    );

    let kill_ok = crate::ipc::force_kill_process(pid).is_ok();
    if kill_ok {
        wait_for_process_exit(pid, FORCE_KILL_REAP_BUDGET).await;
    }
    crate::ipc::remove_lock_file();
    super::recovery::clear_stale_daemon_state();
    let killed_pid = kill_ok.then_some(pid);

    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    killed_pid
}

/// Acquire a working daemon, or fail loudly within a bounded budget.
///
/// #1170 change 2 wraps the ladder below in two bounds that did not exist:
/// a **total deadline** (`ZCCACHE_RECOVERY_BUDGET_MS`, default 30 s — the
/// worst case used to be minutes), and a **cross-invocation breaker**. The
/// wrapper is a fresh process per translation unit, so nothing was shared
/// between them: a 1000-TU build against a dead daemon paid the whole ladder
/// 1000 times. Now the first exhaustion writes a marker and the rest fail in
/// microseconds, reporting the *original* cause rather than "breaker open".
pub async fn ensure_daemon(endpoint: &str) -> Result<(), String> {
    if let Some(reason) = super::recovery::breaker_reason_if_open() {
        return Err(format!(
            "daemon recovery already failed on this cache root and is in its cool-down: {reason}"
        ));
    }
    let outcome = match super::recovery::recovery_budget() {
        Some(budget) => match tokio::time::timeout(budget, ensure_daemon_ladder(endpoint)).await {
            Ok(outcome) => outcome,
            Err(_) => Err(format!(
                "daemon recovery exceeded its {}ms budget ({})",
                budget.as_millis(),
                super::recovery::RECOVERY_BUDGET_ENV
            )),
        },
        None => ensure_daemon_ladder(endpoint).await,
    };
    match &outcome {
        // A working daemon is proof the outage is over. Clearing on every
        // success — not only after a recovery — is what stops a stale marker
        // from fast-failing a healthy build.
        Ok(()) => super::recovery::clear_breaker(),
        Err(reason) => super::recovery::open_breaker(reason),
    }
    outcome
}

async fn ensure_daemon_ladder(endpoint: &str) -> Result<(), String> {
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
    // #1161: capture *before* probing. This names the instance we are about
    // to talk to, so a later kill can be bound to it rather than to whatever
    // the lock file says once the probe has already failed.
    let probed_instance = crate::ipc::read_backend_identity();
    match check_daemon_version(endpoint).await {
        VersionCheck::Ok | VersionCheck::DaemonNewer => return Ok(()),
        VersionCheck::DaemonOlder { daemon_ver } => {
            tracing::info!(
                daemon_ver,
                client_ver = crate::core::VERSION,
                "daemon is older than client, auto-recovering"
            );
            let killed_pid = stop_stale_daemon(endpoint, probed_instance.as_ref()).await;
            return spawn_and_wait(
                endpoint,
                crate::core::lifecycle::REASON_REPLACED_STALE_VERSION,
                killed_pid,
            )
            .await;
        }
        VersionCheck::CommError => {
            // #1161 leg 2: a `CommError` here is usually a *Status probe
            // timeout*, and `check_daemon_version` deliberately does not treat
            // a timeout as "unreachable". Under load that is exactly what a
            // saturated-but-healthy daemon produces, and replacing it is how a
            // cohort of clients destroys the warm daemon they are all waiting
            // on. Ask a second, cheaper question before concluding it is dead
            // — the same probe #753 already applies on the wedge path.
            if probe_says_daemon_is_merely_busy(endpoint).await {
                tracing::info!(
                    "daemon answered a probe after a status timeout; treating it as busy \
                     rather than replacing it"
                );
                return Ok(());
            }
            tracing::info!("cannot communicate with daemon, auto-recovering");
            let killed_pid = stop_stale_daemon(endpoint, probed_instance.as_ref()).await;
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
            // Re-read every iteration: a daemon replaced legitimately between
            // attempts is a different instance, and the kill must follow.
            let attempt_instance = crate::ipc::read_backend_identity();
            match check_daemon_version(endpoint).await {
                VersionCheck::Ok | VersionCheck::DaemonNewer => return Ok(()),
                VersionCheck::DaemonOlder { daemon_ver } => {
                    tracing::info!(
                        daemon_ver,
                        client_ver = crate::core::VERSION,
                        "daemon is older than client during startup, auto-recovering"
                    );
                    let killed_pid = stop_stale_daemon(endpoint, attempt_instance.as_ref()).await;
                    return spawn_and_wait(
                        endpoint,
                        crate::core::lifecycle::REASON_REPLACED_STALE_VERSION,
                        killed_pid,
                    )
                    .await;
                }
                VersionCheck::CommError => {
                    // Same reasoning as the arm above, and this one matters
                    // more: this loop runs while a daemon is still starting
                    // up, which is precisely when it is too busy to answer a
                    // 2 s Status ping.
                    if probe_says_daemon_is_merely_busy(endpoint).await {
                        continue;
                    }
                    let killed_pid = stop_stale_daemon(endpoint, attempt_instance.as_ref()).await;
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

mod deploy;
#[allow(deprecated)]
pub use deploy::gc_daemon_spawn_logs;
pub use deploy::{
    deployed_daemon_path, gc_log_directory, gc_log_directory_in, materialize_daemon_exe,
    materialize_daemon_exe_to, spawn_daemon,
};

#[cfg(test)]
mod tests;
