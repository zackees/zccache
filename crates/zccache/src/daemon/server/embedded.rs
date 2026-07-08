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

/// Per-step timeout for the embedded flush's disk saves (issue #973). The
/// earlier flush steps (pending-writes drain, index-writer flush) are already
/// bounded, but the depgraph / metadata / compiler-hash / system-includes saves
/// and the artifact-store `flush_async` were not — a stuck disk (network FS, AV
/// scan, full volume) could therefore hang `ZccacheService::flush()` /
/// `shutdown()`, i.e. soldr/fbuild's exit path, forever. Bounding each step lets
/// flush stay responsive; the abandoned `spawn_blocking` write completes (or
/// not) on its own and every save uses atomic tmp+rename, so nothing partial is
/// ever visible.
const EMBEDDED_FLUSH_SAVE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

impl EmbeddedDaemon {
    pub(crate) async fn start(
        endpoint: String,
        cache_dir: crate::core::NormalizedPath,
        runtime_handle: Option<tokio::runtime::Handle>,
    ) -> Result<Self, crate::ipc::IpcError> {
        let backend_identity = crate::ipc::current_backend_identity(&endpoint)
            .map_err(|err| crate::ipc::IpcError::Endpoint(err.to_string()))?;
        let (state, index_writer_rx) = new_shared_state(&endpoint, &cache_dir, backend_identity);
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
            index_writer_rx: Some(index_writer_rx),
            index_writer_handle: Mutex::new(None),
        };
        daemon.start_background_tasks(runtime_handle).await;
        Ok(daemon)
    }

    async fn start_background_tasks(&mut self, runtime_handle: Option<tokio::runtime::Handle>) {
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
        let journal_ctx = JournalContext {
            compiler: request.compiler.to_string_lossy().into_owned(),
            args: request.args.clone(),
            cwd: request.cwd.to_string_lossy().into_owned(),
            env: request.env.clone(),
            session_id: None,
        };
        let response = handle_compile_ephemeral(
            &self.state,
            std::process::id(),
            &request.cwd,
            &request.compiler,
            &request.args,
            &request.cwd,
            request.env,
            request.stdin,
        )
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
            let miss_reason =
                super::connection::compile_miss_reason(&journal_ctx, outcome, default_reason);
            let entry = JournalEntry::new(
                journal_ctx,
                outcome,
                exit_code,
                total.elapsed().as_nanos(),
                miss_reason,
            );
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

    pub(crate) async fn flush(&self) -> EmbeddedFlushReport {
        let mut index_writer_handle = self.index_writer_handle.lock().await;
        flush_embedded_state(&self.state, &mut index_writer_handle, false).await
    }

    pub(crate) async fn shutdown(&self) -> EmbeddedFlushReport {
        self.state.shutdown_requested.store(true, Ordering::Release);
        let mut index_writer_handle = self.index_writer_handle.lock().await;
        let report = flush_embedded_state(&self.state, &mut index_writer_handle, true).await;
        let _ = std::fs::remove_dir_all(&self.state.depfile_tmpdir);
        report
    }
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
    }
}

/// Await a single flush save step, bounded by [`EMBEDDED_FLUSH_SAVE_TIMEOUT`].
/// On timeout it complains loudly (`warn!`) and writes a durable lifecycle
/// event (forensics), then returns so flush keeps making progress instead of
/// hanging on a stuck disk (issue #973). The step's result is intentionally
/// discarded — flush is best-effort and its report reflects in-memory counts.
async fn bounded_flush_step<F, T>(step: &str, fut: F)
where
    F: std::future::Future<Output = T>,
{
    if flush_step_timed_out(fut, EMBEDDED_FLUSH_SAVE_TIMEOUT).await {
        tracing::warn!(
            event = "embedded_flush_step_timeout",
            step,
            timeout_ms = EMBEDDED_FLUSH_SAVE_TIMEOUT.as_millis() as u64,
            "embedded flush save step exceeded its timeout — abandoning it so \
             ZccacheService::flush()/shutdown() stays responsive on a stuck disk \
             (issue #973)"
        );
        crate::core::lifecycle::write_event(
            "embedded_flush_step_timeout",
            serde_json::json!({
                "step": step,
                "timeout_ms": EMBEDDED_FLUSH_SAVE_TIMEOUT.as_millis() as u64,
                "reason": "flush save step exceeded timeout; abandoned to keep flush/shutdown responsive",
            }),
        );
    }
}

/// Run `fut` with a timeout, returning `true` if it timed out. Split from
/// [`bounded_flush_step`] (which owns the logging) so the timeout wiring is
/// deterministically unit-testable without touching the lifecycle log.
async fn flush_step_timed_out<F, T>(fut: F, timeout: std::time::Duration) -> bool
where
    F: std::future::Future<Output = T>,
{
    tokio::time::timeout(timeout, fut).await.is_err()
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

    let index_writer_drained =
        flush_index_writer(&state.index_writer_tx, std::time::Duration::from_secs(30)).await;
    if !index_writer_drained {
        tracing::warn!("timed out waiting for artifact index writer flush");
    }

    if shutdown_writer {
        state.index_writer_shutdown.notify_waiters();
        if let Some(handle) = index_writer_handle.as_mut() {
            if !handle.is_finished() {
                let _ = tokio::time::timeout(std::time::Duration::from_secs(2), handle).await;
            }
        }
    }

    let artifact_entries = state.artifact_store.len() as u64;
    bounded_flush_step(
        "artifact_store",
        Arc::clone(&state.artifact_store).flush_async(),
    )
    .await;

    let dg = state.dep_graph.load_full();
    let depgraph_path = embedded_depgraph_file_path(state);
    let depgraph_state = Arc::clone(state);
    bounded_flush_step(
        "depgraph",
        tokio::task::spawn_blocking(move || {
            if let Some(parent) = depgraph_path.parent() {
                std::fs::create_dir_all(parent).ok();
            }
            if crate::depgraph::save_to_file(&dg, depgraph_path.as_path()).is_ok() {
                depgraph_state
                    .dep_graph_persisted
                    .store(true, Ordering::Release);
            }
        }),
    )
    .await;

    let metadata_entries = state.cache_system.metadata().len() as u64;
    if state.metadata_cache_loaded.load(Ordering::Acquire) {
        let metadata_state = Arc::clone(state);
        let metadata_path = state.metadata_path.clone();
        bounded_flush_step(
            "metadata",
            tokio::task::spawn_blocking(move || {
                metadata_state
                    .cache_system
                    .metadata()
                    .save_to_disk(metadata_path.as_path())
            }),
        )
        .await;
    }

    if state.compiler_hash_cache_loaded.load(Ordering::Acquire) {
        let compiler_state = Arc::clone(state);
        let compiler_hash_cache_path = state.compiler_hash_cache_path.clone();
        bounded_flush_step(
            "compiler_hash",
            tokio::task::spawn_blocking(move || {
                compiler_state
                    .compiler_hash_cache
                    .save_to_disk(compiler_hash_cache_path.as_path())
            }),
        )
        .await;
    }

    if state.system_includes_loaded.load(Ordering::Acquire) {
        let includes = {
            let includes = state.system_includes.lock().await;
            includes.clone()
        };
        let system_includes_cache_path = state.system_includes_cache_path.clone();
        bounded_flush_step(
            "system_includes",
            tokio::task::spawn_blocking(move || {
                includes.save_to_disk(system_includes_cache_path.as_path())
            }),
        )
        .await;
    }

    EmbeddedFlushReport {
        pending_writes_drained,
        artifact_entries,
        metadata_entries,
    }
}

fn embedded_depgraph_file_path(state: &SharedState) -> crate::core::NormalizedPath {
    state.cache_dir.join("depgraph").join("depgraph.bin")
}

#[cfg(test)]
mod flush_timeout_tests {
    //! Issue #973: the embedded flush save steps must be bounded so a stuck
    //! disk cannot hang `ZccacheService::flush()`/`shutdown()`.
    use super::flush_step_timed_out;
    use std::time::Duration;

    #[tokio::test]
    async fn ready_step_does_not_time_out() {
        let timed_out = flush_step_timed_out(async { 42u32 }, Duration::from_secs(30)).await;
        assert!(
            !timed_out,
            "a step that completes must not report a timeout"
        );
    }

    #[tokio::test]
    async fn stuck_step_times_out() {
        // A save that never completes (stuck disk) must be abandoned at the
        // bound rather than hanging flush forever.
        let stuck = std::future::pending::<()>();
        let timed_out = flush_step_timed_out(stuck, Duration::from_millis(50)).await;
        assert!(
            timed_out,
            "a stuck step must report a timeout so flush continues"
        );
    }
}
