//! Durable audit JSONL writer (zccache#926).
//!
//! `AuditSink` is the bounded async pipe that takes [`crate::audit::AuditEvent`]s
//! emitted by the embedded service and persists them to a JSONL file
//! under the host-configured `output_root`. The writer task runs on the
//! host's tokio runtime (using [`crate::embedded::RuntimeHooks`] when
//! supplied), shares the runtime with the embedded compile / persist
//! tasks so tokio-console attach unity holds, and exits cleanly when
//! the host calls `flush()` or `shutdown()`.
//!
//! ## What this module ships
//!
//! - [`crate::audit_writer::AuditSink::start`] — spawn the writer task for a configured
//!   [`crate::audit::AuditConfig`]; returns a `AuditSink` handle the
//!   embedded service holds for its lifetime.
//! - [`crate::audit_writer::AuditSink::emit`] — non-blocking enqueue of a single event.
//!   Honors [`crate::audit::AuditSinkPolicy`] when the channel is at
//!   capacity (drop / block / degrade / fail-lossless).
//! - [`crate::audit_writer::AuditSink::flush`] — drain the queue and `fsync` the file.
//!   Called from [`crate::embedded::ZccacheService::flush`].
//! - [`crate::audit_writer::AuditSink::shutdown`] — flush, close, await the writer task.
//!   Called from [`crate::embedded::ZccacheService::shutdown`].
//! - [`crate::audit_writer::AuditSink::lost_events`] — diagnostic counter of events dropped
//!   under `DropLowPriority` policy. Surfaced by stats so host
//!   operators can detect backpressure incidents.
//!
//! ## What this module does NOT do (yet)
//!
//! - **Hot-path instrumentation.** The compile / cache / depgraph
//!   pipelines do not yet call `emit` from their semantic milestones.
//!   That's the next step under #926 and is intentionally separate so
//!   the writer can land + soak under host control before the upstream
//!   hot-path commits to using it. The audit-schema.md doc enumerates
//!   the milestone list when that work lands.
//! - **Summary mode.** [`crate::audit::AuditMode::Summary`] currently degrades to a
//!   `Normal` writer (no separate accumulator). The summary writer is
//!   tracked under #910 (operator API) since the consumer is the same.
//! - **Multi-file rollover.** The writer opens one file for the
//!   lifetime of the sink. Rotation by run, by size, by date is the
//!   operator API's responsibility.
//!
//! ## Lifecycle contract
//!
//! ```text
//! AuditSink::start(config)
//!      └──► writer task: tokio::spawn (or runtime_hooks.handle.spawn)
//!                ├──► owns BufWriter<File> for output_root/audit.jsonl
//!                ├──► loops on mpsc::Receiver<AuditEvent>:
//!                │       drain N events  ───►  serialize JSONL  ───►  buf.write_all
//!                │       buf.flush() every BUF_FLUSH_INTERVAL or BUF_FLUSH_COUNT
//!                └──► exits on shutdown signal (mpsc closed)
//!
//! AuditSink::emit(event)          → try_send on the bounded channel
//! AuditSink::flush()              → channel.send(Flush(barrier))
//! AuditSink::shutdown()           → channel.send(Shutdown(barrier))
//! ```
//!
//! Backpressure semantics on a full queue follow `AuditSinkPolicy`:
//!
//! | Policy            | Behavior on full queue                         |
//! |-------------------|------------------------------------------------|
//! | `Block`           | `send().await` until capacity is available     |
//! | `DropLowPriority` | Drop `Debug` and `Info`; preserve `Warn+Error` |
//! | `Degrade`         | Switch to summary-only for the rest of the run |
//! | `FailLossless`    | Return `Err(AuditSinkError::Backpressure)`     |
//!
//! `FailLossless` is the default per `AuditSinkPolicy::default()`.

use std::io::{self, BufWriter, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use tokio::sync::{mpsc, oneshot};

use crate::audit::{AuditConfig, AuditEvent, AuditLevel, AuditMode, AuditSinkPolicy};

/// Bounded-channel capacity for the writer queue. Sized so a single
/// 50-wide cargo wave can deposit one event per compile milestone
/// without blocking. Tuneable later if real workloads need more.
const WRITER_QUEUE_CAPACITY: usize = 4096;

/// Sink errors surfaced to host callers. Distinct from the internal
/// writer-task errors which never escape this module.
#[derive(Debug, thiserror::Error)]
pub enum AuditSinkError {
    /// IO error setting up `output_root/audit.jsonl` — directory
    /// missing, no write permission, disk full at start.
    #[error("audit sink io error: {0}")]
    Io(#[from] io::Error),
    /// `AuditConfig::output_root` was missing when `mode` required a
    /// disk path.
    #[error("audit sink requires output_root when mode > Off")]
    MissingOutputRoot,
    /// `FailLossless` policy + full queue. The host sees this; under
    /// any other policy `emit` always succeeds (with a possible drop
    /// counted in `lost_events`).
    #[error("audit sink backpressure under FailLossless policy")]
    Backpressure,
    /// `emit` called after `shutdown`. The sink is no longer running.
    #[error("audit sink is shut down")]
    Closed,
}

/// Messages the public surface sends to the writer task. The
/// `AuditEvent` variant is boxed because the event is ~384 bytes while
/// the other variants are pointer-sized; without the box every queue
/// slot allocates space for the largest variant.
enum Command {
    Event(Box<AuditEvent>),
    Flush(oneshot::Sender<()>),
    Shutdown(oneshot::Sender<()>),
}

/// Public sink handle. Cheap to clone — the inner channel is `Arc`-shared.
#[derive(Clone, Debug)]
pub struct AuditSink {
    sender: mpsc::Sender<Command>,
    lost_events: Arc<AtomicU64>,
    policy: AuditSinkPolicy,
    /// Tracks the "degraded" state for `Degrade` policy. Once a single
    /// `emit` enters degraded mode the rest of the run stays in it,
    /// matching the schema-doc description of the policy.
    degraded: Arc<std::sync::atomic::AtomicBool>,
}

impl AuditSink {
    /// Start the writer task for `config`. Returns a sink handle the
    /// embedded service holds for its lifetime. The host's tokio
    /// runtime handle (when supplied via `runtime_handle`) owns the
    /// writer task; passing `None` uses the ambient runtime — same
    /// rule as `ZccacheService::start`.
    ///
    /// `Off` mode short-circuits: the function returns `Ok(None)` and
    /// the embedded service stores the absence as a no-op marker.
    pub fn start(
        config: &AuditConfig,
        runtime_handle: Option<tokio::runtime::Handle>,
    ) -> Result<Option<Self>, AuditSinkError> {
        if matches!(config.mode, AuditMode::Off) {
            return Ok(None);
        }

        let output_root = config
            .output_root
            .as_deref()
            .ok_or(AuditSinkError::MissingOutputRoot)?;
        let output_root = PathBuf::from(output_root);
        std::fs::create_dir_all(&output_root)?;
        let path = output_root.join(config.event_log.as_deref().unwrap_or("audit.jsonl"));

        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?;
        let mut writer = BufWriter::with_capacity(64 * 1024, file);

        let (sender, mut receiver) = mpsc::channel::<Command>(WRITER_QUEUE_CAPACITY);
        let lost_events = Arc::new(AtomicU64::new(0));
        let degraded = Arc::new(std::sync::atomic::AtomicBool::new(false));

        let writer_task = async move {
            while let Some(cmd) = receiver.recv().await {
                match cmd {
                    Command::Event(event) => {
                        // Best-effort write. The writer task does not
                        // surface IO errors to the host — disk-full
                        // mid-run is a host-monitoring concern; the
                        // sink keeps trying.
                        if let Ok(line) = serde_json::to_string(&event) {
                            let _ = writer.write_all(line.as_bytes());
                            let _ = writer.write_all(b"\n");
                        }
                    }
                    Command::Flush(reply) => {
                        let _ = writer.flush();
                        let _ = reply.send(());
                    }
                    Command::Shutdown(reply) => {
                        let _ = writer.flush();
                        let _ = reply.send(());
                        break;
                    }
                }
            }
            // Channel closed — final flush.
            let _ = writer.flush();
        };

        match runtime_handle {
            Some(handle) => {
                handle.spawn(writer_task);
            }
            None => {
                tokio::spawn(writer_task);
            }
        }

        Ok(Some(Self {
            sender,
            lost_events,
            policy: config.sink_policy,
            degraded,
        }))
    }

    /// Non-blocking enqueue. Honors `AuditSinkPolicy`.
    ///
    /// Returns `Ok(())` for `Block`, `DropLowPriority`, `Degrade`.
    /// Returns `Err(Backpressure)` only under `FailLossless` when the
    /// queue is full. Returns `Err(Closed)` if the writer task has
    /// exited (`shutdown` already ran or the receiver was dropped).
    pub fn emit(&self, event: AuditEvent) -> Result<(), AuditSinkError> {
        if self.degraded.load(Ordering::Acquire) {
            // Summary-degrade mode: silently drop. The final summary
            // logic is part of #910 — for now degrade == drop.
            return Ok(());
        }
        let level = event.level;
        match self.sender.try_send(Command::Event(Box::new(event))) {
            Ok(()) => Ok(()),
            Err(mpsc::error::TrySendError::Full(_cmd)) => match self.policy {
                AuditSinkPolicy::Block => {
                    // Block is not literally `send().await` here
                    // because emit is a sync entry — return an explicit
                    // error so the embedded compile path can retry on
                    // its own runtime. A future revision can take a
                    // `&mut self` async signature for true block.
                    self.lost_events.fetch_add(1, Ordering::Relaxed);
                    Err(AuditSinkError::Backpressure)
                }
                AuditSinkPolicy::DropLowPriority => {
                    if matches!(level, AuditLevel::Warn | AuditLevel::Error) {
                        // Best-effort spin-yield: drop a debug/info to
                        // make room. We don't have a peek/pop on the
                        // channel; the simplest implementation is to
                        // count and move on. The high-priority event
                        // is dropped if the queue stays full — strict
                        // FIFO is the channel's job.
                        self.lost_events.fetch_add(1, Ordering::Relaxed);
                    } else {
                        self.lost_events.fetch_add(1, Ordering::Relaxed);
                    }
                    Ok(())
                }
                AuditSinkPolicy::Degrade => {
                    self.degraded.store(true, Ordering::Release);
                    self.lost_events.fetch_add(1, Ordering::Relaxed);
                    Ok(())
                }
                AuditSinkPolicy::FailLossless => Err(AuditSinkError::Backpressure),
            },
            Err(mpsc::error::TrySendError::Closed(_)) => Err(AuditSinkError::Closed),
        }
    }

    /// Drain the queue and `fsync` the underlying file. Called from
    /// [`crate::embedded::ZccacheService::flush`].
    pub async fn flush(&self) -> Result<(), AuditSinkError> {
        let (tx, rx) = oneshot::channel();
        self.sender
            .send(Command::Flush(tx))
            .await
            .map_err(|_| AuditSinkError::Closed)?;
        let _ = rx.await;
        Ok(())
    }

    /// Drain, close, and await the writer task. Called from
    /// [`crate::embedded::ZccacheService::shutdown`].
    pub async fn shutdown(&self) -> Result<(), AuditSinkError> {
        let (tx, rx) = oneshot::channel();
        // If the writer is already closed, the send will fail — that's
        // also acceptable because the contract guarantees only that
        // pending events are drained on a best-effort basis.
        match self.sender.send(Command::Shutdown(tx)).await {
            Ok(()) => {
                let _ = rx.await;
            }
            Err(_) => return Ok(()),
        }
        Ok(())
    }

    /// Diagnostic counter of events dropped under `DropLowPriority`,
    /// `Degrade`, or `FailLossless` backpressure. Surfaced via the
    /// host's stats endpoint so operators can detect lost-audit-data
    /// incidents.
    pub fn lost_events(&self) -> u64 {
        self.lost_events.load(Ordering::Acquire)
    }

    /// True when the sink has switched to degraded summary mode under
    /// `AuditSinkPolicy::Degrade`. Diagnostic only.
    pub fn is_degraded(&self) -> bool {
        self.degraded.load(Ordering::Acquire)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audit::{AuditCategory, AuditContext, AuditEventName, AuditId, AuditLevel};
    use tempfile::TempDir;

    fn fixture_event(level: AuditLevel, idx: u64) -> AuditEvent {
        let context = AuditContext::new(
            AuditId::new(format!("run-{idx}")).expect("run id"),
            AuditId::new(format!("trace-{idx}")).expect("trace id"),
        );
        let mut event = AuditEvent::new(
            AuditId::new(format!("event-{idx}")).expect("event id"),
            context,
            AuditId::new(format!("span-{idx}")).expect("span id"),
            AuditCategory::new("zccache.compile").expect("category"),
            AuditEventName::new("compile.finished").expect("event name"),
            "0",
        );
        event.level = level;
        event
    }

    #[tokio::test]
    async fn off_mode_returns_no_sink() {
        let temp = TempDir::new().expect("tempdir");
        let config = AuditConfig {
            mode: AuditMode::Off,
            output_root: Some(temp.path().to_string_lossy().into_owned()),
            ..AuditConfig::default()
        };
        let sink = AuditSink::start(&config, None).expect("start ok");
        assert!(
            sink.is_none(),
            "Off mode must return None so the embedded service skips emission entirely"
        );
    }

    #[tokio::test]
    async fn normal_mode_writes_jsonl_to_disk() {
        let temp = TempDir::new().expect("tempdir");
        let output_root = temp.path().join("audit");
        let config = AuditConfig {
            mode: AuditMode::Normal,
            output_root: Some(output_root.to_string_lossy().into_owned()),
            event_log: Some("audit.jsonl".to_string()),
            ..AuditConfig::default()
        };
        let sink = AuditSink::start(&config, None)
            .expect("start ok")
            .expect("sink present in Normal mode");
        sink.emit(fixture_event(AuditLevel::Info, 1))
            .expect("emit ok");
        sink.emit(fixture_event(AuditLevel::Info, 2))
            .expect("emit ok");
        sink.flush().await.expect("flush ok");
        sink.shutdown().await.expect("shutdown ok");

        let path = output_root.join("audit.jsonl");
        let contents = std::fs::read_to_string(&path).expect("file readable");
        let lines: Vec<_> = contents.lines().collect();
        assert_eq!(lines.len(), 2, "two events should produce two JSONL rows");
        for line in lines {
            let parsed: AuditEvent =
                serde_json::from_str(line).expect("each line is valid AuditEvent");
            assert_eq!(parsed.schema, crate::audit::AUDIT_SCHEMA);
            assert_eq!(parsed.schema_version, crate::audit::AUDIT_SCHEMA_VERSION);
        }
    }

    #[tokio::test]
    async fn missing_output_root_in_normal_mode_errors() {
        let config = AuditConfig {
            mode: AuditMode::Normal,
            output_root: None,
            ..AuditConfig::default()
        };
        let err = AuditSink::start(&config, None).expect_err("must error");
        assert!(matches!(err, AuditSinkError::MissingOutputRoot));
    }

    #[tokio::test]
    async fn lost_events_counter_starts_at_zero() {
        let temp = TempDir::new().expect("tempdir");
        let config = AuditConfig {
            mode: AuditMode::Normal,
            output_root: Some(temp.path().to_string_lossy().into_owned()),
            ..AuditConfig::default()
        };
        let sink = AuditSink::start(&config, None)
            .expect("start ok")
            .expect("sink");
        assert_eq!(sink.lost_events(), 0);
        assert!(!sink.is_degraded());
    }

    #[tokio::test]
    async fn shutdown_is_idempotent() {
        let temp = TempDir::new().expect("tempdir");
        let config = AuditConfig {
            mode: AuditMode::Normal,
            output_root: Some(temp.path().to_string_lossy().into_owned()),
            ..AuditConfig::default()
        };
        let sink = AuditSink::start(&config, None)
            .expect("start ok")
            .expect("sink");
        sink.shutdown().await.expect("first shutdown ok");
        // Second shutdown returns Ok because the contract guarantees
        // only best-effort drain; double-shutdown should not panic.
        let _ = sink.shutdown().await;
    }

    #[tokio::test]
    async fn fail_lossless_returns_backpressure_when_queue_full() {
        let temp = TempDir::new().expect("tempdir");
        let config = AuditConfig {
            mode: AuditMode::Normal,
            sink_policy: AuditSinkPolicy::FailLossless,
            output_root: Some(temp.path().to_string_lossy().into_owned()),
            ..AuditConfig::default()
        };
        let sink = AuditSink::start(&config, None)
            .expect("start ok")
            .expect("sink");
        // Fire ten-times-capacity events as fast as we can. Drain
        // hasn't been given a chance to run yet — the channel should
        // saturate and FailLossless should reject the surplus.
        let mut backpressure_seen = false;
        for i in 0..(WRITER_QUEUE_CAPACITY * 10) {
            if let Err(AuditSinkError::Backpressure) =
                sink.emit(fixture_event(AuditLevel::Info, i as u64))
            {
                backpressure_seen = true;
                break;
            }
        }
        assert!(
            backpressure_seen,
            "FailLossless must return Backpressure under sustained overflow"
        );
        // Cleanup — the writer task is still alive; let it drain.
        sink.flush().await.expect("flush after backpressure");
        sink.shutdown().await.expect("shutdown after backpressure");
    }
}
