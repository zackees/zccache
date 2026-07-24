//! `DaemonServer::run` — the daemon's main loop, plus the file-watcher
//! pipeline initializer it kicks off.
//!
//! Owns startup-side cleanup of legacy state, the four background tasks
//! (artifact load, memory eviction, disk GC, depgraph save), and the
//! shutdown drain that persists artifact-store, depgraph, and metadata
//! caches to disk.

use super::*;

const ACCEPT_STALL_WATCHDOG_INTERVAL: Duration = Duration::from_secs(600);
const MEMORY_GC_IDLE_GRACE: Duration = Duration::from_millis(250);
const MEMORY_GC_GENTLE_RETRY: Duration = Duration::from_millis(50);
const MEMORY_GC_FORCE_AFTER: Duration = Duration::from_secs(5);
const MEMORY_GC_GENTLE_BATCH: usize = 256;

/// Shutdown budget for the deterministic index-writer WAL drain (#1161).
/// Matches the embedded engine's 30 s flush bound (`embedded.rs`); a full
/// WAL flush snapshots the whole in-memory index to disk, which can take
/// seconds under I/O contention on a small (2-core CI) host.
const INDEX_WRITER_SHUTDOWN_DRAIN_TIMEOUT: Duration = Duration::from_secs(30);

/// Shutdown budget for joining the index-writer task after a successful
/// drain. The drain ack proves the WAL is already empty and flushed, so the
/// join is normally instantaneous; the bound only guards a wedged task.
const INDEX_WRITER_SHUTDOWN_JOIN_TIMEOUT: Duration = Duration::from_secs(5);

impl DaemonServer {
    /// Run the server, accepting connections until shutdown is signaled.
    ///
    /// `idle_timeout_secs`: if non-zero, the daemon shuts down after this many
    /// seconds with no client activity. Pass 0 to disable.
    pub async fn run(&mut self, idle_timeout_secs: u64) -> Result<(), crate::ipc::IpcError> {
        let maintenance_policy =
            MaintenancePolicy::from_env().map_err(crate::ipc::IpcError::Endpoint)?;
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

        // Migrate legacy blob digests once per cache root. Keep this off the
        // Tokio runtime thread because the helper hashes files synchronously.
        // The marker is published only after a successful migration so a
        // failed attempt remains retryable on the next daemon start.
        {
            let cache_dir = cache_dir.clone();
            let artifact_dir = self.state.artifact_dir.clone();
            let state = Arc::clone(&self.state);
            tokio::spawn(async move {
                if let Err(error) = state.staging.cleanup_abandoned() {
                    tracing::debug!(%error, "abandoned private staging cleanup skipped");
                }
                if let Err(error) = cleanup_staged_artifact_temps(&artifact_dir) {
                    tracing::debug!(%error, "staged artifact temp cleanup skipped");
                }
                let marker = cache_dir.join(".legacy-blob-digests-migrated-v1");
                if marker.exists() {
                    tracing::debug!(path = %marker.display(), "legacy blob digest migration already complete");
                    return;
                }

                let migration_root = artifact_dir;
                let result = tokio::task::spawn_blocking(move || {
                    let migrated = migrate_legacy_blob_digests(&migration_root)?;
                    use std::io::Write;
                    let mut marker_file = std::fs::OpenOptions::new()
                        .write(true)
                        .create_new(true)
                        .open(&marker)?;
                    marker_file.write_all(b"v1\n")?;
                    marker_file.sync_all()?;
                    Ok::<usize, std::io::Error>(migrated)
                })
                .await;

                match result {
                    Ok(Ok(migrated)) => {
                        tracing::info!(migrated, "legacy blob digest migration complete")
                    }
                    Ok(Err(error)) => tracing::warn!(
                        path = %cache_dir.display(),
                        "legacy blob digest migration failed: {error}"
                    ),
                    Err(error) => {
                        tracing::warn!("legacy blob digest migration task failed: {error}")
                    }
                }
            });
        }

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
                        state.shutdown_requested.store(true, Ordering::Release);
                        state.shutdown.notify_waiters();
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
            });
        }

        // Start background artifact loading (non-blocking so daemon responds
        // immediately — Bug 6 fix).
        {
            std::mem::drop(spawn_artifact_loader(Arc::clone(&self.state), None).await);
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
                    let (freed, items) = run_memory_eviction_pass(&state, budget).await;
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

        // The daemon is the primary maintenance owner. It checks this exact
        // cache root at startup and every five minutes, with persisted daily
        // full-age catch-up after idle periods or restarts (issue #1148).
        let mut maintenance_handle = Some(spawn_disk_maintenance(
            Arc::clone(&self.state),
            maintenance_policy,
            None,
        ));

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
                    let dg = state.dep_graph.load();
                    match crate::depgraph::save_to_file(&dg, &path) {
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
                () = tokio::time::sleep(ACCEPT_STALL_WATCHDOG_INTERVAL) => {
                    tracing::warn!(
                        stall_secs = ACCEPT_STALL_WATCHDOG_INTERVAL.as_secs(),
                        "daemon accept loop has not accepted a connection within watchdog interval"
                    );
                }
                () = self.shutdown.notified() => {
                    self.state.shutdown_requested.store(true, Ordering::Release);
                    // `shutdown_handle()` exposes the raw Notify for legacy
                    // tests and Ctrl+C handlers, many of which still call
                    // notify_one(). Rebroadcast so whichever task observes the
                    // edge first wakes the rest of the shutdown path.
                    self.state.shutdown.notify_waiters();
                    if let Some(handle) = maintenance_handle.take() {
                        let _ = handle.await;
                    }
                    tracing::info!("daemon server shutting down");
                    // Drop the watcher to stop the OS thread and close channels.
                    // The settle buffer and consumer tasks will exit when their
                    // input channels close.
                    match tokio::time::timeout(
                        Duration::from_secs(5),
                        self.state.watcher.lock(),
                    )
                    .await
                    {
                        Ok(mut watcher) => {
                            *watcher = None;
                            self.state.watcher_active.store(false, Ordering::Release);
                        }
                        Err(_) => {
                            tracing::warn!(
                                "timed out acquiring watcher lock during shutdown; proceeding"
                            );
                        }
                    }

                    // Deferred rustc/C++ persist tasks publish their durable
                    // `ArtifactIndex` rows only after the cache files land on
                    // disk. Wait for those tasks before draining the WAL;
                    // otherwise shutdown can save a warm depgraph whose
                    // artifact keys have not reached index.bin yet (#799).
                    let pending_drained = pending_writes::await_all(
                        &self.state.pending_cache_writes,
                        std::time::Duration::from_secs(30),
                    )
                    .await;
                    if !pending_drained {
                        tracing::warn!(
                            pending = self.state.pending_cache_writes.len(),
                            "timed out waiting for pending artifact writes before WAL drain"
                        );
                    }

                    // Every detached publisher receives an owned read guard
                    // before its request handler returns. Wait for those
                    // handoffs before stopping the WAL writer and performing
                    // the final index snapshot.
                    let _publication_guard =
                        self.state.artifact_publication.write().await;

                    // Deterministically drain the index-writer WAL BEFORE
                    // stopping the task (#1161). The Flush command is
                    // FIFO-ordered behind every `IndexWriterCommand::Insert`
                    // already queued — and no new row can arrive because the
                    // publication write guard above blocks all publishers —
                    // so its acknowledgement proves every queued durable-index
                    // row has been applied to the in-memory store AND
                    // snapshotted to disk. Mirrors the embedded engine's
                    // flush-then-stop sequence (`embedded.rs`). The standalone
                    // daemon previously went straight to notify + 2 s join +
                    // silent `abort()`, which on a slow 2-core host could
                    // abort the writer mid-drain and lose queued rows — the
                    // warm daemon after restart then misses the artifacts the
                    // cold daemon had already persisted (observed as the
                    // Integration `legacy_path_validation` warm-multi miss).
                    let drain_start = std::time::Instant::now();
                    let index_writer_drained = flush_index_writer(
                        &self.state.index_writer_tx,
                        INDEX_WRITER_SHUTDOWN_DRAIN_TIMEOUT,
                    )
                    .await;
                    if !index_writer_drained {
                        // Loud-forensics rule: a shutdown-budget breach emits
                        // BOTH a tracing::warn! AND a durable lifecycle event.
                        tracing::warn!(
                            event = crate::core::lifecycle::EVENT_EMBEDDED_FLUSH_STEP_TIMEOUT,
                            step = "index_writer_drain",
                            timeout_ms =
                                INDEX_WRITER_SHUTDOWN_DRAIN_TIMEOUT.as_millis() as u64,
                            elapsed_ms = drain_start.elapsed().as_millis() as u64,
                            "index-writer WAL drain did not acknowledge within its \
                             shutdown budget; queued durable-index rows may be lost"
                        );
                        crate::core::lifecycle::write_event_in_cache_root(
                            self.state.cache_dir.as_path(),
                            crate::core::lifecycle::EVENT_EMBEDDED_FLUSH_STEP_TIMEOUT,
                            serde_json::json!({
                                "step": "index_writer_drain",
                                "timeout_ms":
                                    INDEX_WRITER_SHUTDOWN_DRAIN_TIMEOUT.as_millis() as u64,
                                "reason": "shutdown WAL drain ack timed out; durable \
                                           index rows queued behind the flush may be lost",
                            }),
                        );
                    }

                    // Stop the writer task. After a successful drain the WAL
                    // is empty and this join is instantaneous; the bound only
                    // guards a wedged task. `notify_one` retains a permit if
                    // the writer is between polls; `notify_waiters` could lose
                    // the signal in that window.
                    self.state.index_writer_shutdown.notify_one();
                    if let Some(mut handle) = index_writer_handle.take() {
                        if tokio::time::timeout(
                            INDEX_WRITER_SHUTDOWN_JOIN_TIMEOUT,
                            &mut handle,
                        )
                        .await
                        .is_err()
                        {
                            tracing::warn!(
                                event = crate::core::lifecycle::EVENT_EMBEDDED_FLUSH_STEP_TIMEOUT,
                                step = "index_writer_join",
                                timeout_ms =
                                    INDEX_WRITER_SHUTDOWN_JOIN_TIMEOUT.as_millis() as u64,
                                "index-writer task did not exit within its shutdown \
                                 budget; aborting it"
                            );
                            crate::core::lifecycle::write_event_in_cache_root(
                                self.state.cache_dir.as_path(),
                                crate::core::lifecycle::EVENT_EMBEDDED_FLUSH_STEP_TIMEOUT,
                                serde_json::json!({
                                    "step": "index_writer_join",
                                    "timeout_ms":
                                        INDEX_WRITER_SHUTDOWN_JOIN_TIMEOUT.as_millis() as u64,
                                    "reason": "index-writer task join timed out after a \
                                               drain attempt; task aborted",
                                }),
                            );
                            handle.abort();
                            let _ = handle.await;
                        }
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
                    let res = store.flush_async().await;
                    match res {
                        Ok(()) => tracing::info!(
                            entries,
                            elapsed_ms = flush_start.elapsed().as_millis() as u64,
                            "artifact store final flush complete"
                        ),
                        Err(e) => tracing::warn!(
                            entries,
                            "artifact store final flush failed: {e}"
                        ),
                    }

                    // Save depgraph to disk before exiting. The serializer and
                    // atomic write path are synchronous, so run them off the
                    // Tokio runtime thread.
                    let start = std::time::Instant::now();
                    let path = crate::depgraph::depgraph_file_path();
                    let dg = self.state.dep_graph.load_full();
                    let depgraph_save = tokio::task::spawn_blocking(move || {
                        if let Some(parent) = path.parent() {
                            std::fs::create_dir_all(parent).ok();
                        }
                        let (cold_ctxs, warm_ctxs, stale_ctxs) = dg.state_breakdown();
                        let ctxs_with_key = dg.contexts_with_artifact_key();
                        let result = crate::depgraph::save_to_file(&dg, &path);
                        (result, cold_ctxs, warm_ctxs, stale_ctxs, ctxs_with_key)
                    })
                    .await;
                    match depgraph_save {
                        Ok((Ok(()), cold_ctxs, warm_ctxs, stale_ctxs, ctxs_with_key)) => {
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
                        Ok((Err(e), _, _, _, _)) => tracing::warn!("depgraph save failed: {e}"),
                        Err(e) => tracing::warn!("depgraph save task join error: {e}"),
                    }

                    // Persist the in-memory MetadataCache so the next
                    // daemon (in particular the warm side of soldr
                    // save/load) starts with its fast path populated.
                    // Failure here is a perf regression, not a
                    // correctness bug — log and move on so shutdown
                    // never hangs on disk I/O.
                    //
                    // Issue #784 phase 2b: gate on `metadata_cache_loaded`.
                    // The disk load now runs in a background
                    // `spawn_blocking` after the readiness lockfile, so
                    // an early shutdown (Ctrl+C before the loader
                    // finishes) could otherwise save a partial snapshot
                    // over the on-disk file. Skipping the save when the
                    // load hasn't completed preserves the existing
                    // snapshot — the entries that DID land in-memory
                    // came from in-process compiles whose verified state
                    // is still on disk in the prior snapshot.
                    if self
                        .state
                        .metadata_cache_loaded
                        .load(Ordering::Acquire)
                    {
                        let meta_start = std::time::Instant::now();
                        let metadata_entries = self.state.cache_system.metadata().len();
                        let state = Arc::clone(&self.state);
                        let metadata_path = self.state.metadata_path.clone();
                        let res = tokio::task::spawn_blocking(move || {
                            state
                                .cache_system
                                .metadata()
                                .save_to_disk(metadata_path.as_path())
                        })
                        .await;
                        match res {
                            Ok(Ok(())) => {
                                if metadata_entries > 0 {
                                    tracing::info!(
                                        entries = metadata_entries,
                                        elapsed_ms = meta_start.elapsed().as_millis() as u64,
                                        "metadata cache persisted"
                                    );
                                }
                            }
                            Ok(Err(e)) => tracing::warn!(
                                path = %self.state.metadata_path.display(),
                                "metadata cache save failed: {e}"
                            ),
                            Err(e) => tracing::warn!(
                                path = %self.state.metadata_path.display(),
                                "metadata cache save task join error: {e}"
                            ),
                        }
                    } else {
                        tracing::debug!(
                            "metadata cache load still pending at shutdown — skipping save"
                        );
                    }

                    // Issue #517: persist the compiler-binary hash cache
                    // so the next daemon does not pay the ~50-60 ms cold
                    // blake3 over rustc on its first compile.
                    //
                    // Issue #784: gate on `compiler_hash_cache_loaded`.
                    // The disk load now runs in a background
                    // `spawn_blocking` after the readiness lockfile, so
                    // an early shutdown (Ctrl+C before the loader
                    // finishes) could otherwise save a partial snapshot
                    // over the on-disk file. Skipping the save when the
                    // load hasn't completed preserves the existing
                    // snapshot — the in-memory DashMap is still warm
                    // enough for the in-process compiles that already
                    // happened.
                    if self
                        .state
                        .compiler_hash_cache_loaded
                        .load(Ordering::Acquire)
                    {
                        let state = Arc::clone(&self.state);
                        let compiler_hash_cache_path = self.state.compiler_hash_cache_path.clone();
                        let res = tokio::task::spawn_blocking(move || {
                            state
                                .compiler_hash_cache
                                .save_to_disk(compiler_hash_cache_path.as_path())
                        })
                        .await;
                        match res {
                            Ok(Ok(())) => {}
                            Ok(Err(e)) => {
                                tracing::warn!(
                                    path = %self.state.compiler_hash_cache_path.display(),
                                    "compiler hash cache save failed: {e}"
                                );
                            }
                            Err(e) => {
                                tracing::warn!(
                                    path = %self.state.compiler_hash_cache_path.display(),
                                    "compiler hash cache save task join error: {e}"
                                );
                            }
                        }
                    } else {
                        tracing::debug!(
                            "compiler hash cache load still pending at shutdown — skipping save"
                        );
                    }

                    // Issue #541: persist the C/C++ system include paths
                    // so the next daemon does not pay the ~30-50 ms
                    // `<compiler> -v -E -x c++ NUL` spawn on its first
                    // C/C++ compile.
                    //
                    // Issue #784 phase 2c: gate on `system_includes_loaded`.
                    // The disk load now runs in a background
                    // `spawn_blocking` after the readiness lockfile, so
                    // an early shutdown could otherwise save a partial
                    // snapshot over the on-disk file. Skipping the save
                    // when the load hasn't completed preserves the
                    // existing snapshot — entries that DID land
                    // in-memory came from in-process compiles whose
                    // re-probe is cheap.
                    if self
                        .state
                        .system_includes_loaded
                        .load(Ordering::Acquire)
                    {
                        let includes = {
                            let includes = self.state.system_includes.lock().await;
                            includes.clone()
                        };
                        let system_includes_cache_path =
                            self.state.system_includes_cache_path.clone();
                        let res = tokio::task::spawn_blocking(move || {
                            includes.save_to_disk(system_includes_cache_path.as_path())
                        })
                        .await;
                        match res {
                            Ok(Ok(())) => {}
                            Ok(Err(e)) => {
                                tracing::warn!(
                                    path = %self.state.system_includes_cache_path.display(),
                                    "system include cache save failed: {e}"
                                );
                            }
                            Err(e) => {
                                tracing::warn!(
                                    path = %self.state.system_includes_cache_path.display(),
                                    "system include cache save task join error: {e}"
                                );
                            }
                        }
                    } else {
                        tracing::debug!(
                            "system include cache load still pending at shutdown — skipping save"
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
                set_registry_watcher_available(false);
                tracing::warn!("failed to start file watcher: {e} — running without watcher");
                return;
            }
        };

        match tokio::time::timeout(Duration::from_secs(5), self.state.watcher.lock()).await {
            Ok(mut watcher_guard) => {
                *watcher_guard = Some(watcher);
            }
            Err(_) => {
                set_registry_watcher_available(false);
                tracing::warn!(
                    "timed out acquiring watcher lock during startup; running without watcher"
                );
                return;
            }
        }
        set_registry_watcher_available(true);
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
            while !state.shutdown_requested.load(Ordering::Acquire) {
                // Race the settled-event recv against the shutdown signal
                // (issue #974). Without this the loop only re-checks
                // `shutdown_requested` AFTER `recv()` returns, so if the settle
                // task neither sends nor drops its sender the consumer parks
                // forever and ignores shutdown — a shutdown-cleanliness gap.
                let event = tokio::select! {
                    e = settled_rx.recv() => e,
                    () = state.shutdown.notified() => {
                        state.shutdown_requested.store(true, Ordering::Release);
                        // See the accept-loop shutdown branch: consuming a
                        // single Notify edge here must still wake the loop.
                        state.shutdown.notify_waiters();
                        None
                    },
                };
                let Some(event) = event else { break };
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
                            mark_registered_links_suspect(
                                changed.iter().map(|path| path.as_path()),
                            );
                            mark_removed_links_suspect(removed.iter().map(|path| path.as_path()));
                            state.fingerprint.on_batch(&changed, &removed);
                            state
                                .cache_system
                                .apply_changes_with_removals(changed, removed);
                        }
                    }
                    SettledEvent::Overflow => {
                        mark_all_registered_links_suspect();
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

async fn run_memory_eviction_pass(state: &Arc<SharedState>, budget: u64) -> (u64, usize) {
    let started = Instant::now();
    let mut total_freed = 0_u64;
    let mut total_items = 0_usize;
    let mut candidate_offset = 0_usize;
    let plan = {
        let dep_graph_guard = state.dep_graph.load();
        let plan = super::super::eviction::plan_eviction_to_budget(
            budget,
            &state.cache_system,
            &dep_graph_guard,
            &state.fast_hit_cache,
            &state.artifacts,
            state.in_flight_bytes.load(Ordering::Relaxed),
        );
        drop(dep_graph_guard);
        plan
    };
    let Some(plan) = plan else {
        return (0, 0);
    };
    let mut gentle_finished = false;
    let mut busy_candidates = Vec::new();
    let mut protected_paths = std::collections::HashSet::new();

    loop {
        let active_requests = state.active_cache_requests();
        if active_requests == 0 || started.elapsed() >= MEMORY_GC_FORCE_AFTER {
            // Nonblocking metadata removal intentionally leaves orphaned
            // journal rows. Settle that debt before deciding whether the
            // original headroom target still requires destructive work.
            let journal_removed = state.cache_system.cleanup_eviction_journal();
            total_freed += (journal_removed * super::super::eviction::JOURNAL_ENTRY_BYTES) as u64;
            total_items += journal_removed;

            let dep_graph_guard = state.dep_graph.load();
            let current = super::super::eviction::memory_snapshot(
                &state.cache_system,
                &dep_graph_guard,
                &state.fast_hit_cache,
                &state.artifacts,
                state.in_flight_bytes.load(Ordering::Relaxed),
            );
            drop(dep_graph_guard);
            if current.total_bytes as u64 <= plan.target_bytes() {
                return (total_freed, total_items);
            }

            // Retry lock-busy candidates in blocking mode, preserve only
            // timestamp-refreshed entries, then replenish from a current
            // read-only plan until the target is met or every remaining
            // metadata candidate is protected for this sweep.
            let mut completion_plan = plan.completion_from(candidate_offset, &busy_candidates);
            loop {
                let completion_had_candidates = completion_plan.metadata_candidate_count() > 0;
                let dep_graph_guard = state.dep_graph.load();
                let outcome = super::super::eviction::evict_to_budget_with_plan_detailed(
                    budget,
                    &state.cache_system,
                    &dep_graph_guard,
                    &state.fast_hit_cache,
                    &state.artifacts,
                    state.in_flight_bytes.load(Ordering::Relaxed),
                    &completion_plan,
                );
                drop(dep_graph_guard);
                total_freed += outcome.freed_bytes;
                total_items += outcome.items_removed;
                protected_paths.extend(
                    outcome
                        .refreshed
                        .iter()
                        .map(|candidate| candidate.path().clone()),
                );

                let dep_graph_guard = state.dep_graph.load();
                let current = super::super::eviction::memory_snapshot(
                    &state.cache_system,
                    &dep_graph_guard,
                    &state.fast_hit_cache,
                    &state.artifacts,
                    state.in_flight_bytes.load(Ordering::Relaxed),
                );
                if current.total_bytes as u64 <= plan.target_bytes() {
                    drop(dep_graph_guard);
                    return (total_freed, total_items);
                }
                let next = super::super::eviction::plan_eviction_to_target_excluding(
                    plan.target_bytes(),
                    &state.cache_system,
                    &dep_graph_guard,
                    &state.fast_hit_cache,
                    &state.artifacts,
                    state.in_flight_bytes.load(Ordering::Relaxed),
                    &protected_paths,
                );
                drop(dep_graph_guard);
                let Some(next) = next else {
                    return (total_freed, total_items);
                };
                if current.metadata_entries > 0 && next.metadata_candidate_count() == 0 {
                    return (total_freed, total_items);
                }
                if forced_completion_stalled(
                    outcome.items_removed,
                    completion_had_candidates,
                    current.metadata_entries,
                ) {
                    // Remaining pressure comes only from non-evictable state
                    // (for example in-flight persistence bytes, artifact-map
                    // overhead, or intentionally preserved recent fast hits).
                    return (total_freed, total_items);
                }
                completion_plan = next;
            }
        }

        if started.elapsed() < MEMORY_GC_IDLE_GRACE {
            tokio::select! {
                () = state.cache_requests_idle.notified() => {}
                () = tokio::time::sleep(
                    MEMORY_GC_IDLE_GRACE.saturating_sub(started.elapsed())
                ) => {}
            }
            continue;
        }

        if !gentle_finished {
            let outcome = super::super::eviction::try_evict_metadata_plan(
                &state.cache_system,
                &plan,
                candidate_offset,
                MEMORY_GC_GENTLE_BATCH,
            );
            total_freed += outcome.freed_bytes;
            total_items += outcome.items_removed;
            busy_candidates.extend(outcome.busy);
            protected_paths.extend(
                outcome
                    .refreshed
                    .iter()
                    .map(|candidate| candidate.path().clone()),
            );
            candidate_offset = candidate_offset.saturating_add(MEMORY_GC_GENTLE_BATCH);

            let dep_graph_guard = state.dep_graph.load();
            let current = super::super::eviction::memory_snapshot(
                &state.cache_system,
                &dep_graph_guard,
                &state.fast_hit_cache,
                &state.artifacts,
                state.in_flight_bytes.load(Ordering::Relaxed),
            );
            drop(dep_graph_guard);
            gentle_finished = current.total_bytes as u64 <= plan.target_bytes()
                || candidate_offset >= plan.metadata_candidate_count();
        }

        tokio::select! {
            () = state.cache_requests_idle.notified() => {}
            () = tokio::time::sleep(MEMORY_GC_GENTLE_RETRY) => {}
        }
    }
}

fn forced_completion_stalled(
    items_removed: usize,
    completion_had_candidates: bool,
    metadata_entries: usize,
) -> bool {
    items_removed == 0 && !completion_had_candidates && metadata_entries == 0
}

/// Start index hydration with publication ownership acquired before spawn.
/// The optional gate is a deterministic test seam for Clear/startup races.
pub(super) async fn spawn_artifact_loader(
    state: Arc<SharedState>,
    start_gate: Option<Arc<Notify>>,
) -> tokio::task::JoinHandle<()> {
    // Acquire before spawning so Clear cannot overtake the startup loader and
    // then have pre-Clear entries inserted afterward.
    let publication_guard = Arc::clone(&state.artifact_publication).read_owned().await;
    tokio::spawn(async move {
        let _publication_guard = publication_guard;
        if let Some(gate) = start_gate {
            gate.notified().await;
        }
        let artifact_dir = state.artifact_dir.clone();
        let state_ref = Arc::clone(&state);
        let loaded = tokio::task::spawn_blocking(move || {
            // Load the in-memory index that `ArtifactStore::open` already
            // hydrated from the on-disk blob.
            let entries = state_ref.artifact_store.load_all();
            if !entries.is_empty() {
                let count = entries.len();
                for (key, meta) in entries {
                    state_ref
                        .artifacts
                        .insert(key, CachedArtifact::from_index(meta));
                }
                count
            } else {
                // Migration: legacy `.meta` files predate the redb index and
                // the current bincode blob; populate the live store from them
                // so the first session after upgrade still has its warm cache.
                migrate_meta_files(
                    &artifact_dir,
                    &state_ref.artifacts,
                    &state_ref.artifact_store,
                )
            }
        })
        .await
        .unwrap_or(0);
        if loaded > 0 {
            tracing::info!(loaded, "background artifact loading complete");
        }
        state.artifacts_loaded.store(true, Ordering::Release);
    })
}

#[cfg(test)]
mod memory_eviction_controller_tests {
    use super::forced_completion_stalled;

    #[test]
    fn in_flight_only_pressure_terminates_after_no_progress() {
        assert!(forced_completion_stalled(0, false, 0));
        assert!(!forced_completion_stalled(1, false, 0));
        assert!(!forced_completion_stalled(0, true, 0));
        assert!(!forced_completion_stalled(0, false, 1));
    }
}
