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
    /// configured cache directory (resolved via [`zccache_monocrate::core::config::default_cache_dir`]).
    ///
    /// Production callers should use this. Tests that need to isolate their
    /// cache directory must use [`Self::bind_with_cache_dir`] instead — this
    /// reads `ZCCACHE_CACHE_DIR` from a process-global env, which races when
    /// multiple tests run in parallel.
    pub fn bind(endpoint: &str) -> Result<Self, zccache_monocrate::ipc::IpcError> {
        Self::bind_with_cache_dir(endpoint, &zccache_monocrate::core::config::default_cache_dir())
    }

    /// Create a new daemon server bound to the given endpoint, rooted at an
    /// explicit cache directory. Bypasses the `ZCCACHE_CACHE_DIR` env var so
    /// parallel tests can each operate in isolation.
    pub fn bind_with_cache_dir(
        endpoint: &str,
        cache_dir: &zccache_monocrate::core::NormalizedPath,
    ) -> Result<Self, zccache_monocrate::ipc::IpcError> {
        let listener = IpcListener::bind(endpoint)?;
        let shutdown = Arc::new(Notify::new());
        let now = now_secs();
        let instance = SERVER_INSTANCE.fetch_add(1, Ordering::Relaxed);
        let artifact_dir = zccache_monocrate::core::config::artifacts_dir_from_cache_dir(cache_dir);
        std::fs::create_dir_all(&artifact_dir).ok();

        // Artifact loading is deferred to a background task in run() so the
        // daemon starts accepting connections immediately (Bug 6 fix).
        let artifacts: DashMap<String, CachedArtifact> = DashMap::new();

        // Open the bincode-backed artifact index for fast startup + persistence.
        let index_path = zccache_monocrate::core::config::index_path_from_cache_dir(cache_dir);
        let artifact_store = ArtifactStore::open(&index_path).map_err(|e| {
            zccache_monocrate::ipc::IpcError::Io(std::io::Error::other(format!(
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
        let metadata_path = zccache_monocrate::core::config::metadata_path_from_cache_dir(cache_dir);
        let cache_system =
            match zccache_fscache::MetadataCache::load_from_disk(metadata_path.as_path()) {
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
                sessions: SessionManager::new(std::time::Duration::from_secs(300)),
                system_includes: Mutex::new(SystemIncludeCache::new()),
                dep_graph: DepGraph::new(),
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
                depfile_tmpdir: {
                    let dir = zccache_monocrate::core::config::depfile_dir_from_cache_dir(cache_dir)
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
                compiler_hash_cache: CompilerHashCache::new(),
                watched_raw_dirs: DashMap::new(),
                pch_source_map: DashMap::new(),
                journal: CompileJournal::new(zccache_monocrate::core::config::log_dir_from_cache_dir(
                    cache_dir,
                )),
                in_flight_bytes: AtomicUsize::new(0),
                persist_semaphore: Arc::new(tokio::sync::Semaphore::new(persist_workers_default())),
                artifact_store,
                index_writer_tx,
                index_writer_shutdown,
                artifacts_loaded: AtomicBool::new(false),
                fingerprint: FingerprintManager::new(),
                dep_graph_persisted: AtomicBool::new(false),
                depgraph_load_warning: Mutex::new(None),
            }),
        })
    }

    /// Get a handle to signal shutdown.
    #[must_use]
    pub fn shutdown_handle(&self) -> Arc<Notify> {
        Arc::clone(&self.shutdown)
    }

    /// Replace the dependency graph with a pre-loaded one.
    ///
    /// Must be called before `run()` (while this is the only Arc holder).
    /// Marks the graph as persisted because it was restored from disk.
    pub fn set_dep_graph(&mut self, graph: zccache_depgraph::DepGraph) {
        let state =
            Arc::get_mut(&mut self.state).expect("set_dep_graph must be called before run()");
        state.dep_graph = graph;
        state.dep_graph_persisted.store(true, Ordering::Release);
    }

    /// Record a load-time depgraph warning to mirror into per-session logs.
    ///
    /// Called by the daemon's startup path after [`zccache_depgraph::classify_load`]
    /// returns a non-`Loaded` outcome that warrants surfacing (version
    /// mismatch, corruption, I/O error). The warning is appended to each
    /// session's log file at `SessionStart` so a cold fallback caused by a
    /// stale or corrupt `depgraph.bin` is visible to operators. Issue #320.
    ///
    /// Must be called before `run()` (while this is the only `Arc` holder).
    pub fn set_depgraph_load_warning(&mut self, warning: String) {
        let state = Arc::get_mut(&mut self.state)
            .expect("set_depgraph_load_warning must be called before run()");
        *state.depgraph_load_warning.get_mut() = Some(warning);
    }

    /// Get a snapshot of the phase profiler (for benchmarks).
    #[must_use]
    pub fn profile_snapshot(&self) -> crate::stats::ProfileSnapshot {
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
}
