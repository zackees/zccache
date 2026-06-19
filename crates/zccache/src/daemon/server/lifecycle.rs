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
        let shutdown = Arc::new(Notify::new());
        let now = now_secs();
        let instance = SERVER_INSTANCE.fetch_add(1, Ordering::Relaxed);
        let artifact_dir = crate::core::config::artifacts_dir_from_cache_dir(cache_dir);
        std::fs::create_dir_all(&artifact_dir).ok();

        // Artifact loading is deferred to a background task in run() so the
        // daemon starts accepting connections immediately (Bug 6 fix).
        let artifacts: DashMap<String, CachedArtifact> = DashMap::new();

        // Open the bincode-backed artifact index for fast startup + persistence.
        let index_path = crate::core::config::index_path_from_cache_dir(cache_dir);
        let artifact_store = ArtifactStore::open(&index_path).map_err(|e| {
            crate::ipc::IpcError::Io(std::io::Error::other(format!(
                "failed to open artifact index at {}: {e}",
                index_path.display()
            )))
        })?;
        let artifact_store = Arc::new(artifact_store);

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
        let system_includes_loaded = match crate::depgraph::SystemIncludeCache::load_from_disk(
            system_includes_cache_path.as_path(),
        ) {
            Ok(cache) => cache,
            Err(e) => {
                tracing::warn!(
                    path = %system_includes_cache_path.display(),
                    "failed to load system include cache, starting empty: {e}"
                );
                crate::depgraph::SystemIncludeCache::new()
            }
        };
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
        let cache_system =
            match crate::fscache::MetadataCache::load_from_disk(metadata_path.as_path()) {
                Ok(metadata) => {
                    let loaded = metadata.len();
                    if loaded > 0 {
                        tracing::info!(
                            loaded,
                            path = %metadata_path.display(),
                            "metadata cache restored from disk"
                        );
                    }
                    CacheSystem::with_metadata(metadata)
                }
                Err(e) => {
                    tracing::warn!(
                        path = %metadata_path.display(),
                        "failed to load metadata cache, starting empty: {e}"
                    );
                    CacheSystem::new()
                }
            };

        Ok(Self {
            listener,
            shutdown: Arc::clone(&shutdown),
            index_writer_rx: Some(index_writer_rx),
            state: Arc::new(SharedState {
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
                request_validation_cache: DashMap::new(),
                compiler_hash_cache,
                watched_raw_dirs: DashMap::new(),
                pch_source_map: DashMap::new(),
                journal: CompileJournal::new(crate::core::config::log_dir_from_cache_dir(
                    cache_dir,
                )),
                in_flight_bytes: AtomicUsize::new(0),
                persist_semaphore: Arc::new(tokio::sync::Semaphore::new(persist_workers_default())),
                artifact_store,
                index_writer_tx,
                index_writer_shutdown,
                artifacts_loaded: AtomicBool::new(false),
                compiler_hash_cache_loaded: AtomicBool::new(false),
                shutdown_event_logged: AtomicBool::new(false),
                fingerprint: FingerprintManager::new(),
                dep_graph_persisted: AtomicBool::new(false),
                depgraph_load_warning: Mutex::new(None),
                in_flight_exec: DashMap::new(),
                pending_cache_writes: DashMap::new(),
            }),
        })
    }

    /// Get a handle to signal shutdown.
    #[must_use]
    pub fn shutdown_handle(&self) -> Arc<Notify> {
        Arc::clone(&self.shutdown)
    }

    /// Clone the running-process identity served by this daemon.
    #[must_use]
    pub fn backend_identity(&self) -> running_process::broker::backend_handle::DaemonProcess {
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
        let mut guard = self.state.depgraph_load_warning.blocking_lock();
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
            let mut guard = self.state.depgraph_load_warning.blocking_lock();
            *guard = Some(warning);
        }
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
