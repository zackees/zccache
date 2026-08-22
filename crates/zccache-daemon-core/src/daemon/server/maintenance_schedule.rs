//! The one periodic-maintenance schedule shared by both service modes
//! (issue #1160).
//!
//! Before this module every background loop was hand-spawned in
//! `DaemonServer::run`, and `EmbeddedDaemon::start_background_tasks` spawned a
//! strict subset of them. That is drift by construction: a task added to
//! `run.rs` simply never ran inside a long-lived embedded host, and the only
//! symptom was something quietly not happening — memory never evicted, the
//! depgraph never persisted between host flushes, interrupted-write temps never
//! reclaimed.
//!
//! The fix is structural rather than a one-by-one port. [`MAINTENANCE_TASKS`]
//! is the single declaration of what periodic work the daemon does;
//! [`MaintenanceSchedule::start`] is the single site that spawns it; and the
//! set of names it reports having started is asserted against the declaration
//! in both modes by the parity test. A future task therefore runs in both modes
//! unless it is explicitly flagged standalone-only **with a rationale**, and
//! forgetting to flag it fails the test rather than shipping.
//!
//! ## Host-runtime contract
//!
//! Everything here spawns through [`MaintenanceSchedule::runtime_handle`] when
//! the embedded host supplied one (zccache#922). A bare `tokio::spawn` would
//! land on whichever runtime happened to be current at `start()` time, which is
//! exactly the contract violation the handle exists to prevent — so
//! `spawn_supervised` takes the handle too.

use super::*;

/// Interval between periodic depgraph snapshots. Matches the pre-#1160
/// standalone loop; the reasoning is bounded crash loss, not throughput.
const DEPGRAPH_SAVE_INTERVAL: Duration = Duration::from_secs(300);
/// Poll period for the standalone idle watchdog.
const IDLE_WATCHDOG_INTERVAL: Duration = Duration::from_secs(60);
/// Poll period for private-daemon owner-PID liveness.
const PRIVATE_DAEMON_POLL_INTERVAL: Duration = Duration::from_secs(5);

pub(super) const TASK_STAGED_TEMP_SWEEP: &str = "staged-temp-sweep";
pub(super) const TASK_MEMORY_EVICTION: &str = "memory-eviction";
pub(super) const TASK_DISK_MAINTENANCE: &str = "disk-maintenance";
pub(super) const TASK_DEPGRAPH_SAVE: &str = "depgraph-save";

/// zackees/soldr#2436 D5: registrations that force a save before the timer.
pub(super) const DEPGRAPH_SAVE_BATCH: usize = 32;
/// How often the batch condition is polled between interval saves.
pub(super) const DEPGRAPH_SAVE_BATCH_POLL: Duration = Duration::from_secs(5);

/// Pure decision core for the save loop: `Some(reason)` when a save is due.
pub(super) fn depgraph_save_due(
    waited: Duration,
    interval: Duration,
    contexts: usize,
    last_saved_contexts: usize,
) -> Option<&'static str> {
    if contexts.saturating_sub(last_saved_contexts) >= DEPGRAPH_SAVE_BATCH {
        return Some("batch");
    }
    if waited >= interval {
        return Some("interval");
    }
    None
}
pub(super) const TASK_LEGACY_TEMP_ROOT_CLEANUP: &str = "legacy-temp-root-cleanup";
pub(super) const TASK_IDLE_WATCHDOG: &str = "idle-watchdog";
pub(super) const TASK_PRIVATE_DAEMON_OWNERS: &str = "private-daemon-owner-reaper";

/// One member of the maintenance schedule.
pub(super) struct MaintenanceTaskSpec {
    pub(super) name: &'static str,
    /// `None` means the task runs in both modes.
    ///
    /// `Some(rationale)` marks it standalone-only. The rationale is mandatory
    /// so "embedded does not run this" is always an argued decision rather than
    /// an omission nobody noticed, and so `zccache status`-style reporting can
    /// explain the difference without reading this file.
    pub(super) standalone_only: Option<&'static str>,
}

impl MaintenanceTaskSpec {
    const fn shared(name: &'static str) -> Self {
        Self {
            name,
            standalone_only: None,
        }
    }

    const fn standalone_only(name: &'static str, rationale: &'static str) -> Self {
        Self {
            name,
            standalone_only: Some(rationale),
        }
    }

    pub(super) fn runs_in(&self, mode: ServiceMode) -> bool {
        mode == ServiceMode::Standalone || self.standalone_only.is_none()
    }
}

/// The complete declaration of the daemon's periodic maintenance.
///
/// Adding a row here without a `standalone_only` rationale makes the task run
/// in both modes; that is the intended default.
pub(super) const MAINTENANCE_TASKS: &[MaintenanceTaskSpec] = &[
    MaintenanceTaskSpec::shared(TASK_STAGED_TEMP_SWEEP),
    MaintenanceTaskSpec::shared(TASK_MEMORY_EVICTION),
    MaintenanceTaskSpec::shared(TASK_DISK_MAINTENANCE),
    MaintenanceTaskSpec::shared(TASK_DEPGRAPH_SAVE),
    MaintenanceTaskSpec::standalone_only(
        TASK_LEGACY_TEMP_ROOT_CLEANUP,
        "reclaims per-process state that only the standalone daemon binary ever \
         wrote under the system temp root; an embedded host has no such state, \
         so running it there would scan a directory this process does not own",
    ),
    MaintenanceTaskSpec::standalone_only(
        TASK_IDLE_WATCHDOG,
        "terminates the process when no client has been seen for the configured \
         idle timeout. The embedded service's lifetime is the host's to decide — \
         self-terminating would take the host's own runtime state with it",
    ),
    MaintenanceTaskSpec::standalone_only(
        TASK_PRIVATE_DAEMON_OWNERS,
        "shuts the daemon down once its caller-supplied owner PIDs are gone. An \
         embedded service's owner is the process it lives in, so the condition \
         is unreachable and the shutdown it triggers would be a host-visible \
         self-kill",
    ),
];

/// Which service is starting the schedule.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ServiceMode {
    Standalone,
    Embedded,
}

/// Injectable periods, so tests drive real ticks instead of sleeping.
#[derive(Debug, Clone, Copy)]
pub(super) struct MaintenanceIntervals {
    pub(super) memory_eviction: Duration,
    pub(super) depgraph_save: Duration,
}

impl Default for MaintenanceIntervals {
    fn default() -> Self {
        Self {
            memory_eviction: Duration::from_secs(
                crate::core::config::Config::default().eviction_interval_secs,
            ),
            depgraph_save: DEPGRAPH_SAVE_INTERVAL,
        }
    }
}

/// What [`MaintenanceSchedule::start`] actually spawned.
///
/// `started` is built by pushing at each spawn site — never derived from
/// [`MAINTENANCE_TASKS`] — so the parity test compares real behaviour against
/// the declaration rather than the declaration against itself.
pub(super) struct StartedMaintenance {
    pub(super) started: Vec<&'static str>,
    /// The disk-maintenance loop, which both shutdown paths join.
    pub(super) disk_maintenance: Option<tokio::task::JoinHandle<()>>,
}

pub(super) struct MaintenanceSchedule {
    state: Arc<SharedState>,
    policy: MaintenancePolicy,
    mode: ServiceMode,
    intervals: MaintenanceIntervals,
    runtime_handle: Option<tokio::runtime::Handle>,
    /// Standalone idle timeout in seconds; 0 disables the watchdog.
    idle_timeout_secs: u64,
    /// When false the *disk* loop is owned by the host
    /// ([`crate::embedded::MaintenanceOwnership::Host`]) and this schedule must
    /// not start a second one. The in-memory members are unaffected: no host
    /// API drives them, so suppressing them would restore the #1160 leak.
    disk_maintenance_enabled: bool,
}

impl MaintenanceSchedule {
    pub(super) fn new(
        state: Arc<SharedState>,
        policy: MaintenancePolicy,
        mode: ServiceMode,
    ) -> Self {
        Self {
            state,
            policy,
            mode,
            intervals: MaintenanceIntervals::default(),
            runtime_handle: None,
            idle_timeout_secs: 0,
            disk_maintenance_enabled: true,
        }
    }

    pub(super) fn with_runtime_handle(mut self, handle: Option<tokio::runtime::Handle>) -> Self {
        self.runtime_handle = handle;
        self
    }

    pub(super) fn with_idle_timeout_secs(mut self, idle_timeout_secs: u64) -> Self {
        self.idle_timeout_secs = idle_timeout_secs;
        self
    }

    pub(super) fn with_disk_maintenance(mut self, enabled: bool) -> Self {
        self.disk_maintenance_enabled = enabled;
        self
    }

    #[cfg(test)]
    pub(super) fn with_intervals(mut self, intervals: MaintenanceIntervals) -> Self {
        self.intervals = intervals;
        self
    }

    fn spawn<Fut>(&self, task: Fut) -> tokio::task::JoinHandle<()>
    where
        Fut: std::future::Future<Output = ()> + Send + 'static,
    {
        match &self.runtime_handle {
            Some(handle) => handle.spawn(task),
            None => tokio::spawn(task),
        }
    }

    fn is_shutting_down(&self) -> impl Fn() -> bool + Send + 'static {
        let state = Arc::clone(&self.state);
        move || state.shutdown_requested.load(Ordering::Acquire)
    }

    /// Start every task this mode owns and report what was started.
    pub(super) fn start(self) -> StartedMaintenance {
        let mut started = Vec::with_capacity(MAINTENANCE_TASKS.len());

        started.push(self.start_staged_temp_sweep());
        started.push(self.start_memory_eviction());
        let disk_maintenance = if self.disk_maintenance_enabled {
            let handle = spawn_disk_maintenance(
                Arc::clone(&self.state),
                self.policy,
                self.runtime_handle.as_ref(),
            );
            started.push(TASK_DISK_MAINTENANCE);
            Some(handle)
        } else {
            None
        };
        started.push(self.start_depgraph_save());

        if self.mode == ServiceMode::Standalone {
            started.push(self.start_legacy_temp_root_cleanup());
            if self.idle_timeout_secs > 0 {
                started.push(self.start_idle_watchdog());
            }
            started.push(self.start_private_daemon_owner_reaper());
        }

        // Surface the difference rather than leaving it implicit: a mode that
        // silently runs less maintenance than another is the whole bug class
        // #1160 is about, so the reason a member was skipped is emitted with
        // the member.
        for task in MAINTENANCE_TASKS
            .iter()
            .filter(|task| !task.runs_in(self.mode))
        {
            tracing::debug!(
                task = task.name,
                rationale = task.standalone_only.unwrap_or_default(),
                "maintenance task is standalone-only and was not started"
            );
        }
        debug_assert!(
            started
                .iter()
                .all(|name| MAINTENANCE_TASKS.iter().any(|task| task.name == *name)),
            "every started task must be declared in MAINTENANCE_TASKS"
        );

        StartedMaintenance {
            started,
            disk_maintenance,
        }
    }

    /// Reclaim interrupted-write `.tmp` files under the staged artifact root.
    ///
    /// A publish that dies after the generation rename but before the pointer
    /// commit leaves a complete orphaned generation that only this sweep
    /// reclaims. On an embedded host that never restarts the process, "startup
    /// only, standalone only" meant *never* (#1160c).
    fn start_staged_temp_sweep(&self) -> &'static str {
        let artifact_dir = self.state.artifact_dir.clone();
        std::mem::drop(self.spawn(async move {
            let swept = tokio::task::spawn_blocking(move || {
                cleanup_staged_artifact_temps(artifact_dir.as_path())
            })
            .await;
            match swept {
                Ok(Ok(0)) | Err(_) => {}
                Ok(Ok(removed)) => tracing::info!(removed, "swept staged artifact temp files"),
                Ok(Err(error)) => tracing::debug!(%error, "staged artifact temp cleanup skipped"),
            }
        }));
        TASK_STAGED_TEMP_SWEEP
    }

    /// Memory-budget eviction plus the ephemeral request/rsp cache trims.
    ///
    /// #1177: supervised. A panic here used to be silent — the task vanished
    /// and memory simply stopped being evicted, which surfaces days later as
    /// "the daemon got fat" with nothing in the log.
    fn start_memory_eviction(&self) -> &'static str {
        let state = Arc::clone(&self.state);
        let budget = crate::core::config::Config::default().max_memory_bytes;
        let interval = self.intervals.memory_eviction;
        std::mem::drop(supervise::spawn_supervised(
            TASK_MEMORY_EVICTION,
            self.is_shutting_down(),
            supervise::Restart::Idempotent,
            self.runtime_handle.as_ref(),
            move || {
                let state = Arc::clone(&state);
                async move {
                    loop {
                        tokio::time::sleep(interval).await;
                        let req_removed =
                            trim_request_cache(&state.request_cache, EPHEMERAL_CACHE_MAX_AGE);
                        let req_validation_removed = trim_request_validation_cache(
                            &state.request_validation_cache,
                            EPHEMERAL_CACHE_MAX_AGE,
                        );
                        let rsp_removed = trim_rsp_cache(&state.rsp_cache, EPHEMERAL_CACHE_MAX_AGE);
                        if req_removed > 0 || req_validation_removed > 0 || rsp_removed > 0 {
                            tracing::debug!(
                                request_cache_removed = req_removed,
                                request_validation_cache_removed = req_validation_removed,
                                rsp_cache_removed = rsp_removed,
                                "trimmed ephemeral daemon caches"
                            );
                        }
                        let (freed, items) =
                            super::run::run_memory_eviction_pass(&state, budget).await;
                        if items > 0 {
                            tracing::info!(
                                freed_bytes = freed,
                                items_removed = items,
                                "memory eviction"
                            );
                        }
                    }
                }
            },
        ));
        TASK_MEMORY_EVICTION
    }

    /// Periodic depgraph snapshot.
    ///
    /// #1177: supervised. A silent death here means the depgraph stops being
    /// persisted, so the next start is a cold graph and a full recompile — with
    /// nothing recording why. #1160b: embedded persisted only inside a
    /// host-driven `flush()`, so a host crash lost the entire delta rather than
    /// one interval of it.
    fn start_depgraph_save(&self) -> &'static str {
        let state = Arc::clone(&self.state);
        let interval = self.intervals.depgraph_save;
        std::mem::drop(supervise::spawn_supervised(
            TASK_DEPGRAPH_SAVE,
            self.is_shutting_down(),
            supervise::Restart::Idempotent,
            self.runtime_handle.as_ref(),
            move || {
                let state = Arc::clone(&state);
                async move {
                    let path = depgraph_file_path_for_cache_dir(&state.cache_dir);
                    // zackees/soldr#2436 D5: the interval alone left up to a
                    // full period of registrations to die with any un-drained
                    // daemon. A batch trigger bounds that loss: once
                    // DEPGRAPH_SAVE_BATCH new contexts have registered since
                    // the last save, save now instead of waiting the timer
                    // out. Ticks are the smaller of the interval and the
                    // batch poll so short test intervals stay honored.
                    let tick = interval.min(DEPGRAPH_SAVE_BATCH_POLL);
                    let mut last_saved_contexts = 0usize;
                    let mut waited = Duration::ZERO;
                    loop {
                        tokio::time::sleep(tick).await;
                        waited += tick;
                        let dg = state.dep_graph.load();
                        let contexts = dg.stats().context_count;
                        let decision =
                            depgraph_save_due(waited, interval, contexts, last_saved_contexts);
                        let Some(reason) = decision else {
                            continue;
                        };
                        waited = Duration::ZERO;
                        if let Some(parent) = path.parent() {
                            std::fs::create_dir_all(parent).ok();
                        }
                        match crate::depgraph::save_to_file(&dg, &path) {
                            Ok(()) => {
                                state.dep_graph_persisted.store(true, Ordering::Release);
                                last_saved_contexts = contexts;
                                tracing::info!(contexts, reason, "depgraph save");
                            }
                            Err(e) => tracing::warn!("periodic depgraph save failed: {e}"),
                        }
                    }
                }
            },
        ));
        TASK_DEPGRAPH_SAVE
    }

    fn start_legacy_temp_root_cleanup(&self) -> &'static str {
        let cache_dir = self.state.cache_dir.clone();
        std::mem::drop(self.spawn(async move {
            let cleaned = tokio::task::spawn_blocking(move || {
                crate::core::config::cleanup_legacy_temp_root_state(
                    &std::env::temp_dir(),
                    cache_dir.as_path(),
                    crate::ipc::is_process_alive,
                )
            })
            .await
            .unwrap_or(0);
            if cleaned > 0 {
                tracing::info!(cleaned, "cleaned legacy temp-root zccache state");
            }
        }));
        TASK_LEGACY_TEMP_ROOT_CLEANUP
    }

    fn start_idle_watchdog(&self) -> &'static str {
        let state = Arc::clone(&self.state);
        let timeout = self.idle_timeout_secs;
        std::mem::drop(self.spawn(async move {
            loop {
                tokio::time::sleep(IDLE_WATCHDOG_INTERVAL).await;
                let last = state.last_activity.load(Ordering::Relaxed);
                let idle = now_secs().saturating_sub(last);
                if idle >= timeout {
                    tracing::info!(idle_secs = idle, "idle timeout — shutting down");
                    // Persist a "died-idle" lifecycle event so operators can
                    // see why the daemon exited. Pair this with the "spawn"
                    // entry to reconstruct daemon lifetime from the lifecycle
                    // log alone — tracing stderr is NUL'd.
                    let (bincode_requests, bincode_requests_by_type) =
                        state.bincode_request_totals();
                    crate::core::lifecycle::write_event(
                        crate::core::lifecycle::EVENT_DIED_IDLE,
                        serde_json::json!({
                            "reason": crate::core::lifecycle::REASON_IDLE_TIMEOUT,
                            "idle_secs": idle,
                            "idle_timeout_secs": timeout,
                            // #840 Slice 5: idle timeout is the common death,
                            // so this is where most of the curve would be lost.
                            "bincode_requests": bincode_requests,
                            "bincode_requests_by_type": bincode_requests_by_type,
                        }),
                    );
                    state.shutdown_requested.store(true, Ordering::Release);
                    state.shutdown.notify_waiters();
                    break;
                }
            }
        }));
        TASK_IDLE_WATCHDOG
    }

    fn start_private_daemon_owner_reaper(&self) -> &'static str {
        let state = Arc::clone(&self.state);
        std::mem::drop(self.spawn(async move {
            loop {
                tokio::time::sleep(PRIVATE_DAEMON_POLL_INTERVAL).await;
                if !state.private_daemon.is_enabled().await {
                    continue;
                }
                let prune = state
                    .private_daemon
                    .prune_dead_owner_pids(crate::ipc::is_process_alive)
                    .await;
                if !prune.removed_pids.is_empty() {
                    tracing::info!(
                        removed_pids = ?prune.removed_pids,
                        "private daemon owner PIDs exited"
                    );
                }
                if prune.should_shutdown {
                    tracing::info!("private daemon has no live owner PIDs - shutting down");
                    crate::core::lifecycle::write_event(
                        crate::core::lifecycle::EVENT_DIED_PRIVATE_OWNER_EXIT,
                        serde_json::json!({
                            "reason": "private-owner-pids-exited",
                            "uptime_secs": now_secs().saturating_sub(state.start_time),
                            "removed_pids": prune.removed_pids,
                        }),
                    );
                    state.shutdown_requested.store(true, Ordering::Release);
                    state.shutdown.notify_waiters();
                    break;
                }
            }
        }));
        TASK_PRIVATE_DAEMON_OWNERS
    }
}

#[cfg(test)]
#[path = "tests/maintenance_schedule.rs"]
mod tests;
