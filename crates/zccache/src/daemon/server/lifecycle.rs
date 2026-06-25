//! `DaemonServer` construction, configuration setters, and test seams.
//!
//! The hot-path `run()` and watcher pipeline live in `server::run`; this file
//! groups the smaller lifecycle methods so `mod.rs` stays thin.

use super::*;

/// Monotonic counter ensuring each `DaemonServer` instance gets unique
/// artifact and depfile directories, even within the same process.
pub(super) static SERVER_INSTANCE: AtomicU64 = AtomicU64::new(0);
pub(super) static ARTIFACT_PERSIST_TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

impl DaemonServer {
    /// Create a new daemon server bound to the given endpoint, using the
    /// configured cache directory (resolved via [`crate::core::config::default_cache_dir`]).
    ///
    /// Production callers should use this. Tests that need to isolate their
    /// cache directory must use [`Self::bind_with_cache_dir`] instead — this
    /// reads `ZCCACHE_CACHE_DIR` from a process-global env, which races when
    /// multiple tests run in parallel.
    pub fn bind(endpoint: &str) -> Result<Self, crate::ipc::IpcError> {
        Self::bind_with_cache_dir(endpoint, &crate::core::config::default_cache_dir())
    }

    /// Create a new daemon server bound to the given endpoint, rooted at an
    /// explicit cache directory. Bypasses the `ZCCACHE_CACHE_DIR` env var so
    /// parallel tests can each operate in isolation.
    pub fn bind_with_cache_dir(
        endpoint: &str,
        cache_dir: &crate::core::NormalizedPath,
    ) -> Result<Self, crate::ipc::IpcError> {
        let listener = IpcListener::bind(endpoint)?;
        let backend_identity = crate::ipc::current_backend_identity(endpoint)
            .map_err(|err| crate::ipc::IpcError::Endpoint(err.to_string()))?;
        let (state, index_writer_rx) = new_shared_state(endpoint, cache_dir, backend_identity);

        Ok(Self {
            listener,
            shutdown: Arc::clone(&state.shutdown),
            index_writer_rx: Some(index_writer_rx),
            state,
        })
    }
}

fn new_shared_state(
    endpoint: &str,
    cache_dir: &crate::core::NormalizedPath,
    backend_identity: running_process::broker::protocol_v2::backend_handle::DaemonProcess,
) -> (
    Arc<SharedState>,
    tokio::sync::mpsc::UnboundedReceiver<(String, ArtifactIndex)>,
) {
    let shutdown = Arc::new(Notify::new());
    let now = now_secs();
    let instance = SERVER_INSTANCE.fetch_add(1, Ordering::Relaxed);
    let artifact_dir = crate::core::config::artifacts_dir_from_cache_dir(cache_dir);
    std::fs::create_dir_all(&artifact_dir).ok();

    // Artifact loading is deferred to a background task in run() so the
    // daemon starts accepting connections immediately (Bug 6 fix).
    let artifacts: DashMap<String, CachedArtifact> = DashMap::new();

    // Issue #784 phase 2d: open the artifact-index store EMPTY
    // here so the readiness lockfile fires before reading +
    // decoding the bincode blob. The on-disk entries are merged
    // into the live `DashMap` by `artifact_store_loader()` in a
    // `tokio::task::spawn_blocking` after the lockfile. The
    // existing on-disk-fallback contract in
    // `util::lookup_artifact_with_disk_fallback` is preserved: if
    // a DashMap-miss request races ahead of the background load,
    // it calls `load_from_disk` synchronously on the spot so the
    // fallback still hits.
    let index_path = crate::core::config::index_path_from_cache_dir(cache_dir);
    let artifact_store = Arc::new(ArtifactStore::open_empty(&index_path));

    let (index_writer_tx, index_writer_rx) =
        tokio::sync::mpsc::unbounded_channel::<(String, ArtifactIndex)>();
    let index_writer_shutdown = Arc::new(Notify::new());

    // Try to restore the metadata cache from disk. A wrong-version /
    // corrupt snapshot falls back to an empty cache (the
    // `MetadataCache::lookup` stat-verify safety net still guards
    // correctness on every subsequent lookup).
    let metadata_path = crate::core::config::metadata_path_from_cache_dir(cache_dir);
    let compiler_hash_cache_path =
        crate::core::config::compiler_hash_cache_path_from_cache_dir(cache_dir);
    let system_includes_cache_path =
        crate::core::config::system_includes_cache_path_from_cache_dir(cache_dir);
    // Issue #541: persist the discovered C/C++ system include paths
    // across daemon restarts so the next daemon does not pay the
    // ~30-50 ms `<compiler> -v -E -x c++ NUL` spawn on its first
    // C/C++ compile. Stat-verify on lookup catches in-place compiler
    // upgrades, so a stale snapshot is harmless.
    //
    // Issue #784 phase 2c: start empty here. The on-disk snapshot
    // is loaded by `system_includes_loader()`'s `load_and_install()`
    // in a `spawn_blocking` AFTER the daemon's readiness lockfile is
    // written, so the disk read is removed from the spawn→lockfile
    // critical path. Compile requests arriving during the merge
    // window pay the cold compiler probe — same outcome as before
    // #541's persistence existed.
    let system_includes_loaded = crate::depgraph::SystemIncludeCache::new();
    // Issue #517: hashing rustc's ~150 MB binary on the cold path
    // costs ~50-60 ms per first-after-restart compile. Loading the
    // (path, mtime, size, hash) snapshot from a prior daemon makes
    // that first compile near-instant — the stat-verify in
    // `get_or_hash_with` keeps correctness if the binary changed since.
    //
    // Issue #784: start empty here. The on-disk snapshot is loaded
    // by `compiler_hash_cache_loader()`'s `install()` in a
    // `spawn_blocking` AFTER the daemon's readiness lockfile is
    // written, so the disk read is removed from the spawn→lockfile
    // critical path. Compile requests arriving during the merge
    // window take the cold blake3 path — same outcome as before
    // #517's persistence existed.
    let compiler_hash_cache = CompilerHashCache::new();
    // Issue #784 phase 2b: same deferred-load pattern as
    // `compiler_hash_cache` above — start with an empty
    // `CacheSystem`; the on-disk `metadata.bin` snapshot is read
    // by `metadata_cache_loader()`'s `load_and_install()` in a
    // `spawn_blocking` AFTER the readiness lockfile is written.
    // Compile requests during the merge window take the cold-path
    // re-stat / re-hash; the stat-verify safety net in
    // `MetadataCache::get_cached_hash_if_stat_valid` keeps cache
    // keys correct either way.
    let cache_system = CacheSystem::new();

    // Issue #813 / #810: log the effective compile-priority default
    // policy at daemon start so the behaviour is observable without
    // strace. Interactive hosts default to `Low` for both compile
    // and link children; CI runners (detected via well-known env
    // vars) preserve the historical `Normal` default.
    let ci_env = crate::daemon::process::is_ci_host();
    match ci_env {
        Some(env_var) => tracing::info!(
            ci_env = env_var,
            "[zccache] CI detected via {env_var} — compile/link priority defaults to Normal \
                 (set ZCCACHE_COMPILE_PRIORITY to override)",
        ),
        None => tracing::info!(
            "[zccache] interactive host — compile/link priority defaults to Low \
                 (set ZCCACHE_COMPILE_PRIORITY to override)",
        ),
    }

    // Issue #813 / #816: global compile-concurrency cap. Wraps all
    // daemon-spawned compiler children in a tokio semaphore so the
    // box can't be saturated by M cargo invocations each asking for
    // num_cpus rustcs (M*N explosion).
    let compile_concurrency =
        crate::daemon::server::compile_concurrency::resolve_pool(ci_env.is_some());
    match &compile_concurrency {
        Some(sem) => tracing::info!(
            cap = sem.available_permits(),
            "[zccache] compile concurrency capped at {} via in-process semaphore \
                 (set ZCCACHE_MAX_PARALLEL_COMPILES to override; =0 to disable)",
            sem.available_permits()
        ),
        None => tracing::info!(
            "[zccache] compile concurrency uncapped (ZCCACHE_MAX_PARALLEL_COMPILES=0)",
        ),
    }

    (
        Arc::new(SharedState {
            endpoint: endpoint.to_string(),
            backend_identity,
            daemon_namespace: crate::core::config::daemon_namespace_label(),
            cache_dir: cache_dir.clone(),
            private_daemon: PrivateDaemonLifecycle::new(),
            sessions: SessionManager::new(std::time::Duration::from_secs(300)),
            system_includes: Mutex::new(system_includes_loaded),
            system_includes_cache_path,
            dep_graph: arc_swap::ArcSwap::from_pointee(DepGraph::new()),
            artifacts,
            cache_system,
            watcher: Mutex::new(None),
            watched_dirs: Mutex::new(HashSet::new()),
            shutdown,
            last_activity: AtomicU64::new(now),
            start_time: now,
            stats: StatsCollector::new(),
            profiler: PhaseProfiler::new(),
            artifact_dir,
            metadata_path,
            compiler_hash_cache_path,
            depfile_tmpdir: {
                let dir = crate::core::config::depfile_dir_from_cache_dir(cache_dir)
                    .join(format!("{}-{instance}", std::process::id()));
                std::fs::create_dir_all(&dir).ok();
                dir
            },
            fast_hit_cache: DashMap::new(),
            watcher_active: AtomicBool::new(false),
            rsp_cache: DashMap::new(),
            request_cache: DashMap::new(),
            session_worktree_roots: DashMap::new(),
            ended_sessions: DashMap::new(),
            request_validation_cache: DashMap::new(),
            compiler_hash_cache,
            watched_raw_dirs: DashMap::new(),
            pch_source_map: DashMap::new(),
            journal: CompileJournal::new(crate::core::config::log_dir_from_cache_dir(cache_dir)),
            in_flight_bytes: AtomicUsize::new(0),
            persist_semaphore: Arc::new(tokio::sync::Semaphore::new(persist_workers_default())),
            compile_concurrency,
            artifact_store,
            index_writer_tx,
            index_writer_shutdown,
            artifacts_loaded: AtomicBool::new(false),
            compiler_hash_cache_loaded: AtomicBool::new(false),
            metadata_cache_loaded: AtomicBool::new(false),
            system_includes_loaded: AtomicBool::new(false),
            artifact_store_loaded: AtomicBool::new(false),
            shutdown_event_logged: AtomicBool::new(false),
            shutdown_requested: AtomicBool::new(false),
            fingerprint: FingerprintManager::new(),
            dep_graph_persisted: AtomicBool::new(false),
            dep_graph_load_complete: AtomicBool::new(true),
            dep_graph_load_notify: Arc::new(Notify::new()),
            depgraph_load_warning: StdMutex::new(None),
            in_flight_exec: DashMap::new(),
            pending_cache_writes: DashMap::new(),
            exec_cache: DashMap::new(),
        }),
        index_writer_rx,
    )
}

impl EmbeddedDaemon {
    pub(crate) async fn start(
        endpoint: String,
        cache_dir: crate::core::NormalizedPath,
    ) -> Result<Self, crate::ipc::IpcError> {
        let backend_identity = crate::ipc::current_backend_identity(&endpoint)
            .map_err(|err| crate::ipc::IpcError::Endpoint(err.to_string()))?;
        let (state, index_writer_rx) = new_shared_state(&endpoint, &cache_dir, backend_identity);

        let mut daemon = Self {
            state,
            index_writer_rx: Some(index_writer_rx),
            index_writer_handle: Mutex::new(None),
        };
        daemon.start_background_tasks().await;
        Ok(daemon)
    }

    async fn start_background_tasks(&mut self) {
        if let Some(rx) = self.index_writer_rx.take() {
            let store = Arc::clone(&self.state.artifact_store);
            let shutdown = Arc::clone(&self.state.index_writer_shutdown);
            *self.index_writer_handle.lock().await =
                Some(tokio::spawn(run_index_writer(rx, store, shutdown)));
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
        match response {
            Response::CompileResult {
                exit_code,
                stdout,
                stderr,
                cached,
            } => Ok(EmbeddedCompileResult {
                exit_code,
                stdout,
                stderr,
                cached,
            }),
            Response::Error { message } => Err(message),
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
        self.state.index_writer_shutdown.notify_waiters();
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

    if shutdown_writer {
        state.index_writer_shutdown.notify_waiters();
        if let Some(handle) = index_writer_handle.as_mut() {
            if !handle.is_finished() {
                let _ = tokio::time::timeout(std::time::Duration::from_secs(2), handle).await;
            }
        }
    }

    let artifact_entries = state.artifact_store.len() as u64;
    let _ = Arc::clone(&state.artifact_store).flush_async().await;

    let dg = state.dep_graph.load_full();
    let depgraph_path = embedded_depgraph_file_path(state);
    let depgraph_state = Arc::clone(state);
    let _ = tokio::task::spawn_blocking(move || {
        if let Some(parent) = depgraph_path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        if crate::depgraph::save_to_file(&dg, depgraph_path.as_path()).is_ok() {
            depgraph_state
                .dep_graph_persisted
                .store(true, Ordering::Release);
        }
    })
    .await;

    let metadata_entries = state.cache_system.metadata().len() as u64;
    if state.metadata_cache_loaded.load(Ordering::Acquire) {
        let metadata_state = Arc::clone(state);
        let metadata_path = state.metadata_path.clone();
        let _ = tokio::task::spawn_blocking(move || {
            metadata_state
                .cache_system
                .metadata()
                .save_to_disk(metadata_path.as_path())
        })
        .await;
    }

    if state.compiler_hash_cache_loaded.load(Ordering::Acquire) {
        let compiler_state = Arc::clone(state);
        let compiler_hash_cache_path = state.compiler_hash_cache_path.clone();
        let _ = tokio::task::spawn_blocking(move || {
            compiler_state
                .compiler_hash_cache
                .save_to_disk(compiler_hash_cache_path.as_path())
        })
        .await;
    }

    if state.system_includes_loaded.load(Ordering::Acquire) {
        let includes = {
            let includes = state.system_includes.lock().await;
            includes.clone()
        };
        let system_includes_cache_path = state.system_includes_cache_path.clone();
        let _ = tokio::task::spawn_blocking(move || {
            includes.save_to_disk(system_includes_cache_path.as_path())
        })
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

impl DaemonServer {
    /// Get a handle to signal shutdown.
    #[must_use]
    pub fn shutdown_handle(&self) -> Arc<Notify> {
        Arc::clone(&self.shutdown)
    }

    /// Clone the running-process identity served by this daemon.
    ///
    /// Slice 24 of zccache#782: migrated to the `protocol_v2::backend_handle`
    /// namespace.
    #[must_use]
    pub fn backend_identity(
        &self,
    ) -> running_process::broker::protocol_v2::backend_handle::DaemonProcess {
        self.state.backend_identity.clone()
    }

    /// Replace the dependency graph with a pre-loaded one.
    ///
    /// **Pre-#640**: this required `&mut self` and `Arc::get_mut` —
    /// constraints that locked the call to BEFORE `run()` and prevented
    /// post-bind injection from a background task. The field is now
    /// `ArcSwap<Arc<DepGraph>>` so atomic replacement is safe at any
    /// time, including from a `tokio::task::spawn_blocking` started
    /// after `run()`.
    ///
    /// Marks the graph as persisted because it was restored from disk.
    pub fn set_dep_graph(&self, graph: crate::depgraph::DepGraph) {
        self.state.dep_graph.store(std::sync::Arc::new(graph));
        self.state
            .dep_graph_persisted
            .store(true, Ordering::Release);
        self.state
            .dep_graph_load_complete
            .store(true, Ordering::Release);
        self.state.dep_graph_load_notify.notify_waiters();
    }

    /// Mark startup depgraph classification as pending.
    ///
    /// The daemon binary calls this before it offloads `depgraph.bin` loading
    /// to `spawn_blocking`. Compile handlers use the paired notify to avoid
    /// making the first warm compile race against the empty default graph.
    #[doc(hidden)]
    pub fn mark_dep_graph_load_pending(&self) {
        self.state
            .dep_graph_load_complete
            .store(false, Ordering::Release);
    }

    /// Record a load-time depgraph warning to mirror into per-session logs.
    ///
    /// Called by the daemon's startup path after [`crate::depgraph::classify_load`]
    /// returns a non-`Loaded` outcome that warrants surfacing (version
    /// mismatch, corruption, I/O error). The warning is appended to each
    /// session's log file at `SessionStart` so a cold fallback caused by a
    /// stale or corrupt `depgraph.bin` is visible to operators. Issue #320.
    ///
    /// **Post-#640**: takes `&self` and uses the field's existing
    /// `tokio::sync::Mutex` via `blocking_lock` — safe to call from a
    /// `tokio::task::spawn_blocking` after `run()` has started so the
    /// daemon can move the depgraph load off the bind critical path.
    pub fn set_depgraph_load_warning(&self, warning: String) {
        let mut guard = self
            .state
            .depgraph_load_warning
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *guard = Some(warning);
    }

    /// Hand out an owned handle that can install a loaded depgraph (and
    /// optional warning) from any thread, at any time during the daemon's
    /// lifetime. This is the #640 seam for the deferred-depgraph-load —
    /// the background `spawn_blocking` loader holds a `DepGraphSetter` and
    /// calls `install()` when the disk load completes; meanwhile
    /// `server.run()` has been accepting connections from t ≈ 0.
    pub fn dep_graph_setter(&self) -> DepGraphSetter {
        DepGraphSetter {
            state: Arc::clone(&self.state),
        }
    }

    /// Hand out a loader that reads the compiler-hash cache from disk and
    /// installs it into the running state. Issue #784 — moves the load
    /// out of `bind_with_cache_dir`'s critical path so the readiness
    /// lockfile fires before any disk I/O.
    ///
    /// Designed to be called once, immediately after `write_lock_file`,
    /// inside a `tokio::task::spawn_blocking`. The handle captures the
    /// on-disk snapshot path so the daemon binary doesn't need access
    /// to the (private) `compiler_hash_cache_path` field.
    pub fn compiler_hash_cache_loader(&self) -> CompilerHashCacheLoader {
        CompilerHashCacheLoader {
            state: Arc::clone(&self.state),
            path: self.state.compiler_hash_cache_path.clone(),
        }
    }

    /// Hand out a loader that reads the on-disk `metadata.bin` snapshot
    /// and installs it into the running state's `CacheSystem`. Issue
    /// #784 phase 2b — extends the compiler-hash-cache deferral above
    /// to the biggest of the four snapshots (the one that scales with
    /// the cache size).
    ///
    /// Designed to be called once, immediately after `write_lock_file`,
    /// inside a `tokio::task::spawn_blocking`. The handle captures the
    /// metadata path so the daemon binary doesn't need access to the
    /// private `metadata_path` field on `SharedState`.
    pub fn metadata_cache_loader(&self) -> MetadataCacheLoader {
        MetadataCacheLoader {
            state: Arc::clone(&self.state),
            path: self.state.metadata_path.clone(),
        }
    }

    /// Hand out a loader that reads the on-disk system-includes snapshot
    /// and merges it into the live `Mutex<SystemIncludeCache>` on the
    /// state. Issue #784 phase 2c.
    ///
    /// Designed to be called once, immediately after `write_lock_file`,
    /// inside a `tokio::task::spawn_blocking`. The loader uses
    /// `blocking_lock()` on the tokio mutex so the brief merge fits
    /// inside the blocking thread (same shape as
    /// `set_depgraph_load_warning`).
    pub fn system_includes_loader(&self) -> SystemIncludesLoader {
        SystemIncludesLoader {
            state: Arc::clone(&self.state),
            path: self.state.system_includes_cache_path.clone(),
        }
    }

    /// Hand out a loader that reads the on-disk `index.bin` blob and
    /// merges its entries into the live `ArtifactStore`. Issue #784
    /// phase 2d — last of the four #784 deferrals.
    ///
    /// Designed to be called once, immediately after `write_lock_file`,
    /// inside a `tokio::task::spawn_blocking`. The handle holds an
    /// `Arc<ArtifactStore>` clone so the merge writes hit the same
    /// `DashMap` request handlers read. If a lookup races ahead of
    /// this background load,
    /// `util::lookup_artifact_with_disk_fallback` invokes
    /// [`ArtifactStore::load_from_disk`] synchronously on the spot —
    /// idempotent because both call sites do equivalent inserts.
    pub fn artifact_store_loader(&self) -> ArtifactStoreLoader {
        ArtifactStoreLoader {
            state: Arc::clone(&self.state),
            store: Arc::clone(&self.state.artifact_store),
        }
    }

    /// Get a snapshot of the phase profiler (for benchmarks).
    #[must_use]
    pub fn profile_snapshot(&self) -> super::super::stats::ProfileSnapshot {
        self.state.profiler.snapshot()
    }

    /// Test-only seam: exercise the DashMap → on-disk-`ArtifactStore`
    /// fallback used by every artifact-lookup site.
    ///
    /// Returns `true` if the key is found either in the in-memory
    /// `artifacts` DashMap or in the on-disk artifact store (in which
    /// case the DashMap is hydrated as a side-effect). Lets perf tests
    /// assert that warm-after-restore lookups hit the on-disk store
    /// without spinning up an IPC server + real compile.
    #[doc(hidden)]
    #[must_use]
    pub fn test_lookup_artifact(&self, key_hex: &str) -> bool {
        lookup_artifact_with_disk_fallback(&self.state, key_hex).is_some()
    }

    /// Test-only seam: report whether the background artifact-load
    /// task has finished hydrating `state.artifacts`. Used by
    /// `perf_artifact_fallback_test.rs` to assert that the fallback
    /// path is the one being exercised (not the post-load fast path).
    #[doc(hidden)]
    #[must_use]
    pub fn test_artifacts_loaded(&self) -> bool {
        self.state.artifacts_loaded.load(Ordering::Acquire)
    }

    /// Test-only seam: report the number of entries currently in the
    /// in-memory `state.artifacts` DashMap. Lets the perf test assert
    /// that a fresh bind (before `run()`) starts with an empty
    /// DashMap, proving that any subsequent hit comes from the
    /// on-disk fallback path.
    #[doc(hidden)]
    #[must_use]
    pub fn test_artifacts_len(&self) -> usize {
        self.state.artifacts.len()
    }

    /// Test-only seam: insert a synthetic system-includes entry for the
    /// given compiler path. Issue #558 — lets the `handle_clear` test
    /// pre-populate the cache before sending Clear so the test can
    /// verify the entry survives.
    #[doc(hidden)]
    pub async fn test_insert_system_includes(
        &self,
        compiler: crate::core::NormalizedPath,
        paths: Vec<crate::core::NormalizedPath>,
    ) {
        let mut cache = self.state.system_includes.lock().await;
        cache.insert(compiler, paths);
    }

    /// Test-only seam: report the number of entries currently in the
    /// in-memory `state.system_includes` cache. Issue #558 — used to
    /// assert `handle_clear` preserves compiler-environment data
    /// (consistent with `compiler_hash_cache` which is also preserved).
    #[doc(hidden)]
    #[must_use]
    pub async fn test_system_includes_len(&self) -> usize {
        self.state.system_includes.lock().await.len()
    }

    /// Test-only seam: borrow the `SharedState` so tests can invoke
    /// the request handlers directly (e.g. `handle_clear`) without
    /// standing up the full IPC machinery. Issue #558.
    #[doc(hidden)]
    #[cfg(test)]
    #[must_use]
    pub(super) fn test_state(&self) -> &SharedState {
        &self.state
    }

    /// Test-only seam: clone the `Arc<SharedState>` so tests can call
    /// handlers whose signatures want an owned arc (e.g. `handle_exec_probe`).
    /// Issue #838.
    #[doc(hidden)]
    #[cfg(test)]
    #[must_use]
    pub(super) fn test_state_arc(&self) -> Arc<SharedState> {
        Arc::clone(&self.state)
    }
}

/// Owned handle to install a loaded depgraph on a running [`DaemonServer`].
///
/// Holds a clone of the daemon's `Arc<SharedState>`. The setter survives
/// across `tokio::task::spawn_blocking` boundaries (it is `Send + Sync`),
/// so `bin/zccache-daemon::run_server` can hand one off to the background
/// loader BEFORE calling `server.run()`. When the disk load finishes, the
/// loader calls [`Self::install`] and the daemon's hot-path readers
/// (`state.dep_graph.load()` at every compile request) atomically pick up
/// the new graph on their next `.load()`.
///
/// Issue #640.
pub struct DepGraphSetter {
    state: Arc<SharedState>,
}

impl DepGraphSetter {
    /// Atomically install a loaded depgraph (and optional warning).
    ///
    /// - `graph = Some(g)` swaps the daemon's empty default with the
    ///   loaded graph and marks the on-disk snapshot as persisted (so
    ///   the next clean shutdown doesn't re-save the empty default
    ///   over the real graph).
    /// - `graph = None` leaves the empty default in place. Use this
    ///   for the `Missing` / corrupt-load fallback so the warning
    ///   still routes into the per-session log via `warning`.
    /// - `warning` is mirrored into `SessionStart`'s per-session log
    ///   if `Some` (issue #320).
    pub fn install(self, graph: Option<crate::depgraph::DepGraph>, warning: Option<String>) {
        if let Some(graph) = graph {
            self.state.dep_graph.store(Arc::new(graph));
            self.state
                .dep_graph_persisted
                .store(true, Ordering::Release);
        }
        if let Some(warning) = warning {
            let mut guard = self
                .state
                .depgraph_load_warning
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            *guard = Some(warning);
        }
        self.state
            .dep_graph_load_complete
            .store(true, Ordering::Release);
        self.state.dep_graph_load_notify.notify_waiters();
    }
}

/// Owned handle that loads the on-disk compiler-hash-cache snapshot and
/// merges it into the running daemon's state.
///
/// Issue #784. Mirrors [`DepGraphSetter`]'s role for the depgraph: the
/// daemon binary holds one across a `spawn_blocking` boundary and calls
/// [`Self::load_and_install`] once after the readiness lockfile is
/// written. The merge uses [`CompilerHashCache::merge_from`], which is
/// `&self`, so concurrent `get_or_hash_with` readers either see no entry
/// (cold-hash path) or a loaded entry (stat-verify guards correctness).
pub struct CompilerHashCacheLoader {
    state: Arc<SharedState>,
    path: crate::core::NormalizedPath,
}

/// Owned handle that loads the on-disk system-includes snapshot and
/// merges it into the running daemon's `Mutex<SystemIncludeCache>`.
///
/// Issue #784 phase 2c. Same shape as [`CompilerHashCacheLoader`] but
/// uses `blocking_lock()` to acquire the tokio mutex briefly during
/// the merge (the `SystemIncludeCache` itself uses `HashMap` with
/// `&mut self` mutations, so the mutex stays on the live field).
pub struct SystemIncludesLoader {
    state: Arc<SharedState>,
    path: crate::core::NormalizedPath,
}

/// Owned handle that reads the on-disk artifact-index blob and merges
/// its entries into the running daemon's `ArtifactStore`.
///
/// Issue #784 phase 2d. Holds an `Arc<ArtifactStore>` directly so the
/// merge writes hit the same `DashMap` the request handlers read — no
/// swap needed (the store was constructed empty at bind time).
///
/// Coexists safely with the on-demand `load_from_disk` invocation
/// inside `util::lookup_artifact_with_disk_fallback`: both call sites
/// perform identical `DashMap::insert` operations, so a race between
/// them produces the same converged state. The `artifact_store_loaded`
/// flag merely prevents redundant disk reads — it is not a load-once
/// gate.
pub struct ArtifactStoreLoader {
    state: Arc<SharedState>,
    store: Arc<ArtifactStore>,
}

impl ArtifactStoreLoader {
    /// Read the on-disk index blob (if any) and insert its entries
    /// into the live store via [`ArtifactStore::load_from_disk`].
    ///
    /// I/O errors are logged at WARN and treated as "empty" — the
    /// daemon stays running with an empty in-memory index, and the
    /// next periodic flush will rewrite the file from whatever
    /// request-handler inserts have landed since. After the load,
    /// `artifact_store_loaded` is set so subsequent on-demand calls
    /// from `lookup_artifact_with_disk_fallback` short-circuit.
    pub fn load_and_install(self) {
        if let Err(e) = self.store.load_from_disk() {
            tracing::warn!("artifact index load failed, continuing with empty store: {e}");
        }
        self.state
            .artifact_store_loaded
            .store(true, Ordering::Release);
    }
}

impl SystemIncludesLoader {
    /// Load the on-disk snapshot (if any) and merge it into the live
    /// cache.
    ///
    /// A missing or corrupt snapshot is logged at WARN and the live
    /// cache is left empty (the stat-verify safety net in
    /// `SystemIncludeCache::get` / `get_or_discover` keeps correctness
    /// either way). After the merge — successful or empty —
    /// `system_includes_loaded` is set so `run.rs`'s shutdown path
    /// knows the in-memory state is canonical and safe to save.
    pub fn load_and_install(self) {
        match crate::depgraph::SystemIncludeCache::load_from_disk(self.path.as_path()) {
            Ok(loaded) => {
                let loaded_len = loaded.len();
                {
                    let mut live = self.state.system_includes.blocking_lock();
                    live.merge_from(loaded);
                }
                if loaded_len > 0 {
                    tracing::info!(
                        loaded = loaded_len,
                        path = %self.path.display(),
                        "system include cache restored from disk (background)"
                    );
                }
            }
            Err(e) => {
                tracing::warn!(
                    path = %self.path.display(),
                    "failed to load system include cache, starting empty: {e}"
                );
            }
        }
        self.state
            .system_includes_loaded
            .store(true, Ordering::Release);
    }
}

impl CompilerHashCacheLoader {
    /// Load the on-disk compiler-hash-cache snapshot (if any) and merge
    /// it into the live state.
    ///
    /// A missing or corrupt snapshot is logged at WARN and the live
    /// cache is left empty (the stat-verify safety net in
    /// `get_or_hash_with` keeps correctness either way). After the
    /// merge — successful or empty — `compiler_hash_cache_loaded` is
    /// set so `run.rs`'s shutdown path knows the in-memory state is
    /// canonical and safe to save.
    pub fn load_and_install(self) {
        match CompilerHashCache::load_from_disk(self.path.as_path()) {
            Ok(loaded) => {
                self.state.compiler_hash_cache.merge_from(loaded);
            }
            Err(e) => {
                tracing::warn!(
                    path = %self.path.display(),
                    "failed to load compiler hash cache, starting empty: {e}"
                );
            }
        }
        self.state
            .compiler_hash_cache_loaded
            .store(true, Ordering::Release);
    }
}

/// Owned handle that loads the on-disk `metadata.bin` snapshot and
/// merges it into the running daemon's `CacheSystem`.
///
/// Issue #784 phase 2b. Same shape as [`CompilerHashCacheLoader`] —
/// the daemon binary holds one across a `spawn_blocking` boundary and
/// calls [`Self::load_and_install`] once after the readiness lockfile
/// is written. The merge uses [`crate::fscache::MetadataCache::merge_from`],
/// which is `&self`, so concurrent `get_cached_hash_if_stat_valid`
/// readers either see no entry (cold-path miss — re-stat + re-hash)
/// or a loaded entry (stat-verify guards correctness).
pub struct MetadataCacheLoader {
    state: Arc<SharedState>,
    path: crate::core::NormalizedPath,
}

impl MetadataCacheLoader {
    /// Load the on-disk `metadata.bin` snapshot (if any) and merge it
    /// into the live state.
    ///
    /// A missing or corrupt snapshot is logged at WARN and the live
    /// cache is left empty (the stat-verify safety net in
    /// `get_cached_hash_if_stat_valid` keeps correctness either way).
    /// After the merge — successful or empty — `metadata_cache_loaded`
    /// is set so `run.rs`'s shutdown path knows the in-memory state is
    /// canonical and safe to save.
    pub fn load_and_install(self) {
        match crate::fscache::MetadataCache::load_from_disk(self.path.as_path()) {
            Ok(loaded) => {
                let loaded_len = loaded.len();
                self.state.cache_system.metadata().merge_from(loaded);
                if loaded_len > 0 {
                    tracing::info!(
                        loaded = loaded_len,
                        path = %self.path.display(),
                        "metadata cache restored from disk (background)"
                    );
                }
            }
            Err(e) => {
                tracing::warn!(
                    path = %self.path.display(),
                    "failed to load metadata cache, starting empty: {e}"
                );
            }
        }
        self.state
            .metadata_cache_loaded
            .store(true, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_ctx(source: &str) -> crate::depgraph::CompileContext {
        crate::depgraph::CompileContext {
            source_file: source.into(),
            include_search: crate::depgraph::IncludeSearchPaths::default(),
            defines: Vec::new(),
            flags: Vec::new(),
            force_includes: Vec::new(),
            unknown_flags: Vec::new(),
        }
    }

    fn dummy_hash(path: &std::path::Path) -> Option<crate::hash::ContentHash> {
        Some(crate::hash::hash_bytes(path.to_string_lossy().as_bytes()))
    }

    #[tokio::test]
    async fn depgraph_load_gate_waits_until_loaded_graph_is_visible() {
        let server = DaemonServer::bind(&crate::ipc::unique_test_endpoint()).unwrap();
        let state = Arc::clone(&server.state);
        let setter = server.dep_graph_setter();

        let graph = crate::depgraph::DepGraph::new();
        let ctx = make_ctx("/src/warm.cc");
        let key = graph.register(ctx);
        graph.update(
            &key,
            crate::depgraph::ScanResult {
                resolved: Vec::new(),
                unresolved: Vec::new(),
                has_computed: false,
            },
            dummy_hash,
        );

        server.mark_dep_graph_load_pending();
        let handle = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(25));
            setter.install(Some(graph), None);
        });

        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            state.dep_graph_load_notify.notified(),
        )
        .await
        .expect("depgraph load notify should fire");
        handle.join().unwrap();

        assert!(
            !state.dep_graph.load().is_cold(&key),
            "first compile must see the loaded warm graph instead of the empty default"
        );
        assert!(state.dep_graph_persisted.load(Ordering::Acquire));
    }
}
