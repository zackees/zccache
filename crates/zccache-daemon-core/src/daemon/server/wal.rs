//! Artifact-index WAL: in-memory write-ahead log that batches `ArtifactStore` snapshots to disk.

use super::*;

pub(super) enum IndexWriterCommand {
    Insert(String, ArtifactIndex),
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
