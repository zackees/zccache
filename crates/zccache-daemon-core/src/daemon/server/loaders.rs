//! Deferred cache-load handles (#640 / #784): the `DepGraphSetter` and the
//! four `*Loader` types that hydrate on-disk snapshots into the running
//! daemon *after* bind, plus the `DaemonServer` factory methods that hand
//! them out. Split out of `lifecycle.rs` to keep files under 1k LOC.
//!
//! Startup ordering these implement: bind the IPC pipe + write the readiness
//! lockfile FIRST, then load each DB snapshot in a `spawn_blocking` task and
//! publish it via `ArcSwap` + `Notify` + the `*_loaded` `AtomicBool` gates,
//! so a request that races the load takes a miss (never a wrong-hit).

use super::*;

impl DaemonServer {
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
