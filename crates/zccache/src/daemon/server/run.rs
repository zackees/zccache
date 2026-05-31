//! `DaemonServer::run` — the daemon's main loop, plus the file-watcher
//! pipeline initializer it kicks off.
//!
//! Owns startup-side cleanup of legacy state, the four background tasks
//! (artifact load, memory eviction, disk GC, depgraph save), and the
//! shutdown drain that persists artifact-store, depgraph, and metadata
//! caches to disk.

use super::*;

impl DaemonServer {
    /// Run the server, accepting connections until shutdown is signaled.
    ///
    /// `idle_timeout_secs`: if non-zero, the daemon shuts down after this many
    /// seconds with no client activity. Pass 0 to disable.
    pub async fn run(&mut self, idle_timeout_secs: u64) -> Result<(), crate::ipc::IpcError> {
        tracing::info!(
            persist_workers = self.state.persist_semaphore.available_permits(),
            "daemon server running"
        );

        // Background index-writer task: in-memory WAL with timer-driven
        // flushing. See `run_index_writer` for the design rationale.
        let mut index_writer_handle: Option<tokio::task::JoinHandle<()>> = None;
        if let Some(rx) = self.index_writer_rx.take() {
            let store = Arc::clone(&self.state.artifact_store);
            let shutdown = Arc::clone(&self.state.index_writer_shutdown);
            index_writer_handle = Some(tokio::spawn(run_index_writer(rx, store, shutdown)));
        }

        let cache_dir = self.state.cache_dir.clone();
        let temp_root = std::env::temp_dir();

        // Clean up legacy log backup directory (Bug 7).
        {
            let legacy_logs = cache_dir.join("logs.bak");
            if legacy_logs.is_dir() {
                match std::fs::remove_dir_all(&legacy_logs) {
                    Ok(()) => tracing::info!("removed legacy logs.bak directory"),
                    Err(e) => tracing::warn!(
                        path = %legacy_logs.display(),
                        "failed to remove legacy logs.bak: {e}"
                    ),
                }
            }
            // Also remove stale daemon.lock.bak if present.
            let legacy_lock = cache_dir.join("daemon.lock.bak");
            let _ = std::fs::remove_file(&legacy_lock);
        }

        // Remove legacy temp-root state from older builds before starting the daemon.
        {
            let cleaned = crate::core::config::cleanup_legacy_temp_root_state(
                &temp_root,
                &cache_dir,
                crate::ipc::is_process_alive,
            );
            if cleaned > 0 {
                tracing::info!(cleaned, "cleaned legacy temp-root zccache state");
            }
        }

        // Clean up stale depfile directories from dead daemon instances.
        {
            let cleaned =
                crate::core::config::cleanup_stale_depfile_dirs(crate::ipc::is_process_alive);
            if cleaned > 0 {
                tracing::info!(cleaned, "cleaned stale depfile directories");
            }
        }

        self.start_watcher_pipeline().await;

        // Start idle watchdog if timeout is configured.
        if idle_timeout_secs > 0 {
            let state = Arc::clone(&self.state);
            let timeout = idle_timeout_secs;
            tokio::spawn(async move {
                loop {
                    tokio::time::sleep(std::time::Duration::from_secs(60)).await;
                    let last = state.last_activity.load(Ordering::Relaxed);
                    let idle = now_secs().saturating_sub(last);
                    if idle >= timeout {
                        tracing::info!(idle_secs = idle, "idle timeout — shutting down");
                        // Persist a "died-idle" lifecycle event so operators
                        // can see why the daemon exited. Pair this with the
                        // "spawn" entry to reconstruct daemon lifetime from
                        // the lifecycle log alone — tracing stderr is NUL'd.
                        super::super::lifecycle::write_event(
                            super::super::lifecycle::EVENT_DIED_IDLE,
                            serde_json::json!({
                                "reason": super::super::lifecycle::REASON_IDLE_TIMEOUT,
                                "idle_secs": idle,
                                "idle_timeout_secs": timeout,
                            }),
                        );
                        state.shutdown.notify_one();
                        break;
                    }
                }
            });
        }

        // Private daemons are owned by caller-supplied PIDs. Once the last
        // live owner disappears, shut down even if the normal idle timeout is
        // disabled or still far in the future.
        {
            let state = Arc::clone(&self.state);
            tokio::spawn(async move {
                loop {
                    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
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
                        super::super::lifecycle::write_event(
                            "died-private-owner-exit",
                            serde_json::json!({
                                "reason": "private-owner-pids-exited",
                                "uptime_secs": now_secs().saturating_sub(state.start_time),
                                "removed_pids": prune.removed_pids,
                            }),
                        );
                        state.shutdown.notify_one();
                        break;
                    }
                }
            });
        }

        // Start background artifact loading (non-blocking so daemon responds
        // immediately — Bug 6 fix).
        {
            let state = Arc::clone(&self.state);
            let state2 = Arc::clone(&self.state);
            tokio::spawn(async move {
                let artifact_dir = state.artifact_dir.clone();
                let artifacts = state.artifacts.clone();
                let state_ref = Arc::clone(&state);
                let loaded = tokio::task::spawn_blocking(move || {
                    // Load the in-memory index that `ArtifactStore::open` already
                    // hydrated from the on-disk blob.
                    let entries = state_ref.artifact_store.load_all();
                    if !entries.is_empty() {
                        let count = entries.len();
                        for (key, meta) in entries {
                            artifacts.insert(key, CachedArtifact::from_index(meta));
                        }
                        count
                    } else {
                        // Migration: legacy `.meta` files predate the redb index
                        // and the current bincode blob; populate the live store
                        // from them so the first session after upgrade still has
                        // its warm cache.
                        migrate_meta_files(&artifact_dir, &artifacts, &state_ref.artifact_store)
                    }
                })
                .await
                .unwrap_or(0);
                if loaded > 0 {
                    tracing::info!(loaded, "background artifact loading complete");
                }
                state2.artifacts_loaded.store(true, Ordering::Release);
            });
        }

        // Start memory eviction background task.
        {
            let state = Arc::clone(&self.state);
            let budget = crate::core::config::Config::default().max_memory_bytes;
            let interval_secs = crate::core::config::Config::default().eviction_interval_secs;
            tokio::spawn(async move {
                loop {
                    tokio::time::sleep(std::time::Duration::from_secs(interval_secs)).await;
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
                    let (freed, items) = super::super::eviction::evict_to_budget(
                        budget,
                        &state.cache_system,
                        &state.dep_graph,
                        &state.fast_hit_cache,
                        &state.artifacts,
                        state.in_flight_bytes.load(Ordering::Relaxed),
                    );
                    if items > 0 {
                        tracing::info!(
                            freed_bytes = freed,
                            items_removed = items,
                            "memory eviction"
                        );
                    }
                }
            });
        }

        // Start disk artifact GC background task.
        {
            let state = Arc::clone(&self.state);
            let max_cache_size = crate::core::config::Config::default().max_cache_size;
            let interval_secs = crate::core::config::Config::default().disk_gc_interval_secs;
            tokio::spawn(async move {
                // Run once immediately at startup to reclaim excess disk from Bug 5.
                {
                    let dir = state.artifact_dir.clone();
                    let artifacts = state.artifacts.clone();
                    let result = tokio::task::spawn_blocking(move || {
                        super::super::eviction::evict_disk_artifacts(
                            &dir,
                            &artifacts,
                            max_cache_size,
                        )
                    })
                    .await;
                    if let Ok((freed, removed)) = result {
                        if removed > 0 {
                            tracing::info!(
                                freed_bytes = freed,
                                artifacts_removed = removed,
                                "initial disk GC"
                            );
                        }
                    }
                }
                loop {
                    tokio::time::sleep(std::time::Duration::from_secs(interval_secs)).await;
                    let dir = state.artifact_dir.clone();
                    let artifacts = state.artifacts.clone();
                    let result = tokio::task::spawn_blocking(move || {
                        super::super::eviction::evict_disk_artifacts(
                            &dir,
                            &artifacts,
                            max_cache_size,
                        )
                    })
                    .await;
                    if let Ok((freed, removed)) = result {
                        if removed > 0 {
                            tracing::info!(
                                freed_bytes = freed,
                                artifacts_removed = removed,
                                "disk GC"
                            );
                        }
                    }
                }
            });
        }

        // Start periodic depgraph save task (every 5 minutes).
        {
            let state = Arc::clone(&self.state);
            tokio::spawn(async move {
                loop {
                    tokio::time::sleep(std::time::Duration::from_secs(300)).await;
                    let path = crate::depgraph::depgraph_file_path();
                    if let Some(parent) = path.parent() {
                        std::fs::create_dir_all(parent).ok();
                    }
                    match crate::depgraph::save_to_file(&state.dep_graph, &path) {
                        Ok(()) => {
                            state.dep_graph_persisted.store(true, Ordering::Release);
                            tracing::debug!("periodic depgraph save");
                        }
                        Err(e) => tracing::warn!("periodic depgraph save failed: {e}"),
                    }
                }
            });
        }

        loop {
            tokio::select! {
                result = self.listener.accept() => {
                    let conn = match result {
                        Ok(c) => c,
                        Err(e) => {
                            tracing::error!("accept failed, continuing: {e}");
                            continue;
                        }
                    };
                    let state = Arc::clone(&self.state);
                    tokio::spawn(async move {
                        if let Err(e) = handle_connection(conn, state).await {
                            tracing::warn!("connection error: {e}");
                        }
                    });
                }
                () = self.shutdown.notified() => {
                    tracing::info!("daemon server shutting down");
                    // Drop the watcher to stop the OS thread and close channels.
                    // The settle buffer and consumer tasks will exit when their
                    // input channels close.
                    *self.state.watcher.lock().await = None;

                    // Signal the index-writer to drain its WAL to disk, then
                    // wait briefly for it. Without this, unflushed entries are
                    // lost if the runtime aborts before the next interval tick.
                    self.state.index_writer_shutdown.notify_waiters();
                    if let Some(h) = index_writer_handle.take() {
                        let _ = tokio::time::timeout(
                            std::time::Duration::from_secs(2),
                            h,
                        )
                        .await;
                    }

                    // Critical: the WAL drain above only persists entries that
                    // went through `index_writer_tx`. The compile-success path
                    // at server.rs:6122 (and friends) inserts DIRECTLY into
                    // `artifact_store` without sending to the WAL, and
                    // `flush_wal_to_disk` early-returns on an empty WAL —
                    // so those direct-inserts never reach disk on a
                    // WAL-only-empty shutdown. Reproduced locally: a fresh
                    // medium-fixture build wrote 271 MB of CAS payloads
                    // but no index.bin, leaving the warm-side daemon (and
                    // every other `soldr load` consumer) with an empty index
                    // even though all artifacts are on disk.
                    //
                    // Force a final `store.flush()` here so the in-memory
                    // DashMap snapshot lands on disk regardless of WAL state.
                    // spawn_blocking keeps the synchronous I/O off the
                    // runtime; the await is bounded by the same 2s pattern
                    // as the WAL drain above.
                    let store = Arc::clone(&self.state.artifact_store);
                    let entries = store.len();
                    let flush_start = std::time::Instant::now();
                    let res = tokio::task::spawn_blocking(move || store.flush()).await;
                    match res {
                        Ok(Ok(())) => tracing::info!(
                            entries,
                            elapsed_ms = flush_start.elapsed().as_millis() as u64,
                            "artifact store final flush complete"
                        ),
                        Ok(Err(e)) => tracing::warn!(
                            entries,
                            "artifact store final flush failed: {e}"
                        ),
                        Err(e) => tracing::warn!(
                            entries,
                            "artifact store final flush task join error: {e}"
                        ),
                    }

                    // Save depgraph to disk before exiting.
                    let start = std::time::Instant::now();
                    let path = crate::depgraph::depgraph_file_path();
                    if let Some(parent) = path.parent() {
                        std::fs::create_dir_all(parent).ok();
                    }
                    let (cold_ctxs, warm_ctxs, stale_ctxs) =
                        self.state.dep_graph.state_breakdown();
                    let ctxs_with_key = self.state.dep_graph.contexts_with_artifact_key();
                    match crate::depgraph::save_to_file(&self.state.dep_graph, &path) {
                        Ok(()) => {
                            self.state
                                .dep_graph_persisted
                                .store(true, Ordering::Release);
                            // State breakdown lets a future warm-side daemon
                            // explain its cold_skip miss rate: if cold_ctxs
                            // is high relative to warm_ctxs, the warm side
                            // will take the cold_skip branch for those keys
                            // and never consult the artifact_store.
                            tracing::info!(
                                elapsed_ms = start.elapsed().as_millis() as u64,
                                cold = cold_ctxs,
                                warm = warm_ctxs,
                                stale = stale_ctxs,
                                with_artifact_key = ctxs_with_key,
                                "depgraph saved"
                            );
                        }
                        Err(e) => tracing::warn!("depgraph save failed: {e}"),
                    }

                    // Persist the in-memory MetadataCache so the next
                    // daemon (in particular the warm side of soldr
                    // save/load) starts with its fast path populated.
                    // Failure here is a perf regression, not a
                    // correctness bug — log and move on so shutdown
                    // never hangs on disk I/O.
                    let meta_start = std::time::Instant::now();
                    let metadata_entries = self.state.cache_system.metadata().len();
                    match self
                        .state
                        .cache_system
                        .metadata()
                        .save_to_disk(self.state.metadata_path.as_path())
                    {
                        Ok(()) => {
                            if metadata_entries > 0 {
                                tracing::info!(
                                    entries = metadata_entries,
                                    elapsed_ms = meta_start.elapsed().as_millis() as u64,
                                    "metadata cache persisted"
                                );
                            }
                        }
                        Err(e) => tracing::warn!(
                            path = %self.state.metadata_path.display(),
                            "metadata cache save failed: {e}"
                        ),
                    }

                    // Issue #517: persist the compiler-binary hash cache
                    // so the next daemon does not pay the ~50-60 ms cold
                    // blake3 over rustc on its first compile.
                    if let Err(e) = self
                        .state
                        .compiler_hash_cache
                        .save_to_disk(self.state.compiler_hash_cache_path.as_path())
                    {
                        tracing::warn!(
                            path = %self.state.compiler_hash_cache_path.display(),
                            "compiler hash cache save failed: {e}"
                        );
                    }

                    // Clean up our own depfile temp directory.
                    let _ = std::fs::remove_dir_all(&self.state.depfile_tmpdir);

                    return Ok(());
                }
            }
        }
    }

    /// Initialize the file watcher pipeline:
    /// `NotifyWatcher (OS thread) → SettleBuffer (tokio task) → CacheSystem consumer (tokio task)`
    async fn start_watcher_pipeline(&self) {
        let ignore = Arc::new(crate::watcher::IgnoreFilter::default());
        let (watcher, raw_rx) = match NotifyWatcher::new(ignore) {
            Ok(w) => w,
            Err(e) => {
                tracing::warn!("failed to start file watcher: {e} — running without watcher");
                return;
            }
        };

        *self.state.watcher.lock().await = Some(watcher);
        self.state.watcher_active.store(true, Ordering::Release);

        // Settle buffer: coalesces raw events into batches after a quiet period.
        let (settled_tx, mut settled_rx) = tokio::sync::mpsc::unbounded_channel();
        let settle = SettleBuffer::default_window();
        tokio::spawn(async move {
            settle.run(raw_rx, settled_tx).await;
        });

        // Consumer: feeds settled events into CacheSystem for metadata invalidation.
        let state = Arc::clone(&self.state);
        tokio::spawn(async move {
            while let Some(event) = settled_rx.recv().await {
                match event {
                    SettledEvent::Batch { changed, removed } => {
                        let count = changed.len() + removed.len();
                        if count > 0 {
                            tracing::debug!(
                                changed = changed.len(),
                                removed = removed.len(),
                                "watcher batch applied"
                            );
                            // On Windows, notify reports paths with \\?\
                            // extended-length prefix but the rest of the
                            // codebase uses plain paths. Strip the prefix
                            // so journal/metadata lookups match.
                            #[cfg(windows)]
                            let (changed, removed) = {
                                let strip = |paths: Vec<NormalizedPath>| -> Vec<NormalizedPath> {
                                    paths
                                        .into_iter()
                                        .map(|p| {
                                            let s = p.to_string_lossy();
                                            if let Some(stripped) = s.strip_prefix(r"\\?\") {
                                                stripped.into()
                                            } else {
                                                p
                                            }
                                        })
                                        .collect()
                                };
                                (strip(changed), strip(removed))
                            };
                            #[cfg(debug_assertions)]
                            for p in changed.iter().chain(removed.iter()) {
                                debug_assert!(
                                    !p.to_string_lossy().starts_with(r"\\?\"),
                                    "watcher path must not have \\\\?\\ prefix: {}",
                                    p.display()
                                );
                            }
                            state.fingerprint.on_batch(&changed, &removed);
                            state
                                .cache_system
                                .apply_changes_with_removals(changed, removed);
                        }
                    }
                    SettledEvent::Overflow => {
                        tracing::warn!("watcher overflow — downgrading all metadata");
                        state.cache_system.apply_overflow();
                    }
                }
            }
            tracing::debug!("watcher consumer task exiting");
        });

        tracing::info!("file watcher pipeline started");
    }
}
