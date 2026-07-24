//! Artifact-index WAL: in-memory write-ahead log that batches `ArtifactStore` snapshots to disk.

use super::*;

pub(super) enum IndexWriterCommand {
    Insert(String, ArtifactIndex),
    Remove(Vec<String>),
    Clear(tokio::sync::oneshot::Sender<()>),
    Flush(tokio::sync::oneshot::Sender<()>),
}

/// Default WAL flush interval. Persist tasks return immediately after sending
/// to the WAL; the WAL is flushed to the on-disk bincode blob on this cadence
/// (or earlier if it exceeds the size budget).
///
/// 5 s is intentionally long: hot-path reads and writes both go through the
/// in-memory `state.artifacts` `DashMap` (hydrated from the blob at startup),
/// so the on-disk file is touched only by the periodic background flush. The
/// cost of losing a flush window on hard crash is bounded — the artifact
/// files themselves are durable on disk, and the next session re-misses only
/// the keys that hadn't been flushed yet, repopulating both layers. Graceful
/// shutdown flushes synchronously, so this cost only materialises on power
/// loss / `kill -9`. Override via `ZCCACHE_WAL_FLUSH_MS`.
pub(super) fn wal_flush_interval() -> std::time::Duration {
    let ms: u64 = std::env::var("ZCCACHE_WAL_FLUSH_MS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(5000);
    std::time::Duration::from_millis(ms.max(1))
}

/// Size-based early-flush threshold. Prevents the WAL from growing unbounded
/// under a sustained burst that fills more than one flush window.
///
/// 2048 entries × ~770 bytes serialised = ~1.5 MB per flush. Each flush
/// snapshots the whole in-memory map (typically ~9 MB at steady state) and
/// writes it sequentially, so the trigger is "how many *new* entries before
/// we should re-snapshot" — not the size of one write.
pub(super) fn wal_max_pending() -> usize {
    std::env::var("ZCCACHE_WAL_MAX_PENDING")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(2048)
        .max(1)
}

/// Shutdown budget for the deterministic index-writer WAL drain (#1161).
/// Matches the embedded engine's 30 s flush bound (`embedded.rs`); a full
/// WAL flush snapshots the whole in-memory index to disk, which can take
/// seconds under I/O contention on a small (2-core CI) host.
const INDEX_WRITER_SHUTDOWN_DRAIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Shutdown budget for joining the index-writer task after a successful
/// drain. The drain ack proves the WAL is already empty and flushed, so the
/// join is normally instantaneous; the bound only guards a wedged task.
const INDEX_WRITER_SHUTDOWN_JOIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Shutdown budget for the deferred-persist drain (`pending_cache_writes`).
/// Matches the pre-existing bound in `run.rs`'s Shutdown arm (#799).
const PENDING_WRITES_SHUTDOWN_DRAIN_TIMEOUT: std::time::Duration =
    std::time::Duration::from_secs(30);

/// Shutdown-time durability drain (#1161).
///
/// Everything a graceful shutdown must quiesce BEFORE snapshotting the
/// depgraph, in dependency order:
///
/// 1. `pending_cache_writes` — deferred rustc/C++ persist tasks publish
///    their durable `ArtifactIndex` rows only after the cache files land on
///    disk (#799). They register before the compile response and complete
///    after their `IndexWriterCommand::Insert` send, so awaiting them
///    guarantees every backgrounded persist has finished its disk write and
///    queued its index row.
/// 2. The publication write guard — every detached publisher holds a read
///    guard until it finishes, so acquiring the write guard proves no new
///    index row can arrive after this point.
/// 3. The index-writer WAL flush ack — `IndexWriterCommand::Flush` is
///    FIFO-ordered behind every queued Insert, so its acknowledgement proves
///    every durable-index row has been applied to the in-memory store AND
///    snapshotted to disk. Mirrors the embedded engine's flush-then-stop
///    sequence (`embedded.rs`).
/// 4. Stop the writer task (bounded join; the WAL is already empty).
/// 5. Final `ArtifactStore` flush — covers call sites that insert directly
///    into the store without going through the WAL.
///
/// Returns the held publication write guard so the caller can keep
/// publishers blocked through its own depgraph/metadata snapshotting.
///
/// Called from `DaemonServer::run`'s Shutdown arm AND from restart-shaped
/// tests that drive the compile pipeline without `run()`
/// (`tests/multi_restart_context_key.rs`) — the tests exercise this exact
/// production drain rather than a parallel approximation.
///
/// Loud-forensics rule: every budget breach emits BOTH a `tracing::warn!`
/// AND a durable lifecycle event.
pub(super) async fn drain_durable_state_for_shutdown(
    state: &SharedState,
    index_writer_handle: Option<tokio::task::JoinHandle<()>>,
) -> tokio::sync::RwLockWriteGuard<'_, ()> {
    // 1. Deferred persist tasks (#799).
    let pending_drained = pending_writes::await_all(
        &state.pending_cache_writes,
        PENDING_WRITES_SHUTDOWN_DRAIN_TIMEOUT,
    )
    .await;
    if !pending_drained {
        tracing::warn!(
            pending = state.pending_cache_writes.len(),
            "timed out waiting for pending artifact writes before WAL drain"
        );
    }

    // 2. Block all publishers for the remainder of shutdown.
    let publication_guard = state.artifact_publication.write().await;

    // 3. Deterministic WAL drain. The standalone daemon previously went
    // straight to notify + 2 s join + silent `abort()`, which on a slow
    // 2-core host could abort the writer mid-drain and lose queued rows —
    // the warm daemon after restart then misses the artifacts the cold
    // daemon had already persisted (observed as the Integration
    // `legacy_path_validation` warm-multi miss).
    let drain_start = std::time::Instant::now();
    let index_writer_drained =
        flush_index_writer(&state.index_writer_tx, INDEX_WRITER_SHUTDOWN_DRAIN_TIMEOUT).await;
    if !index_writer_drained {
        tracing::warn!(
            event = crate::core::lifecycle::EVENT_EMBEDDED_FLUSH_STEP_TIMEOUT,
            step = "index_writer_drain",
            timeout_ms = INDEX_WRITER_SHUTDOWN_DRAIN_TIMEOUT.as_millis() as u64,
            elapsed_ms = drain_start.elapsed().as_millis() as u64,
            "index-writer WAL drain did not acknowledge within its \
             shutdown budget; queued durable-index rows may be lost"
        );
        crate::core::lifecycle::write_event_in_cache_root(
            state.cache_dir.as_path(),
            crate::core::lifecycle::EVENT_EMBEDDED_FLUSH_STEP_TIMEOUT,
            serde_json::json!({
                "step": "index_writer_drain",
                "timeout_ms": INDEX_WRITER_SHUTDOWN_DRAIN_TIMEOUT.as_millis() as u64,
                "reason": "shutdown WAL drain ack timed out; durable \
                           index rows queued behind the flush may be lost",
            }),
        );
    }

    // 4. Stop the writer task. `notify_one` retains a permit if the writer
    // is between polls; `notify_waiters` could lose the signal in that
    // window.
    state.index_writer_shutdown.notify_one();
    if let Some(mut handle) = index_writer_handle {
        if tokio::time::timeout(INDEX_WRITER_SHUTDOWN_JOIN_TIMEOUT, &mut handle)
            .await
            .is_err()
        {
            tracing::warn!(
                event = crate::core::lifecycle::EVENT_EMBEDDED_FLUSH_STEP_TIMEOUT,
                step = "index_writer_join",
                timeout_ms = INDEX_WRITER_SHUTDOWN_JOIN_TIMEOUT.as_millis() as u64,
                "index-writer task did not exit within its shutdown \
                 budget; aborting it"
            );
            crate::core::lifecycle::write_event_in_cache_root(
                state.cache_dir.as_path(),
                crate::core::lifecycle::EVENT_EMBEDDED_FLUSH_STEP_TIMEOUT,
                serde_json::json!({
                    "step": "index_writer_join",
                    "timeout_ms": INDEX_WRITER_SHUTDOWN_JOIN_TIMEOUT.as_millis() as u64,
                    "reason": "index-writer task join timed out after a \
                               drain attempt; task aborted",
                }),
            );
            handle.abort();
            let _ = handle.await;
        }
    }

    // 5. Final store flush: the WAL drain above only persists entries that
    // went through `index_writer_tx`. Some compile-success paths insert
    // DIRECTLY into `artifact_store` without sending to the WAL, and
    // `flush_wal_to_disk` early-returns on an empty WAL — so those
    // direct-inserts never reach disk on a WAL-only-empty shutdown.
    // Reproduced historically: a fresh medium-fixture build wrote 271 MB of
    // CAS payloads but no index.bin, leaving the warm-side daemon (and
    // every other `soldr load` consumer) with an empty index even though
    // all artifacts were on disk.
    let store = Arc::clone(&state.artifact_store);
    let entries = store.len();
    let flush_start = std::time::Instant::now();
    match store.flush_async().await {
        Ok(()) => tracing::info!(
            entries,
            elapsed_ms = flush_start.elapsed().as_millis() as u64,
            "artifact store final flush complete"
        ),
        Err(e) => tracing::warn!(entries, "artifact store final flush failed: {e}"),
    }

    publication_guard
}

/// Background index-writer task.
///
/// Acts as an in-memory WAL in front of the on-disk bincode blob:
///   * persist tasks push `(key, ArtifactIndex)` into the channel; they don't
///     wait for the disk write (cheap send).
///   * this task drains the channel into an in-memory `HashMap` (the WAL),
///     dedup'ing repeat keys.
///   * the WAL is flushed to disk on a timer (`ZCCACHE_WAL_FLUSH_MS`, default
///     5 s) or eagerly when it exceeds a size budget
///     (`ZCCACHE_WAL_MAX_PENDING`, default 2048).
///   * each flush applies the batch to `ArtifactStore` (in-memory DashMap)
///     and then snapshots the whole map atomically via `ArtifactStore::flush`
///     (tmp file + rename). One sequential write per flush window.
///   * channel close signals a final flush + clean exit (used by graceful
///     shutdown).
///
/// Reads don't consult the WAL: the daemon's authoritative in-memory state
/// lives in `state.artifacts` (a `DashMap` populated synchronously by the
/// persist call-sites themselves), and the on-disk blob is consulted only at
/// startup via `load_all()`. Entries that haven't yet flushed are still
/// visible to the running daemon; they're just at risk of being lost across
/// an abrupt crash (where the files-on-disk are durable but the next
/// session's `load_all()` won't see them, forcing a one-time re-miss).
pub(super) async fn run_index_writer(
    mut rx: tokio::sync::mpsc::UnboundedReceiver<IndexWriterCommand>,
    store: Arc<ArtifactStore>,
    shutdown: Arc<Notify>,
) {
    use std::collections::HashMap;
    let flush_interval = wal_flush_interval();
    let max_pending = wal_max_pending();
    let mut wal: HashMap<String, ArtifactIndex> = HashMap::with_capacity(max_pending);
    let mut ticker = tokio::time::interval(flush_interval);
    // Don't immediately fire on the first tick — wait one interval.
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let _ = ticker.tick().await;

    loop {
        tokio::select! {
            msg = rx.recv() => {
                match msg {
                    Some(command) => {
                        process_index_writer_command(command, &store, &mut wal, max_pending).await;
                        // Drain whatever else is already queued in this tick.
                        while let Ok(command) = rx.try_recv() {
                            process_index_writer_command(
                                command,
                                &store,
                                &mut wal,
                                max_pending,
                            )
                            .await;
                        }
                    }
                    None => {
                        // Channel closed (last sender dropped). Final flush.
                        flush_wal_to_disk(&store, &mut wal).await;
                        return;
                    }
                }
            }
            _ = ticker.tick() => {
                if !wal.is_empty() {
                    flush_wal_to_disk(&store, &mut wal).await;
                }
            }
            _ = shutdown.notified() => {
                // Daemon-initiated graceful shutdown. Drain anything still
                // queued and flush before the runtime aborts us.
                while let Ok(command) = rx.try_recv() {
                    process_index_writer_command(command, &store, &mut wal, max_pending).await;
                }
                tracing::info!(
                    pending = wal.len(),
                    "index-writer shutdown signal received, draining and flushing"
                );
                flush_wal_to_disk(&store, &mut wal).await;
                return;
            }
        }
    }
}

async fn process_index_writer_command(
    command: IndexWriterCommand,
    store: &Arc<ArtifactStore>,
    wal: &mut std::collections::HashMap<String, ArtifactIndex>,
    max_pending: usize,
) {
    match command {
        IndexWriterCommand::Insert(k, v) => {
            wal.insert(k, v);
            if wal.len() >= max_pending {
                flush_wal_to_disk(store, wal).await;
            }
        }
        IndexWriterCommand::Remove(keys) => {
            for key in &keys {
                wal.remove(key);
            }
            let refs: Vec<&str> = keys.iter().map(String::as_str).collect();
            let removed = store.remove_batch(&refs);
            match Arc::clone(store).flush_async().await {
                Ok(()) => tracing::info!(removed, "artifact-index removals flushed to disk"),
                Err(error) => {
                    tracing::warn!(removed, %error, "artifact-index removal flush failed")
                }
            }
        }
        IndexWriterCommand::Clear(ack) => {
            wal.clear();
            store.clear();
            if let Err(error) = Arc::clone(store).flush_async().await {
                tracing::warn!(%error, "cleared artifact index flush failed");
            }
            let _ = ack.send(());
        }
        IndexWriterCommand::Flush(ack) => {
            flush_wal_to_disk(store, wal).await;
            let _ = ack.send(());
        }
    }
}

pub(super) async fn flush_index_writer(
    tx: &tokio::sync::mpsc::UnboundedSender<IndexWriterCommand>,
    timeout: std::time::Duration,
) -> bool {
    let (ack_tx, ack_rx) = tokio::sync::oneshot::channel();
    if tx.send(IndexWriterCommand::Flush(ack_tx)).is_err() {
        return false;
    }
    matches!(tokio::time::timeout(timeout, ack_rx).await, Ok(Ok(())))
}

pub(super) async fn clear_index_writer(
    tx: &tokio::sync::mpsc::UnboundedSender<IndexWriterCommand>,
    timeout: std::time::Duration,
) -> bool {
    let (ack_tx, ack_rx) = tokio::sync::oneshot::channel();
    if tx.send(IndexWriterCommand::Clear(ack_tx)).is_err() {
        return false;
    }
    matches!(tokio::time::timeout(timeout, ack_rx).await, Ok(Ok(())))
}

pub(super) async fn flush_wal_to_disk(
    store: &Arc<ArtifactStore>,
    wal: &mut std::collections::HashMap<String, ArtifactIndex>,
) {
    if wal.is_empty() {
        return;
    }
    let drained: Vec<(String, ArtifactIndex)> = wal.drain().collect();
    let count = drained.len();
    // Apply the batch to the in-memory store synchronously (cheap), then
    // do the disk write off the runtime thread so the flush doesn't block
    // request handlers.
    store.insert_many(drained);
    let res = Arc::clone(store).flush_async().await;
    match res {
        Ok(()) => tracing::info!(committed = count, "WAL flushed to disk"),
        Err(e) => tracing::warn!(count, "WAL flush to disk failed: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn remove_cancels_pending_insert_and_persists_deletion() {
        let temp = tempfile::tempdir().expect("temporary index directory");
        let index_path = temp.path().join("index.bin");
        let store = Arc::new(ArtifactStore::open_empty(&index_path));
        let mut wal = std::collections::HashMap::new();
        let key = "a".repeat(64);
        let meta = ArtifactIndex::new(
            vec!["output.o".to_string()],
            vec![4096],
            Vec::new(),
            Vec::new(),
            0,
        );

        process_index_writer_command(
            IndexWriterCommand::Insert(key.clone(), meta),
            &store,
            &mut wal,
            usize::MAX,
        )
        .await;
        assert!(wal.contains_key(&key));

        process_index_writer_command(
            IndexWriterCommand::Remove(vec![key.clone()]),
            &store,
            &mut wal,
            usize::MAX,
        )
        .await;

        assert!(!wal.contains_key(&key));
        assert!(store.get(&key).is_none());
        let reopened = ArtifactStore::open(&index_path).expect("reopen persisted index");
        assert!(reopened.get(&key).is_none());
    }
}
