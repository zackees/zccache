//! Settle/coalesce buffer for bursty filesystem events.
//!
//! Filesystem watchers fire many events in rapid succession (e.g., a `cargo build`
//! touching 100 files in 10ms). The settle buffer waits for a configurable quiet
//! period before emitting a single coalesced batch.
//!
//! Overflow events bypass coalescing entirely — they clear pending state and
//! emit immediately, since everything is invalidated.

use std::collections::HashMap;
use std::time::Duration;
use tokio::sync::mpsc;
use zccache::core::NormalizedPath;

use super::WatchEvent;

/// Output from the settle buffer.
#[derive(Debug, Clone)]
pub enum SettledEvent {
    /// A coalesced batch of file changes after the settle window.
    Batch {
        changed: Vec<NormalizedPath>,
        removed: Vec<NormalizedPath>,
    },
    /// Watcher overflow — all cached state should be considered stale.
    Overflow,
}

/// Coalesces bursty filesystem events into settled batches.
///
/// The daemon's file watcher produces a storm of events during builds.
/// The settle buffer absorbs them and waits for a quiet period (the settle
/// window) before emitting a single batch. This prevents the cache from
/// doing redundant work during a burst.
#[derive(Debug)]
pub struct SettleBuffer {
    settle_window: Duration,
    /// Maximum time from the first event before forcing batch emission,
    /// even if events are still arriving. Prevents starvation when
    /// the daemon writes to watched directories (logs, artifacts).
    max_wait: Duration,
}

/// Tracks the most recent change kind for a path during coalescing.
#[derive(Debug, Clone, Copy)]
enum ChangeKind {
    Modified,
    Removed,
}

impl SettleBuffer {
    /// Create a settle buffer with the given settle window and max wait.
    #[must_use]
    pub fn new(settle_window: Duration) -> Self {
        Self {
            settle_window,
            max_wait: Duration::from_millis(50),
        }
    }

    /// Create a settle buffer with the default 50ms settle window.
    #[must_use]
    pub fn default_window() -> Self {
        Self::new(Duration::from_millis(50))
    }

    /// Run the settle loop.
    ///
    /// Reads raw `WatchEvent`s from `rx`, coalesces them, and sends
    /// `SettledEvent`s to `tx` after each settle window elapses.
    ///
    /// Returns when the input channel is closed.
    pub async fn run(
        &self,
        mut rx: mpsc::UnboundedReceiver<WatchEvent>,
        tx: mpsc::UnboundedSender<SettledEvent>,
    ) {
        let mut pending: HashMap<NormalizedPath, ChangeKind> = HashMap::new();

        loop {
            // Wait for the first event (or channel close).
            let event = match rx.recv().await {
                Some(e) => e,
                None => {
                    // Channel closed — flush any remaining events.
                    if !pending.is_empty() {
                        let _ = tx.send(Self::drain(&mut pending));
                    }
                    return;
                }
            };

            // Handle overflow immediately — don't wait for settle.
            // WatchEvent::Error is also treated as overflow because on Windows,
            // ReadDirectoryChangesW buffer overflow and watcher death arrive as
            // errors from the notify crate, not as distinct overflow events.
            // Treating errors as overflow is conservative but correct: it forces
            // a full re-stat of all cached entries on the next access.
            if matches!(event, WatchEvent::Overflow | WatchEvent::Error(_)) {
                pending.clear();
                let _ = tx.send(SettledEvent::Overflow);
                continue;
            }

            Self::apply_event(&mut pending, event);

            // Coalesce: keep reading until either (a) the settle window elapses
            // with no new events, or (b) max_wait from the first event is reached.
            // Without (b), continuous writes (e.g. session log) starve the buffer.
            let deadline = tokio::time::Instant::now() + self.max_wait;
            loop {
                let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
                let wait = self.settle_window.min(remaining);
                if wait.is_zero() {
                    // Max wait reached — force emit.
                    if !pending.is_empty() {
                        let _ = tx.send(Self::drain(&mut pending));
                    }
                    break;
                }
                match tokio::time::timeout(wait, rx.recv()).await {
                    Ok(Some(WatchEvent::Overflow | WatchEvent::Error(_))) => {
                        pending.clear();
                        let _ = tx.send(SettledEvent::Overflow);
                        break;
                    }
                    Ok(Some(event)) => {
                        Self::apply_event(&mut pending, event);
                    }
                    Ok(None) => {
                        // Channel closed — flush remaining.
                        if !pending.is_empty() {
                            let _ = tx.send(Self::drain(&mut pending));
                        }
                        return;
                    }
                    Err(_timeout) => {
                        // Settle window elapsed — emit batch.
                        if !pending.is_empty() {
                            let _ = tx.send(Self::drain(&mut pending));
                        }
                        break;
                    }
                }
            }
        }
    }

    fn apply_event(pending: &mut HashMap<NormalizedPath, ChangeKind>, event: WatchEvent) {
        match event {
            WatchEvent::Modified(p) | WatchEvent::Created(p) => {
                pending.insert(p, ChangeKind::Modified);
            }
            WatchEvent::Removed(p) => {
                pending.insert(p, ChangeKind::Removed);
            }
            WatchEvent::Renamed { from, to } => {
                pending.insert(from, ChangeKind::Removed);
                pending.insert(to, ChangeKind::Modified);
            }
            WatchEvent::Overflow | WatchEvent::Error(_) => {
                // Overflow handled in run(). Errors are logged upstream.
            }
        }
    }

    fn drain(pending: &mut HashMap<NormalizedPath, ChangeKind>) -> SettledEvent {
        let mut changed = Vec::new();
        let mut removed = Vec::new();
        for (path, kind) in pending.drain() {
            match kind {
                ChangeKind::Modified => changed.push(path),
                ChangeKind::Removed => removed.push(path),
            }
        }
        SettledEvent::Batch { changed, removed }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn single_event_settles() {
        let (raw_tx, raw_rx) = mpsc::unbounded_channel();
        let (settled_tx, mut settled_rx) = mpsc::unbounded_channel();

        let buffer = SettleBuffer::new(Duration::from_millis(20));
        let handle = tokio::spawn(async move {
            buffer.run(raw_rx, settled_tx).await;
        });

        raw_tx.send(WatchEvent::Modified("a.c".into())).unwrap();
        drop(raw_tx);

        let event = settled_rx.recv().await.unwrap();
        match event {
            SettledEvent::Batch { changed, removed } => {
                assert_eq!(changed.len(), 1);
                assert!(removed.is_empty());
            }
            SettledEvent::Overflow => panic!("expected batch"),
        }

        handle.await.unwrap();
    }

    #[tokio::test]
    async fn rapid_events_coalesce_into_one_batch() {
        let (raw_tx, raw_rx) = mpsc::unbounded_channel();
        let (settled_tx, mut settled_rx) = mpsc::unbounded_channel();

        let buffer = SettleBuffer::new(Duration::from_millis(50));
        let handle = tokio::spawn(async move {
            buffer.run(raw_rx, settled_tx).await;
        });

        for i in 0..5 {
            raw_tx
                .send(WatchEvent::Modified(format!("file_{i}.c").into()))
                .unwrap();
        }
        drop(raw_tx);

        let event = settled_rx.recv().await.unwrap();
        match event {
            SettledEvent::Batch { changed, removed } => {
                assert_eq!(changed.len(), 5);
                assert!(removed.is_empty());
            }
            SettledEvent::Overflow => panic!("expected batch"),
        }

        handle.await.unwrap();
    }

    #[tokio::test]
    async fn same_file_deduplicates() {
        let (raw_tx, raw_rx) = mpsc::unbounded_channel();
        let (settled_tx, mut settled_rx) = mpsc::unbounded_channel();

        let buffer = SettleBuffer::new(Duration::from_millis(20));
        let handle = tokio::spawn(async move {
            buffer.run(raw_rx, settled_tx).await;
        });

        let path = NormalizedPath::new("hot.c");
        raw_tx.send(WatchEvent::Modified(path.clone())).unwrap();
        raw_tx.send(WatchEvent::Modified(path.clone())).unwrap();
        raw_tx.send(WatchEvent::Modified(path)).unwrap();
        drop(raw_tx);

        let event = settled_rx.recv().await.unwrap();
        match event {
            SettledEvent::Batch { changed, removed } => {
                assert_eq!(changed.len(), 1);
                assert!(removed.is_empty());
            }
            SettledEvent::Overflow => panic!("expected batch"),
        }

        handle.await.unwrap();
    }

    #[tokio::test]
    async fn modify_then_remove_tracks_as_removed() {
        let (raw_tx, raw_rx) = mpsc::unbounded_channel();
        let (settled_tx, mut settled_rx) = mpsc::unbounded_channel();

        let buffer = SettleBuffer::new(Duration::from_millis(20));
        let handle = tokio::spawn(async move {
            buffer.run(raw_rx, settled_tx).await;
        });

        let path = NormalizedPath::new("temp.c");
        raw_tx.send(WatchEvent::Modified(path.clone())).unwrap();
        raw_tx.send(WatchEvent::Removed(path)).unwrap();
        drop(raw_tx);

        let event = settled_rx.recv().await.unwrap();
        match event {
            SettledEvent::Batch { changed, removed } => {
                assert!(changed.is_empty());
                assert_eq!(removed.len(), 1);
            }
            SettledEvent::Overflow => panic!("expected batch"),
        }

        handle.await.unwrap();
    }

    #[tokio::test]
    async fn remove_then_create_tracks_as_modified() {
        let (raw_tx, raw_rx) = mpsc::unbounded_channel();
        let (settled_tx, mut settled_rx) = mpsc::unbounded_channel();

        let buffer = SettleBuffer::new(Duration::from_millis(20));
        let handle = tokio::spawn(async move {
            buffer.run(raw_rx, settled_tx).await;
        });

        let path = NormalizedPath::new("replaced.c");
        raw_tx.send(WatchEvent::Removed(path.clone())).unwrap();
        raw_tx.send(WatchEvent::Created(path)).unwrap();
        drop(raw_tx);

        let event = settled_rx.recv().await.unwrap();
        match event {
            SettledEvent::Batch { changed, removed } => {
                assert_eq!(changed.len(), 1);
                assert!(changed.contains(&NormalizedPath::new("replaced.c")));
                assert!(removed.is_empty());
            }
            SettledEvent::Overflow => panic!("expected batch"),
        }

        handle.await.unwrap();
    }

    #[tokio::test]
    async fn rename_becomes_remove_and_modify() {
        let (raw_tx, raw_rx) = mpsc::unbounded_channel();
        let (settled_tx, mut settled_rx) = mpsc::unbounded_channel();

        let buffer = SettleBuffer::new(Duration::from_millis(20));
        let handle = tokio::spawn(async move {
            buffer.run(raw_rx, settled_tx).await;
        });

        raw_tx
            .send(WatchEvent::Renamed {
                from: "old.c".into(),
                to: "new.c".into(),
            })
            .unwrap();
        drop(raw_tx);

        let event = settled_rx.recv().await.unwrap();
        match event {
            SettledEvent::Batch { changed, removed } => {
                assert_eq!(changed.len(), 1);
                assert!(changed.contains(&NormalizedPath::new("new.c")));
                assert_eq!(removed.len(), 1);
                assert!(removed.contains(&NormalizedPath::new("old.c")));
            }
            SettledEvent::Overflow => panic!("expected batch"),
        }

        handle.await.unwrap();
    }

    #[tokio::test]
    async fn overflow_clears_pending_and_emits_immediately() {
        let (raw_tx, raw_rx) = mpsc::unbounded_channel();
        let (settled_tx, mut settled_rx) = mpsc::unbounded_channel();

        let buffer = SettleBuffer::new(Duration::from_millis(50));
        let handle = tokio::spawn(async move {
            buffer.run(raw_rx, settled_tx).await;
        });

        // Send some events, then overflow.
        raw_tx.send(WatchEvent::Modified("a.c".into())).unwrap();
        raw_tx.send(WatchEvent::Overflow).unwrap();
        drop(raw_tx);

        let event = settled_rx.recv().await.unwrap();
        assert!(matches!(event, SettledEvent::Overflow));

        handle.await.unwrap();
    }

    #[tokio::test]
    async fn error_events_trigger_overflow() {
        // On Windows, ReadDirectoryChangesW buffer overflow and watcher death
        // arrive as errors from the notify crate. We treat them as overflow to
        // force a full re-stat of all cached entries.
        let (raw_tx, raw_rx) = mpsc::unbounded_channel();
        let (settled_tx, mut settled_rx) = mpsc::unbounded_channel();

        let buffer = SettleBuffer::new(Duration::from_millis(20));
        let handle = tokio::spawn(async move {
            buffer.run(raw_rx, settled_tx).await;
        });

        raw_tx.send(WatchEvent::Modified("a.c".into())).unwrap();
        raw_tx
            .send(WatchEvent::Error("some error".to_string()))
            .unwrap();
        drop(raw_tx);

        // Error should trigger overflow, discarding the pending Modified event.
        let event = settled_rx.recv().await.unwrap();
        assert!(matches!(event, SettledEvent::Overflow));

        handle.await.unwrap();
    }

    #[tokio::test]
    async fn default_window_creates_buffer() {
        let buffer = SettleBuffer::default_window();
        // Just verify it can run without panicking.
        let (raw_tx, raw_rx) = mpsc::unbounded_channel();
        let (settled_tx, mut settled_rx) = mpsc::unbounded_channel();

        let handle = tokio::spawn(async move {
            buffer.run(raw_rx, settled_tx).await;
        });

        raw_tx.send(WatchEvent::Modified("x.c".into())).unwrap();
        drop(raw_tx);

        let event = settled_rx.recv().await.unwrap();
        assert!(matches!(event, SettledEvent::Batch { .. }));
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn multiple_overflows_in_sequence() {
        let (raw_tx, raw_rx) = mpsc::unbounded_channel();
        let (settled_tx, mut settled_rx) = mpsc::unbounded_channel();

        let buffer = SettleBuffer::new(Duration::from_millis(20));
        let handle = tokio::spawn(async move {
            buffer.run(raw_rx, settled_tx).await;
        });

        raw_tx.send(WatchEvent::Overflow).unwrap();
        raw_tx.send(WatchEvent::Overflow).unwrap();
        raw_tx.send(WatchEvent::Overflow).unwrap();
        drop(raw_tx);

        // Should get at least one overflow event.
        let mut overflow_count = 0;
        while let Some(event) = settled_rx.recv().await {
            if matches!(event, SettledEvent::Overflow) {
                overflow_count += 1;
            }
        }
        assert!(overflow_count >= 1);
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn overflow_then_normal_events() {
        let (raw_tx, raw_rx) = mpsc::unbounded_channel();
        let (settled_tx, mut settled_rx) = mpsc::unbounded_channel();

        let buffer = SettleBuffer::new(Duration::from_millis(20));
        let handle = tokio::spawn(async move {
            buffer.run(raw_rx, settled_tx).await;
        });

        raw_tx.send(WatchEvent::Overflow).unwrap();
        raw_tx.send(WatchEvent::Modified("after.c".into())).unwrap();
        drop(raw_tx);

        let mut saw_overflow = false;
        let mut saw_batch = false;
        while let Some(event) = settled_rx.recv().await {
            match event {
                SettledEvent::Overflow => saw_overflow = true,
                SettledEvent::Batch { changed, .. } => {
                    assert!(changed.contains(&NormalizedPath::new("after.c")));
                    saw_batch = true;
                }
            }
        }
        assert!(saw_overflow);
        assert!(saw_batch);
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn large_batch_coalesces() {
        let (raw_tx, raw_rx) = mpsc::unbounded_channel();
        let (settled_tx, mut settled_rx) = mpsc::unbounded_channel();

        let buffer = SettleBuffer::new(Duration::from_millis(50));
        let handle = tokio::spawn(async move {
            buffer.run(raw_rx, settled_tx).await;
        });

        for i in 0..200 {
            raw_tx
                .send(WatchEvent::Modified(format!("src/file_{i}.c").into()))
                .unwrap();
        }
        drop(raw_tx);

        // Collect all batches — total changed files should be 200.
        let mut total_changed = 0;
        while let Some(event) = settled_rx.recv().await {
            if let SettledEvent::Batch { changed, .. } = event {
                total_changed += changed.len();
            }
        }
        assert_eq!(total_changed, 200);
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn mixed_event_types_in_burst() {
        let (raw_tx, raw_rx) = mpsc::unbounded_channel();
        let (settled_tx, mut settled_rx) = mpsc::unbounded_channel();

        let buffer = SettleBuffer::new(Duration::from_millis(20));
        let handle = tokio::spawn(async move {
            buffer.run(raw_rx, settled_tx).await;
        });

        raw_tx.send(WatchEvent::Created("new.c".into())).unwrap();
        raw_tx.send(WatchEvent::Modified("edit.c".into())).unwrap();
        raw_tx.send(WatchEvent::Removed("gone.c".into())).unwrap();
        raw_tx
            .send(WatchEvent::Renamed {
                from: "old.c".into(),
                to: "renamed.c".into(),
            })
            .unwrap();
        drop(raw_tx);

        let event = settled_rx.recv().await.unwrap();
        match event {
            SettledEvent::Batch { changed, removed } => {
                // new.c (Created), edit.c (Modified), renamed.c (from Renamed.to)
                assert_eq!(changed.len(), 3);
                // gone.c (Removed), old.c (from Renamed.from)
                assert_eq!(removed.len(), 2);
            }
            SettledEvent::Overflow => panic!("expected batch"),
        }
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn channel_close_mid_coalesce_flushes() {
        let (raw_tx, raw_rx) = mpsc::unbounded_channel();
        let (settled_tx, mut settled_rx) = mpsc::unbounded_channel();

        let buffer = SettleBuffer::new(Duration::from_millis(500)); // Long window
        let handle = tokio::spawn(async move {
            buffer.run(raw_rx, settled_tx).await;
        });

        // Send events then close channel before settle window elapses.
        raw_tx.send(WatchEvent::Modified("a.c".into())).unwrap();
        raw_tx.send(WatchEvent::Modified("b.c".into())).unwrap();
        drop(raw_tx); // Close before 500ms window

        let event = settled_rx.recv().await.unwrap();
        match event {
            SettledEvent::Batch { changed, .. } => {
                assert_eq!(changed.len(), 2);
            }
            SettledEvent::Overflow => panic!("expected batch"),
        }
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn overflow_with_empty_pending() {
        let (raw_tx, raw_rx) = mpsc::unbounded_channel();
        let (settled_tx, mut settled_rx) = mpsc::unbounded_channel();

        let buffer = SettleBuffer::new(Duration::from_millis(20));
        let handle = tokio::spawn(async move {
            buffer.run(raw_rx, settled_tx).await;
        });

        // Overflow with no prior events.
        raw_tx.send(WatchEvent::Overflow).unwrap();
        drop(raw_tx);

        let event = settled_rx.recv().await.unwrap();
        assert!(matches!(event, SettledEvent::Overflow));
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn empty_close_produces_no_output() {
        let (raw_tx, raw_rx) = mpsc::unbounded_channel();
        let (settled_tx, mut settled_rx) = mpsc::unbounded_channel();

        let buffer = SettleBuffer::new(Duration::from_millis(20));
        let handle = tokio::spawn(async move {
            buffer.run(raw_rx, settled_tx).await;
        });

        drop(raw_tx);

        // Should get None (channel closed, no events).
        let event = settled_rx.recv().await;
        assert!(event.is_none());

        handle.await.unwrap();
    }
}
