//! `Request::GenericToolExec` handler — generic tool caching (issue #272).
//!
//! Lets arbitrary tools that don't speak a compiler-style CLI plug into the
//! daemon's artifact cache. Inputs are declared explicitly by the caller
//! (`input_files`, `input_env`, `input_extra`, optional `tool_hash`,
//! optional `include_scan_files` for Path A, optional `depfile` for Path B).
//! On a cache hit the tool process is NOT spawned; cached stdout, stderr,
//! exit code, and declared `output_files` are replayed. On miss the tool
//! runs and the result is stored under a deterministic cache key.
//!
//! Cache-key composition (domain tag `zccache-exec-key-v2`):
//!   - tool identity (caller-supplied hash, or daemon-hashed binary)
//!   - args in argv order, after `key_args_filter` regex drops
//!   - sorted (name=value) env subset
//!   - cwd (when `cwd_in_key`)
//!   - sorted (path, content-hash) input file pairs
//!   - sorted (path, content-hash) Path A include-scan transitive headers
//!   - sorted (path, content-hash) Path B stored depfile dep set, if any
//!   - declared output_file names (so changing the capture set invalidates)
//!   - input_extra bytes
//!
//! Concurrent callers with the same full cache key coalesce on
//! `state.in_flight_exec` — the first inserter spawns the tool; the rest
//! wait on the shared `Notify` and re-attempt the cache lookup once it
//! fires.

use super::*;
use crate::depgraph::scanner::scan_recursive;
use crate::depgraph::search_paths::IncludeSearchPaths;
use crate::protocol::{ExecCachePolicy, ExecOutputStreams};
use dashmap::mapref::entry::Entry;

static EXEC_STAGE_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

struct ExecStagedPlan {
    outputs: Vec<(NormalizedPath, NormalizedPath)>,
    rewritten_args: Vec<String>,
    root: PathBuf,
}

impl ExecStagedPlan {
    fn build(
        staging_dir: &Path,
        args: &[String],
        output_files: &[NormalizedPath],
        cwd: &Path,
    ) -> StagedPlanOutcome<Self> {
        if !exec_staging_enabled() {
            return StagedPlanOutcome::Unsupported(StagedPlanReason::LaneDisabled);
        }
        if output_files.is_empty() {
            return StagedPlanOutcome::Unsupported(StagedPlanReason::NoDeclaredOutputs);
        }
        let root = staging_dir.join(format!(
            ".exec-{}-{}",
            std::process::id(),
            EXEC_STAGE_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        if let Err(source) = std::fs::create_dir_all(&root) {
            return StagedPlanOutcome::Error(StagedPlanError {
                reason: StagedPlanReason::StagingDirectoryCreate,
                source,
            });
        }
        let result = (|| {
            let mut outputs = Vec::with_capacity(output_files.len());
            let mut rewritten_args = args.to_vec();
            for declared in output_files {
                let requested: NormalizedPath = absolutize(declared.as_path(), cwd).into();
                let Some(filename) = requested.file_name() else {
                    return StagedPlanOutcome::Unsupported(StagedPlanReason::OutputMissingFilename);
                };
                let staged: NormalizedPath = root.join(filename).into();
                if outputs
                    .iter()
                    .any(|(_, existing): &(NormalizedPath, NormalizedPath)| existing == &staged)
                {
                    return StagedPlanOutcome::Unsupported(StagedPlanReason::OutputNameCollision);
                }
                let requested_text = requested.to_string_lossy();
                let declared_text = declared.to_string_lossy();
                let mut replaced = false;
                for arg in &mut rewritten_args {
                    if arg == requested_text.as_ref() || arg == declared_text.as_ref() {
                        *arg = staged.to_string_lossy().into_owned();
                        replaced = true;
                    }
                }
                if !replaced {
                    return StagedPlanOutcome::Unsupported(StagedPlanReason::OutputNotInArguments);
                }
                outputs.push((requested, staged));
            }
            StagedPlanOutcome::Enabled(Self {
                outputs,
                rewritten_args,
                root: root.clone(),
            })
        })();
        if !matches!(result, StagedPlanOutcome::Enabled(_)) {
            let _ = std::fs::remove_dir_all(&root);
        }
        result
    }

    fn staged_paths(&self) -> Vec<NormalizedPath> {
        self.outputs
            .iter()
            .map(|(_, staged)| staged.clone())
            .collect()
    }

    fn materialize(&self) -> std::io::Result<StagedMaterializationStats> {
        let mut observed = StagedMaterializationStats::default();
        #[cfg(test)]
        let mut fault_index = 0;
        for (requested, staged) in &self.outputs {
            #[cfg(test)]
            {
                let index = fault_index;
                fault_index += 1;
                inject_staged_fault(
                    requested.as_path(),
                    StagedFaultPoint::MaterializeOutput(index),
                )
                .map_err(|error| materialization_error(error, observed))?;
            }
            if let Some(parent) = requested.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|error| materialization_error(error, observed))?;
            }
            let output = crate::daemon::server::persist::materialize_independent_with_stats(
                staged.as_path(),
                requested.as_path(),
            )
            .map_err(|error| materialization_error(error, observed))?;
            observed.add(output);
        }
        self.cleanup()
            .map_err(|error| materialization_error(error, observed))?;
        Ok(observed)
    }

    fn cleanup(&self) -> std::io::Result<()> {
        std::fs::remove_dir_all(&self.root).or_else(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                Ok(())
            } else {
                Err(error)
            }
        })
    }
}

impl Drop for ExecStagedPlan {
    fn drop(&mut self) {
        let _ = self.cleanup();
    }
}

fn exec_staging_enabled() -> bool {
    std::env::var(crate::daemon::server::persist::STAGED_ARTIFACTS_ENV)
        .ok()
        .is_some_and(|value| {
            matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "all" | "1" | "true" | "yes" | "on" | "exec"
            )
        })
}

/// Domain separation tag for generic-exec cache keys. v2 covers Path A +
/// Path B + filtered args; v1 callers (PROTOCOL_VERSION 10) are no longer
/// wire-compatible since the protocol version itself shifted to 11.
const EXEC_KEY_DOMAIN: &[u8] = b"zccache-exec-key-v2";

/// Cap on per-stream captured bytes. Exceeding this skips caching for the
/// run and emits a diagnostic to stderr; the tool's output still flows
/// through unchanged. The cap matches the IPC frame budget (`MAX_MESSAGE_SIZE`
/// in `protocol::mod`).
const EXEC_STREAM_CAP_BYTES: usize = 16 * 1024 * 1024;

/// Env override (milliseconds) for the [`acquire_in_flight`] coalesce wait
/// budget. See [`in_flight_wait_budget`].
const EXEC_COALESCE_WAIT_ENV: &str = "ZCCACHE_EXEC_COALESCE_WAIT_MS";

/// Default coalesce wait budget (ms). Generous because exec tools can
/// legitimately run a while; a wedged owner still can't hang waiters past this
/// (issue #971 mode 3 — they fall back to running their own copy).
const EXEC_COALESCE_WAIT_DEFAULT_MS: u64 = 60_000;

#[allow(clippy::too_many_arguments)]
pub(super) async fn handle_generic_tool_exec(
    state: &Arc<SharedState>,
    tool: &Path,
    args: &[String],
    cwd: &Path,
    env: Vec<(String, String)>,
    input_files: &[NormalizedPath],
    input_extra: Arc<Vec<u8>>,
    output_streams: ExecOutputStreams,
    output_files: &[NormalizedPath],
    tool_hash_override: Option<[u8; 32]>,
    cache_policy: ExecCachePolicy,
    cwd_in_key: bool,
    include_scan_files: &[NormalizedPath],
    include_dirs: &[NormalizedPath],
    system_include_dirs: &[NormalizedPath],
    iquote_dirs: &[NormalizedPath],
    depfile: Option<&Path>,
    non_deterministic: bool,
    key_args_filter: &[String],
) -> Response {
    // 1. Filtered args for the cache key (the tool always sees the raw args).
    let key_args = match apply_key_args_filter(args, key_args_filter) {
        Ok(v) => v,
        Err(e) => {
            return Response::Error {
                message: format!("invalid key-args-filter regex: {e}"),
            };
        }
    };

    // 2. Tool identity hash — caller override wins, otherwise blake3 the
    //    binary (cached by (path, mtime, size) via the compiler-hash cache).
    let tool_id_hash = match tool_hash_override {
        Some(bytes) => ContentHash::from_bytes(bytes),
        None => match hash_file_via_cache(state, tool) {
            Some(h) => h,
            None => {
                return Response::Error {
                    message: format!("cannot hash tool {}", tool.display()),
                };
            }
        },
    };

    // 3. Hash each declared input file via the metadata cache (TwoLayer
    //    mtime+size → blake3 fast path). Paths get absolutized against cwd
    //    so a relative declaration is interpreted the same way it would be
    //    from the tool's perspective.
    let mut input_pairs: Vec<(String, ContentHash)> = Vec::with_capacity(input_files.len());
    for input in input_files {
        let abs: PathBuf = absolutize(input.as_path(), cwd);
        let hash = match hash_file_via_cache(state, &abs) {
            Some(h) => h,
            None => {
                return Response::Error {
                    message: format!("cannot hash input file {}", abs.display()),
                };
            }
        };
        input_pairs.push((normalize_for_key(&abs), hash));
    }
    input_pairs.sort_by(|a, b| a.0.cmp(&b.0));

    // 4. Path A: scan declared seed files for transitive includes, hash each
    //    resolved header. Skipped when no seed files are given.
    let scan_pairs = match run_include_scan(
        state,
        cwd,
        include_scan_files,
        include_dirs,
        system_include_dirs,
        iquote_dirs,
    ) {
        Ok(v) => v,
        Err(e) => return Response::Error { message: e },
    };

    // 5. Compose the primary cache key — everything we know BEFORE we run
    //    the tool. Path B's depfile-derived dep set is mixed in later
    //    (after the tool runs) or — on a warm invocation — read from the
    //    `<primary>.deps` sidecar before lookup.
    let primary_key = compose_primary_key(
        &tool_id_hash,
        &key_args,
        &env,
        cwd,
        cwd_in_key,
        &input_pairs,
        &scan_pairs,
        output_files,
        &input_extra,
    );
    let primary_hex = primary_key.to_hex();

    // 6. Path B: if the caller declared a depfile, try to load a stored dep
    //    set keyed by the primary. On hit we have the *full* key; on miss
    //    we use the primary as the full key for now (first invocation).
    let depfile_path = depfile.map(|p| absolutize(p, cwd));
    let stored_dep_pairs = if depfile_path.is_some() {
        load_depfile_sidecar(state, &primary_hex, cwd)
    } else {
        None
    };
    let full_key = match &stored_dep_pairs {
        Some(deps) => compose_full_key(&primary_key, deps),
        None => primary_key,
    };
    let full_hex = full_key.to_hex();

    let bypass = non_deterministic || matches!(cache_policy, ExecCachePolicy::Bypass);
    let lookup_allowed = !bypass
        && matches!(
            cache_policy,
            ExecCachePolicy::Normal | ExecCachePolicy::ReadOnly
        );
    let store_allowed = !bypass && matches!(cache_policy, ExecCachePolicy::Normal);

    // 7. Cache lookup (Normal + ReadOnly) under the full key.
    if lookup_allowed {
        if let Some(resp) =
            try_exec_cache_hit(state, &full_hex, cwd, output_files, output_streams).await
        {
            return resp;
        }
    }

    // 8. Coalesce concurrent callers with the same full key: insert into
    //    `in_flight_exec`. If someone else is already there, wait for them
    //    and retry the lookup — exactly one tool spawn services the herd.
    let coalesce_guard = if lookup_allowed {
        Some(acquire_in_flight(state, &full_hex).await)
    } else {
        None
    };
    if let Some(InFlight::WokenByPeer) = coalesce_guard.as_ref().map(|g| g.outcome()) {
        if let Some(resp) =
            try_exec_cache_hit(state, &full_hex, cwd, output_files, output_streams).await
        {
            return resp;
        }
        // Fell through: peer ran but didn't store, or store failed. Run it
        // ourselves below; we still hold the guard so further peers wait
        // for us, not the original runner.
    }

    // 9. Cache miss — stage declared outputs only when the tool's argv gives
    // us an unambiguous exact-token rewrite for every output. Opaque output
    // syntax remains on the legacy path before spawn.
    use crate::daemon::staged_stats::{StagedCounter, StagedTiming};
    let planning_started = std::time::Instant::now();
    state.profiler.staged.count(StagedCounter::PlanAttempted);
    let staged_plan_result = ExecStagedPlan::build(state.staging.path(), args, output_files, cwd);
    state.profiler.staged.timing(
        StagedTiming::Planning,
        planning_started.elapsed().as_nanos() as u64,
    );
    let staged_plan = match staged_plan_result {
        StagedPlanOutcome::Enabled(plan) => {
            state.profiler.staged.count(StagedCounter::PlanEnabled);
            Some(plan)
        }
        StagedPlanOutcome::Unsupported(reason) => {
            state.profiler.staged.count(StagedCounter::PlanUnsupported);
            state.profiler.staged.failure(reason.failure());
            None
        }
        StagedPlanOutcome::Error(error) => {
            state.profiler.staged.count(StagedCounter::PlanError);
            state.profiler.staged.failure(error.reason.failure());
            tracing::warn!(
                reason = error.reason.id(),
                error = %error.source,
                "exact-exec staging plan failed; using legacy path"
            );
            None
        }
    };
    let compiler_args = staged_plan
        .as_ref()
        .map_or(args, |plan| plan.rewritten_args.as_slice());
    let execution_outputs = staged_plan
        .as_ref()
        .map_or_else(|| output_files.to_vec(), ExecStagedPlan::staged_paths);
    for path in &execution_outputs {
        let path = path.as_path();
        if let Err(error) = break_output_hardlink_before_compile(path) {
            tracing::warn!(
                event = "exec_output_detach_failed",
                path = %path.display(),
                error = %error,
                "failed to detach generic-tool output before execution"
            );
            return Response::Error {
                message: format!("failed to detach output {}: {error}", path.display()),
            };
        }
    }
    let compiler_started = std::time::Instant::now();
    let output = match spawn_tool(tool, compiler_args, cwd, &env).await {
        Ok(o) => o,
        Err(e) => {
            return Response::Error {
                message: format!("failed to run {}: {e}", tool.display()),
            };
        }
    };
    if staged_plan.is_some() {
        state.profiler.staged.count(StagedCounter::CompilerStaged);
        state.profiler.staged.timing(
            StagedTiming::Compiler,
            compiler_started.elapsed().as_nanos() as u64,
        );
    }

    let exit_code = output.status.code().unwrap_or(1);
    let stdout = Arc::new(if output_streams.stdout {
        output.stdout
    } else {
        Vec::new()
    });
    let stderr = Arc::new(if output_streams.stderr {
        output.stderr
    } else {
        Vec::new()
    });

    if staged_plan.is_some() && exit_code != 0 {
        return Response::GenericToolExecResult {
            exit_code,
            stdout,
            stderr,
            output_files: Vec::new(),
            cached: false,
            cache_key_hex: full_hex,
        };
    }

    // 10. Snapshot declared output files. A missing declared file marks the
    //     run uncacheable — the next request can't replay something we
    //     never captured.
    let (captured_outputs, cache_outputs, all_captured) =
        snapshot_output_files(output_files, &execution_outputs, cwd);

    // 11. Path B: after a successful run, parse the depfile the tool wrote,
    //     persist the dep set under the primary key, and re-compose the
    //     full key. Failure modes:
    //     - depfile missing → skip storing (next call is a clean first run)
    //     - parse error    → skip storing
    //     - listed file missing → skip storing
    let mut final_full_hex = full_hex.clone();
    let mut depfile_ok = true;
    if let Some(df_path) = depfile_path.as_deref() {
        if exit_code != 0 {
            // Tool failed → don't trust the depfile, don't store anything
            // depfile-derived. The non-zero exit code is still returned but
            // the caching decision below will skip the store anyway when
            // we mark depfile_ok = false.
            depfile_ok = false;
        } else {
            match harvest_depfile(state, df_path, cwd, &input_pairs) {
                Ok(dep_pairs) => {
                    persist_depfile_sidecar(state, &primary_hex, &dep_pairs);
                    let new_full = compose_full_key(&primary_key, &dep_pairs);
                    final_full_hex = new_full.to_hex();
                }
                Err(e) => {
                    tracing::warn!(
                        depfile = %df_path.display(),
                        err = %e,
                        "depfile parse failed; skipping cache store for this run"
                    );
                    depfile_ok = false;
                }
            }
        }
    }

    // 12. Skip caching when the cap was blown — keeps the cache from
    //     carrying multi-hundred-MB stdout that won't fit in an IPC frame.
    let too_large = stdout.len() > EXEC_STREAM_CAP_BYTES || stderr.len() > EXEC_STREAM_CAP_BYTES;

    let cacheable_exit = true; // exit codes are part of the cached payload — even non-zero
    if store_allowed && all_captured && !too_large && depfile_ok && cacheable_exit {
        let artifact = ArtifactData {
            outputs: cache_outputs,
            stdout: Arc::clone(&stdout),
            stderr: Arc::clone(&stderr),
            exit_code,
        };
        store_exec_artifact(state, final_full_hex.clone(), artifact).await;
    } else if too_large {
        tracing::warn!(
            stdout_len = stdout.len(),
            stderr_len = stderr.len(),
            cap = EXEC_STREAM_CAP_BYTES,
            "exec output exceeded cache cap; not storing this run"
        );
    } else if non_deterministic {
        tracing::debug!(
            key = %final_full_hex,
            "exec marked non-deterministic; not storing"
        );
    }

    if let Some(plan) = staged_plan.as_ref() {
        if !all_captured {
            return Response::Error {
                message: "successful generic tool omitted a staged output".to_string(),
            };
        }
        use crate::daemon::staged_stats::{StagedBytes, StagedCounter, StagedFailure};
        let materialize_started = std::time::Instant::now();
        match plan.materialize() {
            Ok(observed) => {
                state
                    .profiler
                    .staged
                    .add_count(StagedCounter::MaterializeReflink, observed.reflink_count);
                state
                    .profiler
                    .staged
                    .add_count(StagedCounter::MaterializeCopy, observed.copy_count);
                state
                    .profiler
                    .staged
                    .bytes(StagedBytes::Materialization, observed.copy_bytes);
                state.profiler.staged.timing(
                    StagedTiming::MissMaterialization,
                    materialize_started.elapsed().as_nanos() as u64,
                );
            }
            Err(error) => {
                let elapsed_ns = materialize_started.elapsed().as_nanos() as u64;
                let progress = materialization_error_progress(&error);
                state
                    .profiler
                    .staged
                    .add_count(StagedCounter::MaterializeReflink, progress.reflink_count);
                state
                    .profiler
                    .staged
                    .add_count(StagedCounter::MaterializeCopy, progress.copy_count);
                state
                    .profiler
                    .staged
                    .bytes(StagedBytes::Materialization, progress.copy_bytes);
                state
                    .profiler
                    .staged
                    .count(StagedCounter::MaterializeFailure);
                state
                    .profiler
                    .staged
                    .failure(StagedFailure::RequestedMaterialization);
                state
                    .profiler
                    .staged
                    .timing(StagedTiming::MissMaterialization, elapsed_ns);
                crate::core::lifecycle::write_event(
                    "staged_materialization_failed",
                    serde_json::json!({
                        "reason": "requested_materialization",
                        "output_count": plan.outputs.len(),
                        "copied_bytes": progress.copy_bytes,
                        "elapsed_ns": elapsed_ns,
                    }),
                );
                return Response::Error {
                    message: format!("failed to materialize generic tool outputs: {error}"),
                };
            }
        }
    }

    drop(coalesce_guard); // Releasing the guard wakes any waiters.

    Response::GenericToolExecResult {
        exit_code,
        stdout,
        stderr,
        output_files: captured_outputs,
        cached: false,
        cache_key_hex: final_full_hex,
    }
}

// ─── Key composition ─────────────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
#[path = "handle_exec_key.rs"]
mod key;
use key::*;
enum InFlight {
    /// We acquired the slot ourselves (no peer was running).
    Owner,
    /// We waited on a peer's `Notify`; caller should re-attempt the cache
    /// lookup before spawning the tool.
    WokenByPeer,
}

struct InFlightGuardExec {
    state: Arc<SharedState>,
    key: String,
    outcome: InFlight,
}

impl InFlightGuardExec {
    fn outcome(&self) -> InFlight {
        match self.outcome {
            InFlight::Owner => InFlight::Owner,
            InFlight::WokenByPeer => InFlight::WokenByPeer,
        }
    }
}

impl Drop for InFlightGuardExec {
    fn drop(&mut self) {
        if matches!(self.outcome, InFlight::Owner) {
            // Remove the slot, then wake any waiters.
            if let Some((_, notify)) = self.state.in_flight_exec.remove(&self.key) {
                notify.notify_waiters();
            }
        }
    }
}

async fn acquire_in_flight(state: &Arc<SharedState>, key_hex: &str) -> InFlightGuardExec {
    let key = key_hex.to_string();
    let notify_arc = {
        match state.in_flight_exec.entry(key.clone()) {
            Entry::Occupied(o) => Arc::clone(o.get()),
            Entry::Vacant(v) => {
                v.insert(Arc::new(Notify::new()));
                return InFlightGuardExec {
                    state: Arc::clone(state),
                    key,
                    outcome: InFlight::Owner,
                };
            }
        }
    };

    let budget = in_flight_wait_budget();
    if let CoalesceOutcome::TimedOut =
        coalesce_wait(&state.in_flight_exec, &key, notify_arc, budget).await
    {
        // Loud + durable: a wedged owner made the herd fall back to running
        // their own copies. Logged here (not in `coalesce_wait`) so the pure
        // wait helper stays unit-testable without touching the lifecycle log.
        tracing::warn!(
            event = "in_flight_exec_wait_timeout",
            key = %key,
            budget_ms = budget.as_millis() as u64,
            "waited past the coalesce budget for the in-flight exec owner of this key; \
             the owner may be wedged — running our own copy instead of hanging (issue #971)"
        );
        crate::core::lifecycle::write_event(
            "in_flight_exec_wait_timeout",
            serde_json::json!({
                "key": key,
                "budget_ms": budget.as_millis() as u64,
                "reason": "in-flight exec owner did not finish within the coalesce budget; running own copy",
            }),
        );
    }
    InFlightGuardExec {
        state: Arc::clone(state),
        key,
        outcome: InFlight::WokenByPeer,
    }
}

/// Outcome of [`coalesce_wait`].
#[derive(Debug, PartialEq, Eq)]
enum CoalesceOutcome {
    /// The owner's `Notify` fired while we were parked.
    Woken,
    /// The slot was already gone or replaced by a new owner when we re-checked
    /// after registering — no wait was needed.
    SlotResolved,
    /// We waited the full budget without a wakeup (owner likely wedged).
    TimedOut,
}

/// Wait for the in-flight owner of `key` to finish, bounded by `budget`.
///
/// Closes the two `tokio::sync::Notify` single-flight hazards behind #971:
/// - **Lost wakeup (mode 1):** `Notified` only arms a waiter once polled, so we
///   `enable()` the future BEFORE re-checking the map. An owner that runs
///   `remove()` + `notify_waiters()` between the caller cloning the `Arc` and
///   this point would otherwise have its wakeup dropped (`notify_waiters`
///   stores no permit), stranding the waiter forever.
/// - **Slot replaced (mode 2):** after `enable()` we re-check by identity
///   (`Arc::ptr_eq`); if the slot is gone or a *new* owner installed a fresh
///   `Notify`, the `notify_arc` we hold will never fire again, so we return
///   `SlotResolved` instead of parking on a dead notify.
///
/// Kept free of `SharedState` and logging so it is deterministically testable
/// against a bare `DashMap`.
async fn coalesce_wait(
    in_flight: &dashmap::DashMap<String, Arc<Notify>>,
    key: &str,
    notify_arc: Arc<Notify>,
    budget: std::time::Duration,
) -> CoalesceOutcome {
    let notified = notify_arc.notified();
    tokio::pin!(notified);
    notified.as_mut().enable();

    let still_ours = in_flight
        .get(key)
        .is_some_and(|cur| Arc::ptr_eq(cur.value(), &notify_arc));
    if !still_ours {
        return CoalesceOutcome::SlotResolved;
    }

    tokio::select! {
        () = notified.as_mut() => CoalesceOutcome::Woken,
        () = tokio::time::sleep(budget) => CoalesceOutcome::TimedOut,
    }
}

/// Coalesce wait budget for [`acquire_in_flight`]. A peer already running the
/// same keyed tool is usually the fast path, but a wedged owner must not hang
/// its waiters forever — on expiry the waiter runs its own copy. Generous by
/// default (exec tools can legitimately run a while); override with
/// `ZCCACHE_EXEC_COALESCE_WAIT_MS`.
fn in_flight_wait_budget() -> std::time::Duration {
    std::env::var(EXEC_COALESCE_WAIT_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .map(std::time::Duration::from_millis)
        .unwrap_or(std::time::Duration::from_millis(
            EXEC_COALESCE_WAIT_DEFAULT_MS,
        ))
}

// ─── Spawn + capture ────────────────────────────────────────────────────

async fn spawn_tool(
    tool: &Path,
    args: &[String],
    cwd: &Path,
    env: &[(String, String)],
) -> std::io::Result<std::process::Output> {
    let mut cmd = tokio::process::Command::new(tool);
    cmd.args(args).current_dir(cwd);
    // Clear env and apply only the declared subset so the run is reproducible
    // across hosts that may have unrelated env differences. PATH is
    // intentionally NOT auto-injected — callers that need it must declare
    // `--input-env PATH` so it participates in the key.
    cmd.env_clear();
    for (k, v) in env {
        cmd.env(k, v);
    }
    // Route through the async priority helper so the tool wait flows through
    // the orphan-pipe watchdog (issue #962): a tool that leaves a pipe-holding
    // grandchild can no longer wedge the exec owner forever — which in turn
    // stops that wedge from stranding every coalesced waiter on this key
    // (issue #971 mode 3, the wedged-owner path).
    crate::daemon::process::tokio_command_output_with_priority(&mut cmd, CompilePriority::Normal)
        .await
}

fn snapshot_output_files(
    output_files: &[NormalizedPath],
    actual_paths: &[NormalizedPath],
    cwd: &Path,
) -> (Vec<ArtifactOutput>, Vec<ArtifactOutput>, bool) {
    let mut captured_outputs: Vec<ArtifactOutput> = Vec::with_capacity(output_files.len());
    let mut cache_outputs: Vec<ArtifactOutput> = Vec::with_capacity(output_files.len());
    let mut all_captured = true;
    for (declared, actual) in output_files.iter().zip(actual_paths) {
        let abs: PathBuf = absolutize(actual.as_path(), cwd);
        match std::fs::read(&abs) {
            Ok(bytes) => {
                let payload = ArtifactPayload::Bytes(Arc::new(bytes));
                captured_outputs.push(ArtifactOutput {
                    name: declared.to_string_lossy().into_owned(),
                    payload: payload.clone(),
                });
                cache_outputs.push(ArtifactOutput {
                    name: declared.to_string_lossy().into_owned(),
                    payload,
                });
            }
            Err(e) => {
                all_captured = false;
                tracing::warn!(
                    path = %abs.display(),
                    err = %e,
                    "declared output file missing after exec; not caching this output"
                );
            }
        }
    }
    (captured_outputs, cache_outputs, all_captured)
}

// ─── Replay (cache hit) ─────────────────────────────────────────────────

async fn try_exec_cache_hit(
    state: &Arc<SharedState>,
    key_hex: &str,
    cwd: &Path,
    output_files: &[NormalizedPath],
    output_streams: ExecOutputStreams,
) -> Option<Response> {
    let mut entry = lookup_artifact_with_disk_fallback(state, key_hex)?;
    entry.last_used = std::time::Instant::now();

    let exit_code = entry.meta.exit_code;
    let stdout_full = entry.stdout.clone();
    let stderr_full = entry.stderr.clone();
    let names = Arc::clone(&entry.meta.output_names);

    let payloads_loaded = ensure_payloads(&mut entry, &state.artifact_dir, key_hex).is_some();
    if !payloads_loaded {
        return None;
    }
    let payloads = Arc::clone(entry.payloads.as_ref()?);
    drop(entry);

    let mut paired: Vec<(NormalizedPath, &CachedPayload)> = Vec::with_capacity(output_files.len());
    for declared in output_files {
        let declared_name = declared.to_string_lossy().into_owned();
        let idx = names.iter().position(|n| n == &declared_name)?;
        let payload = payloads.get(idx)?;
        let abs: NormalizedPath = if declared.as_path().is_absolute() {
            declared.clone()
        } else {
            cwd.join(declared.as_path()).into()
        };
        paired.push((abs, payload));
    }

    let targets: Vec<(NormalizedPath, NormalizedPath)> = paired
        .iter()
        .enumerate()
        .map(|(i, (abs, _))| {
            let cache_file = state.artifact_dir.join(format!("{key_hex}_{i}"));
            (abs.clone(), cache_file)
        })
        .collect();
    let payloads_for_write: Vec<CachedPayload> =
        paired.into_iter().map(|(_, p)| p.clone()).collect();
    if !write_payloads_par(&targets, &payloads_for_write) {
        return None;
    }

    let response_stdout = if output_streams.stdout {
        stdout_full
    } else {
        Arc::new(Vec::new())
    };
    let response_stderr = if output_streams.stderr {
        stderr_full
    } else {
        Arc::new(Vec::new())
    };

    let mut response_outputs: Vec<ArtifactOutput> = Vec::with_capacity(targets.len());
    for ((abs, _), payload) in targets.iter().zip(payloads_for_write.iter()) {
        let bytes: Arc<Vec<u8>> = match payload {
            CachedPayload::Bytes(b) => Arc::clone(b),
            CachedPayload::File(p) => match std::fs::read(p.as_path()) {
                Ok(b) => Arc::new(b),
                Err(_) => Arc::new(Vec::new()),
            },
        };
        response_outputs.push(ArtifactOutput {
            name: abs.to_string_lossy().into_owned(),
            payload: ArtifactPayload::Bytes(bytes),
        });
    }

    Some(Response::GenericToolExecResult {
        exit_code,
        stdout: response_stdout,
        stderr: response_stderr,
        output_files: response_outputs,
        cached: true,
        cache_key_hex: key_hex.to_string(),
    })
}

// ─── Store ──────────────────────────────────────────────────────────────

async fn store_exec_artifact(state: &Arc<SharedState>, key_hex: String, artifact: ArtifactData) {
    let cached = CachedArtifact::from_artifact_data(&artifact);
    {
        let artifact_dir = state.artifact_dir.clone();
        let key_for_persist = key_hex.clone();
        let payloads: Vec<Arc<Vec<u8>>> = artifact
            .outputs
            .iter()
            .filter_map(|o| o.payload.as_bytes().cloned())
            .collect();
        let persist_meta = cached.meta.clone();
        if payloads.is_empty() {
            // Stdout/stderr-only artifacts have no payload files to persist,
            // so expose their index entry to the final shutdown flush now.
            state.artifact_store.insert(&key_hex, &persist_meta);
        }
        let state_ref = Arc::clone(state);
        let sem = Arc::clone(&state.persist_semaphore);
        tokio::spawn(async move {
            #[expect(
                clippy::expect_used,
                reason = "persist_semaphore is owned by ServerState for the daemon's lifetime; AcquireError here would be a logic bug (semaphore explicitly closed), not a runtime condition"
            )]
            let _permit = sem
                .acquire()
                .await
                .expect("persist_semaphore is owned by ServerState and never closed");
            let written = tokio::task::spawn_blocking(move || {
                let _ = persist_artifact_payloads(&artifact_dir, &key_for_persist, &payloads);
                (key_for_persist, persist_meta)
            })
            .await;
            if let Ok((kh, meta)) = written {
                let _ = state_ref
                    .index_writer_tx
                    .send(IndexWriterCommand::Insert(kh, meta));
            }
        });
    }
    state.artifacts.insert(key_hex, cached);
}

// ─── Path normalization ─────────────────────────────────────────────────

fn absolutize(p: &Path, cwd: &Path) -> PathBuf {
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        cwd.join(p)
    }
}

fn absolutize_norm(p: &NormalizedPath, cwd: &Path) -> NormalizedPath {
    let ap = absolutize(p.as_path(), cwd);
    NormalizedPath::from(ap.as_path())
}

/// Stable forward-slash-only path representation for use in cache keys.
/// Mirrors what other handlers do via `core::path::normalize_for_key` — kept
/// local so this module has no extra deps to wire.
fn normalize_for_key(path: &Path) -> String {
    let s = path.to_string_lossy().into_owned();
    s.replace('\\', "/")
}

#[cfg(test)]
#[path = "handle_exec_coalesce_tests.rs"]
mod coalesce_tests;

#[cfg(test)]
#[path = "handle_exec_tests.rs"]
mod tests;
