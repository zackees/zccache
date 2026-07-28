//! Embedded (in-process) `EmbeddedDaemon`: construction, background cache
//! loads, the compile entrypoint, and flush/shutdown. Split out of
//! `lifecycle.rs` to keep each server file under the 1k-LOC budget.
//!
//! The bind-first / load-in-background startup ordering these methods rely on
//! is the same #640/#784 invariant the daemon binary uses; see `loaders`.

use super::*;

/// zccache#940 — monotonic per-process compile counter for the inner
/// diagnostic trace. Resets across process restarts by design (the
/// trace file is process-scoped). Hosts that need durable ids should
/// cross-correlate by `ts_ns` against their own audit log.
static INNER_COMPILE_SEQ: AtomicU64 = AtomicU64::new(0);

fn next_inner_compile_id() -> String {
    let n = INNER_COMPILE_SEQ.fetch_add(1, Ordering::Relaxed);
    format!("z{n:08x}")
}

/// Non-shutdown flushes may report an incomplete publication barrier instead
/// of waiting forever for a stuck publisher. Graceful shutdown is different:
/// it owns the process exit contract and therefore waits for every mutating
/// worker to become quiescent before the final index checkpoint.
const EMBEDDED_PUBLICATION_BARRIER_TIMEOUT: std::time::Duration =
    std::time::Duration::from_secs(30);

impl EmbeddedDaemon {
    #[cfg(test)]
    pub(crate) async fn start(
        endpoint: String,
        cache_dir: crate::core::NormalizedPath,
        runtime_handle: Option<tokio::runtime::Handle>,
        maintenance_policy: MaintenancePolicy,
    ) -> Result<Self, crate::ipc::IpcError> {
        Self::start_with_maintenance(
            endpoint,
            cache_dir,
            runtime_handle,
            maintenance_policy,
            true,
        )
        .await
    }

    pub(crate) async fn start_with_maintenance(
        endpoint: String,
        cache_dir: crate::core::NormalizedPath,
        runtime_handle: Option<tokio::runtime::Handle>,
        maintenance_policy: MaintenancePolicy,
        automatic_maintenance: bool,
    ) -> Result<Self, crate::ipc::IpcError> {
        let backend_identity = crate::ipc::current_backend_identity(&endpoint)
            .map_err(|err| super::lifecycle::daemon_identity_error(&endpoint, &err))?;
        let (state, index_writer_rx) = new_shared_state(&endpoint, &cache_dir, backend_identity)
            .map_err(|error| super::lifecycle::cache_root_error(&cache_dir, &error))?;
        // Arm the startup depgraph-load gate as early as possible — before
        // this state can serve any compile. The shared `dep_graph_load_complete`
        // flag inits `true` ("assume loaded"); the standalone daemon flips it
        // to `false` via `mark_dep_graph_load_pending()` before offloading the
        // `depgraph.bin` load, but the embedded service never did. So
        // `wait_for_startup_depgraph_load` in the compile pipeline was a no-op
        // and the first warm compiles after a `soldr load` raced the empty
        // default graph, taking a `CacheVerdict::Cold` (miss) until the
        // background load swapped the restored graph in. The depgraph load in
        // `start_background_tasks` flips this back to `true` + notifies waiters.
        state
            .dep_graph_load_complete
            .store(false, std::sync::atomic::Ordering::Release);

        let mut daemon = Self {
            state,
            maintenance_policy,
            index_writer_rx: Some(index_writer_rx),
            index_writer_handle: Mutex::new(None),
            maintenance_handle: Mutex::new(None),
        };
        daemon
            .start_background_tasks(runtime_handle, automatic_maintenance)
            .await;
        Ok(daemon)
    }

    async fn start_background_tasks(
        &mut self,
        runtime_handle: Option<tokio::runtime::Handle>,
        automatic_maintenance: bool,
    ) {
        if let Some(rx) = self.index_writer_rx.take() {
            let store = Arc::clone(&self.state.artifact_store);
            let shutdown = Arc::clone(&self.state.index_writer_shutdown);
            let task = run_index_writer(rx, store, shutdown);
            // zccache#922: when the embedded host supplied a Tokio Handle,
            // route the persistent index-writer spawn through it. Otherwise
            // fall back to the ambient runtime (the calling runtime is the
            // only one available when `runtime_handle.is_none()`, and the
            // ambient resolves to it).
            let handle = match &runtime_handle {
                Some(h) => h.spawn(task),
                None => tokio::spawn(task),
            };
            *self.index_writer_handle.lock().await = Some(handle);
        }

        let state = Arc::clone(&self.state);
        let artifact_load = tokio::task::spawn_blocking(move || {
            if let Err(error) = state.staging.cleanup_abandoned() {
                tracing::debug!(%error, "abandoned private staging cleanup skipped");
            }
            if let Err(e) = state.artifact_store.load_from_disk() {
                tracing::warn!("embedded artifact index load failed, continuing empty: {e}");
            }
            let entries = state.artifact_store.load_all();
            let count = entries.len();
            for (key, meta) in entries {
                state
                    .artifacts
                    .insert(key, CachedArtifact::from_index(meta));
            }
            state.artifacts_loaded.store(true, Ordering::Release);
            state.artifact_store_loaded.store(true, Ordering::Release);
            count
        })
        .await
        .unwrap_or(0);
        if artifact_load > 0 {
            tracing::info!(loaded = artifact_load, "embedded artifact index restored");
        }

        let metadata_state = Arc::clone(&self.state);
        let metadata_path = self.state.metadata_path.clone();
        let _ = tokio::task::spawn_blocking(move || {
            match crate::fscache::MetadataCache::load_from_disk(metadata_path.as_path()) {
                Ok(loaded) => metadata_state.cache_system.metadata().merge_from(loaded),
                Err(e) => tracing::warn!(
                    path = %metadata_path.display(),
                    "failed to load embedded metadata cache, starting empty: {e}"
                ),
            }
            metadata_state
                .metadata_cache_loaded
                .store(true, Ordering::Release);
        })
        .await;

        let compiler_state = Arc::clone(&self.state);
        let compiler_hash_cache_path = self.state.compiler_hash_cache_path.clone();
        let _ = tokio::task::spawn_blocking(move || {
            match CompilerHashCache::load_from_disk(compiler_hash_cache_path.as_path()) {
                Ok(loaded) => compiler_state.compiler_hash_cache.merge_from(loaded),
                Err(e) => tracing::warn!(
                    path = %compiler_hash_cache_path.display(),
                    "failed to load embedded compiler hash cache, starting empty: {e}"
                ),
            }
            compiler_state
                .compiler_hash_cache_loaded
                .store(true, Ordering::Release);
        })
        .await;

        let includes_state = Arc::clone(&self.state);
        let system_includes_cache_path = self.state.system_includes_cache_path.clone();
        let _ = tokio::task::spawn_blocking(move || {
            match crate::depgraph::SystemIncludeCache::load_from_disk(
                system_includes_cache_path.as_path(),
            ) {
                Ok(loaded) => {
                    let mut live = includes_state.system_includes.blocking_lock();
                    live.merge_from(loaded);
                }
                Err(e) => tracing::warn!(
                    path = %system_includes_cache_path.display(),
                    "failed to load embedded system include cache, starting empty: {e}"
                ),
            }
            includes_state
                .system_includes_loaded
                .store(true, Ordering::Release);
        })
        .await;

        let depgraph_path = embedded_depgraph_file_path(&self.state);
        let state = Arc::clone(&self.state);
        let _ = tokio::task::spawn_blocking(move || {
            let outcome = crate::depgraph::classify_load(depgraph_path.as_path());
            let warning = outcome.warning(depgraph_path.as_path());
            if let Some(graph) = outcome.into_graph() {
                state.dep_graph.store(Arc::new(graph));
                state.dep_graph_persisted.store(true, Ordering::Release);
            }
            if let Some(warning) = warning {
                let mut guard = state
                    .depgraph_load_warning
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                *guard = Some(warning);
            }
            state.dep_graph_load_complete.store(true, Ordering::Release);
            state.dep_graph_load_notify.notify_waiters();
        })
        .await;

        if automatic_maintenance {
            let maintenance_handle = spawn_disk_maintenance(
                Arc::clone(&self.state),
                self.maintenance_policy,
                runtime_handle.as_ref(),
            );
            *self.maintenance_handle.lock().await = Some(maintenance_handle);
        }
    }

    pub(crate) async fn compile(
        &self,
        request: EmbeddedCompileRequest,
    ) -> Result<EmbeddedCompileResult, String> {
        self.state
            .last_activity
            .store(now_secs(), Ordering::Relaxed);
        // zccache#940: per-compile id for the diagnostic trace. The
        // embedded daemon does not yet surface an audit id through
        // EmbeddedCompileRequest, so we generate a monotonic per-process
        // counter here. Hosts that already track their own per-compile
        // id (soldr's `c<N>` scheme) get a parallel namespace; the two
        // sides can be cross-correlated by timestamp.
        let compile_id = next_inner_compile_id();
        let total = std::time::Instant::now();
        // soldr#1286: capture journal metadata BEFORE the handler consumes
        // the request so embedded compiles land in compile_journal.jsonl
        // exactly like daemon-IPC compiles do (connection.rs journal block).
        // Without this the embedded backend — the only compile path for
        // soldr since zccache became an embedded service — was invisible
        // to hit/miss telemetry: `zccache analyze`, dashboards, and
        // post-mortem scripts saw zero rustc records.
        let journal_ctx = JournalContext::new(
            request.compiler.to_string_lossy().into_owned(),
            request.args.clone(),
            request.cwd.to_string_lossy().into_owned(),
            request.env.clone(),
            None,
        );
        // zccache#940: open the inner-trace scope so the deep pipeline seams
        // (input_hash, cache_lookup, cache_load, rustc_spawn/wait, output_read,
        // cache_store) attribute their sub-phase records to this compile_id.
        // No-op unless ZCCACHE_INNER_TRACE is set; the IPC wrapper path does
        // not open a scope, so only embedded compiles emit sub-phase records.
        let (mut response, attributed_miss_reason, context_key) =
            capture_miss_reason(Box::pin(super::inner_trace::scope(
                compile_id.clone(),
                handle_compile_ephemeral(
                    &self.state,
                    std::process::id(),
                    &request.cwd,
                    &request.compiler,
                    &request.args,
                    &request.cwd,
                    request.env,
                    request.stdin,
                ),
            )))
            .await;
        crate::compile_trace::record(
            "embedded_daemon_compile",
            total.elapsed().as_micros() as u64,
            &compile_id,
        );
        // Journal the outcome (hit/miss/error + miss_reason) on the same
        // background-thread writer the daemon path uses. `log` never
        // blocks, so the embedded hot path pays only the context capture
        // above plus serde serialization — parity with the IPC path's
        // accepted cost (issue #459).
        if let Some((outcome, exit_code, default_reason)) = extract_outcome(&response) {
            let latency_ns = total.elapsed().as_nanos();
            let miss_reason = super::connection::compile_miss_reason(
                &journal_ctx,
                outcome,
                attributed_miss_reason.or(default_reason),
                latency_ns,
                self.state.cache_dir.as_path(),
            );
            if miss_reason == Some(miss_reason::UNKNOWN) {
                super::connection::append_unknown_miss_warning(
                    &mut response,
                    &journal_ctx,
                    latency_ns,
                );
            }
            let entry = JournalEntry::new(journal_ctx, outcome, exit_code, latency_ns, miss_reason)
                .with_context_key(context_key);
            self.state.journal.log(&entry, None);
        }
        match response {
            Response::CompileResult {
                exit_code,
                stdout,
                stderr,
                cached,
            } => {
                crate::compile_trace::record(
                    if cached {
                        "embedded_outcome_cached"
                    } else {
                        "embedded_outcome_miss"
                    },
                    0,
                    &compile_id,
                );
                Ok(EmbeddedCompileResult {
                    exit_code,
                    stdout,
                    stderr,
                    cached,
                })
            }
            Response::Error { message } => {
                crate::compile_trace::record("embedded_outcome_error", 0, &compile_id);
                Err(message)
            }
            other => Err(format!("unexpected embedded compile response: {other:?}")),
        }
    }

    pub(crate) async fn stats(&self) -> EmbeddedStatsSnapshot {
        EmbeddedStatsSnapshot {
            status: status_snapshot(&self.state).await,
            phase_profile: self.state.profiler.totals_snapshot().into(),
        }
    }

    pub(crate) async fn maintain_disk(
        &self,
        kind: MaintenanceKind,
    ) -> std::io::Result<DiskMaintenanceReport> {
        maintain_state_disk(Arc::clone(&self.state), self.maintenance_policy, kind).await
    }

    pub(crate) async fn flush(&self) -> EmbeddedFlushReport {
        let _maintenance_guard = self.state.disk_maintenance.lock().await;
        let mut index_writer_handle = self.index_writer_handle.lock().await;
        flush_embedded_state(&self.state, &mut index_writer_handle, false).await
    }

    pub(crate) async fn shutdown(&self) -> EmbeddedFlushReport {
        self.state.shutdown_requested.store(true, Ordering::Release);
        let maintenance_step = self.stop_maintenance_task().await;
        // A host-requested pass can run independently of the background
        // handle. Wait for it before the final index flush; new passes reject
        // the latched shutdown flag after acquiring this mutex.
        let _maintenance_guard = self.state.disk_maintenance.lock().await;
        let mut index_writer_handle = self.index_writer_handle.lock().await;
        let mut report = flush_embedded_state(&self.state, &mut index_writer_handle, true).await;
        report.steps.insert(0, maintenance_step);
        let _ = std::fs::remove_dir_all(&self.state.depfile_tmpdir);
        report
    }

    async fn stop_maintenance_task(&self) -> EmbeddedFlushStepReport {
        // `notify_one` retains a permit if the maintenance loop is between
        // polls. The latched shutdown flag remains the authoritative stop
        // condition.
        self.state.shutdown.notify_one();
        let handle = self.maintenance_handle.lock().await.take();
        let outcome = match handle {
            Some(handle) => join_task(handle, "embedded maintenance").await,
            None => FlushStepOutcome::Completed,
        };
        EmbeddedFlushStepReport {
            step: "maintenance_shutdown".to_owned(),
            outcome,
        }
    }
}

async fn join_task(task: tokio::task::JoinHandle<()>, task_name: &str) -> FlushStepOutcome {
    match task.await {
        Ok(()) => FlushStepOutcome::Completed,
        Err(error) => FlushStepOutcome::Failed(format!("{task_name} task failed: {error}")),
    }
}

async fn stop_index_writer_task(
    shutdown: &tokio::sync::Notify,
    handle: Option<tokio::task::JoinHandle<()>>,
) -> Option<EmbeddedFlushStepReport> {
    // `notify_one` retains a permit when the writer is between polls. Using
    // `notify_waiters` here can lose the signal in the small window after the
    // preceding Flush acknowledgement and before the writer reaches select!,
    // leaving graceful shutdown to time out on an otherwise healthy writer.
    shutdown.notify_one();
    let handle = handle?;
    let outcome = join_task(handle, "index writer").await;
    Some(EmbeddedFlushStepReport {
        step: "index_writer_shutdown".to_owned(),
        outcome,
    })
}

async fn status_snapshot(state: &SharedState) -> crate::protocol::DaemonStatus {
    let snap = state.stats.snapshot();
    let dg = state.dep_graph.load().stats();
    let artifact_count = state.artifacts.len() as u64;
    let cache_size_bytes: u64 = state
        .artifacts
        .iter()
        .map(|entry| entry.value().meta.total_size)
        .sum();
    let metadata_entries = state.cache_system.metadata().len() as u64;
    let private_daemon = state.private_daemon.snapshot().await;
    crate::protocol::DaemonStatus {
        version: crate::core::VERSION.to_string(),
        daemon_namespace: state.daemon_namespace.clone(),
        endpoint: state.endpoint.clone(),
        private_daemon,
        artifact_count,
        cache_size_bytes,
        metadata_entries,
        uptime_secs: now_secs().saturating_sub(state.start_time),
        cache_hits: snap.hits,
        cache_misses: snap.misses,
        total_compilations: snap.compilations,
        non_cacheable: snap.non_cacheable,
        compile_errors: snap.compile_errors,
        compile_errors_cached: snap.compile_errors_cached,
        time_saved_ms: snap.time_saved_ms(),
        total_links: snap.link_total,
        link_hits: snap.link_hits,
        link_misses: snap.link_misses,
        link_non_cacheable: snap.link_non_cacheable,
        dep_graph_contexts: dg.context_count as u64,
        dep_graph_files: dg.file_count as u64,
        sessions_total: snap.sessions_total,
        sessions_active: state.sessions.active_count() as u64,
        cache_dir: state.cache_dir.clone(),
        dep_graph_version: crate::depgraph::DEPGRAPH_VERSION,
        dep_graph_disk_size: embedded_depgraph_file_path(state)
            .metadata()
            .map(|m| m.len())
            .unwrap_or(0),
        dep_graph_persisted: state.dep_graph_persisted.load(Ordering::Acquire),
        watcher_active: state.watcher_active.load(Ordering::Acquire),
        watcher_degradations: state.watcher_degradations.load(Ordering::Relaxed),
    }
}

/// Await one persistence step to quiescence and preserve its result.
///
/// `spawn_blocking` work cannot be cancelled by dropping its Tokio join
/// future. Returning on a timer would therefore allow an older save to race a
/// later flush or cache archive. Persistence remains owned until completion;
/// callers can bound the IPC wait without detaching the daemon-side write.
async fn flush_step<F>(step: &str, fut: F) -> EmbeddedFlushStepReport
where
    F: std::future::Future<Output = Result<(), String>>,
{
    let outcome = match fut.await {
        Ok(()) => FlushStepOutcome::Completed,
        Err(error) => FlushStepOutcome::Failed(error),
    };
    match &outcome {
        FlushStepOutcome::Completed => {}
        FlushStepOutcome::Failed(error) => {
            tracing::warn!(
                event = "embedded_flush_step_failed",
                step,
                %error,
                "embedded flush persistence step failed"
            );
            crate::core::lifecycle::write_event(
                "embedded_flush_step_failed",
                serde_json::json!({
                    "step": step,
                    "error": error,
                }),
            );
        }
        FlushStepOutcome::TimedOut => unreachable!("owned persistence steps are not abandoned"),
    }
    EmbeddedFlushStepReport {
        step: step.to_owned(),
        outcome,
    }
}

async fn flush_embedded_state(
    state: &Arc<SharedState>,
    index_writer_handle: &mut Option<tokio::task::JoinHandle<()>>,
    shutdown_writer: bool,
) -> EmbeddedFlushReport {
    let pending_writes_drained = pending_writes::await_all(
        &state.pending_cache_writes,
        std::time::Duration::from_secs(30),
    )
    .await;

    // Detached publishers acquire an owned read guard before their handler
    // returns. Taking the write guard here waits for every such handoff before
    // the index writer is flushed or stopped, so no row can arrive afterward.
    // A regular flush may fail closed after its deadline; graceful shutdown
    // must wait because returning would allow a publisher to mutate the cache
    // after the final checkpoint.
    let publication_guard = if shutdown_writer {
        Some(state.artifact_publication.write().await)
    } else {
        tokio::time::timeout(
            EMBEDDED_PUBLICATION_BARRIER_TIMEOUT,
            state.artifact_publication.write(),
        )
        .await
        .ok()
    };
    let Some(_publication_guard) = publication_guard else {
        return EmbeddedFlushReport {
            pending_writes_drained,
            index_writer_drained: false,
            steps: vec![EmbeddedFlushStepReport {
                step: "publication_barrier".to_owned(),
                outcome: FlushStepOutcome::TimedOut,
            }],
            artifact_entries: state.artifact_store.len() as u64,
            metadata_entries: state.cache_system.metadata().len() as u64,
        };
    };

    let index_writer_drained =
        flush_index_writer(&state.index_writer_tx, std::time::Duration::from_secs(30)).await;
    if !index_writer_drained {
        tracing::warn!("timed out waiting for artifact index writer flush");
    }

    let mut steps = Vec::with_capacity(6);
    if shutdown_writer {
        if let Some(step) = stop_index_writer_task(
            state.index_writer_shutdown.as_ref(),
            index_writer_handle.take(),
        )
        .await
        {
            steps.push(step);
        }
    }

    let artifact_entries = state.artifact_store.len() as u64;
    steps.push(
        flush_step("artifact_store", async {
            Arc::clone(&state.artifact_store)
                .flush_async()
                .await
                .map_err(|error| error.to_string())
        })
        .await,
    );

    let dg = state.dep_graph.load_full();
    let depgraph_path = embedded_depgraph_file_path(state);
    let depgraph_state = Arc::clone(state);
    steps.push(
        flush_step("depgraph", async move {
            tokio::task::spawn_blocking(move || {
                if let Some(parent) = depgraph_path.parent() {
                    std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
                }
                crate::depgraph::save_to_file(&dg, depgraph_path.as_path())
                    .map_err(|error| error.to_string())?;
                depgraph_state
                    .dep_graph_persisted
                    .store(true, Ordering::Release);
                Ok::<(), String>(())
            })
            .await
            .map_err(|error| format!("depgraph save task failed: {error}"))?
        })
        .await,
    );

    let metadata_entries = state.cache_system.metadata().len() as u64;
    if state.metadata_cache_loaded.load(Ordering::Acquire) {
        let metadata_state = Arc::clone(state);
        let metadata_path = state.metadata_path.clone();
        steps.push(
            flush_step("metadata", async move {
                tokio::task::spawn_blocking(move || {
                    metadata_state
                        .cache_system
                        .metadata()
                        .save_to_disk(metadata_path.as_path())
                        .map_err(|error| error.to_string())
                })
                .await
                .map_err(|error| format!("metadata save task failed: {error}"))?
            })
            .await,
        );
    }

    if state.compiler_hash_cache_loaded.load(Ordering::Acquire) {
        let compiler_state = Arc::clone(state);
        let compiler_hash_cache_path = state.compiler_hash_cache_path.clone();
        steps.push(
            flush_step("compiler_hash", async move {
                tokio::task::spawn_blocking(move || {
                    compiler_state
                        .compiler_hash_cache
                        .save_to_disk(compiler_hash_cache_path.as_path())
                        .map_err(|error| error.to_string())
                })
                .await
                .map_err(|error| format!("compiler hash save task failed: {error}"))?
            })
            .await,
        );
    }

    if state.system_includes_loaded.load(Ordering::Acquire) {
        let includes = {
            let includes = state.system_includes.lock().await;
            includes.clone()
        };
        let system_includes_cache_path = state.system_includes_cache_path.clone();
        steps.push(
            flush_step("system_includes", async move {
                tokio::task::spawn_blocking(move || {
                    includes
                        .save_to_disk(system_includes_cache_path.as_path())
                        .map_err(|error| error.to_string())
                })
                .await
                .map_err(|error| format!("system includes save task failed: {error}"))?
            })
            .await,
        );
    }

    EmbeddedFlushReport {
        pending_writes_drained,
        index_writer_drained,
        steps,
        artifact_entries,
        metadata_entries,
    }
}

fn embedded_depgraph_file_path(state: &SharedState) -> crate::core::NormalizedPath {
    depgraph_file_path_for_cache_dir(&state.cache_dir)
}

#[cfg(test)]
mod flush_ownership_tests {
    use super::{
        flush_step, join_task, stop_index_writer_task, EmbeddedDaemon, FlushStepOutcome,
        MaintenancePolicy,
    };
    use std::sync::Arc;
    use std::time::Duration;

    #[tokio::test]
    async fn ready_step_reports_completion() {
        let outcome = flush_step("test", async { Ok::<(), String>(()) }).await;
        assert_eq!(
            outcome.outcome,
            FlushStepOutcome::Completed,
            "a completed persistence step must be reported as complete"
        );
    }

    #[tokio::test]
    async fn failed_step_preserves_error() {
        let outcome = flush_step("test", async { Err::<(), String>("disk full".into()) }).await;
        assert_eq!(
            outcome.outcome,
            FlushStepOutcome::Failed("disk full".into())
        );
    }

    #[tokio::test]
    async fn task_shutdown_waits_for_owned_work() {
        let task = tokio::spawn(async {
            tokio::time::sleep(Duration::from_millis(20)).await;
        });
        assert_eq!(
            join_task(task, "test task").await,
            FlushStepOutcome::Completed
        );
    }

    #[tokio::test]
    async fn index_writer_shutdown_signal_survives_between_waits() {
        let shutdown = Arc::new(tokio::sync::Notify::new());
        let writer_shutdown = Arc::clone(&shutdown);
        let task = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            writer_shutdown.notified().await;
        });

        let step = tokio::time::timeout(
            Duration::from_secs(1),
            stop_index_writer_task(shutdown.as_ref(), Some(task)),
        )
        .await
        .expect("index writer shutdown signal was lost")
        .expect("writer handle produces a shutdown step");

        assert_eq!(step.outcome, FlushStepOutcome::Completed);
    }

    #[tokio::test]
    async fn host_owned_maintenance_does_not_start_a_duplicate_scheduler() {
        let temp = tempfile::tempdir().expect("temp cache");
        let daemon = EmbeddedDaemon::start_with_maintenance(
            crate::ipc::unique_test_endpoint(),
            crate::core::NormalizedPath::new(temp.path()),
            None,
            MaintenancePolicy::default(),
            false,
        )
        .await
        .expect("embedded daemon");

        assert!(
            daemon.maintenance_handle.lock().await.is_none(),
            "host ownership must suppress the embedded periodic scheduler"
        );
        let report = daemon.shutdown().await;
        assert!(report.is_complete());
    }
}
