//! Daemon event logger with file rotation and garbage collection.
//!
//! Provides a single write path that fans out to:
//! - **Daemon log**: persistent rotating log at `{cache_dir}/logs/daemon.log`
//! - **Session log**: optional per-session file fork when `log_file` is set
//!
//! The hot path formats a log line and posts it to a lock-free `tokio::sync::mpsc`
//! unbounded channel. A dedicated background `std::thread` drains the channel
//! and performs all file I/O (writes, rotation, GC). Zero blocking, zero
//! contention on the compilation path. Logging failure never blocks compilation.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::time::{Duration, SystemTime};

use crate::core::NormalizedPath;
use crate::depgraph::{SessionId, SessionManager};
use tokio::sync::mpsc;

/// Open a file in append mode with sharing flags that allow deletion on Windows.
///
/// On Windows, Rust's default `OpenOptions` uses `FILE_SHARE_READ | FILE_SHARE_WRITE`
/// but omits `FILE_SHARE_DELETE`, which prevents any other process from deleting or
/// renaming the file while a handle is open. This helper adds `FILE_SHARE_DELETE`
/// so log files remain deletable at any time.
pub(crate) fn open_append(path: &Path) -> std::io::Result<File> {
    let mut opts = OpenOptions::new();
    opts.create(true).append(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        // FILE_SHARE_READ (0x1) | FILE_SHARE_WRITE (0x2) | FILE_SHARE_DELETE (0x4)
        opts.share_mode(0x1 | 0x2 | 0x4);
    }
    opts.open(path)
}

// ─── Public types ───────────────────────────────────────────────────────────

/// Outcome of a compilation or direct passthrough.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompileOutcome {
    Hit,
    HitFast,
    Miss,
    Direct,
    Error,
    LinkHit,
    LinkMiss,
    LinkDirect,
}

impl std::fmt::Display for CompileOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Hit => write!(f, "HIT"),
            Self::HitFast => write!(f, "HIT_FAST"),
            Self::Miss => write!(f, "MISS"),
            Self::Direct => write!(f, "DIRECT"),
            Self::Error => write!(f, "ERROR"),
            Self::LinkHit => write!(f, "LINK_HIT"),
            Self::LinkMiss => write!(f, "LINK_MISS"),
            Self::LinkDirect => write!(f, "LINK_DIRECT"),
        }
    }
}

/// A structured compilation event to log.
pub struct CompileEvent<'a> {
    pub session_id: Option<&'a str>,
    pub compiler: &'a str,
    pub args: &'a [String],
    pub cwd: &'a str,
    pub exit_code: i32,
    pub outcome: CompileOutcome,
    pub latency: Duration,
    pub reason: Option<&'a str>,
}

// ─── Internal channel message ───────────────────────────────────────────────

/// Message sent to the background writer thread via lock-free channel.
enum LogMessage {
    /// Write a formatted line to daemon.log and optionally a session log.
    Write {
        line: String,
        session_log_path: Option<NormalizedPath>,
    },
    /// Synchronization sentinel: when the writer thread receives this, all
    /// previously-queued `Write` messages have already been processed
    /// (channels are FIFO). The writer signals completion by sending `()` on
    /// the embedded ack channel, which the caller of `EventLogger::flush()`
    /// blocks on. This eliminates the sleep-based "wait for the writer to
    /// catch up" pattern in tests and any future flush call sites.
    Flush(std::sync::mpsc::SyncSender<()>),
}

// ─── EventLogger ────────────────────────────────────────────────────────────

/// Daemon event logger backed by a lock-free channel and background writer thread.
///
/// `tokio::sync::mpsc::UnboundedSender` is `Send + Sync` via an atomic linked
/// list — no `Mutex`, no contention on the hot path. The background
/// `std::thread` drains the channel and does all file I/O.
///
/// When the `EventLogger` is dropped, the sender is dropped, the channel
/// closes, and the background thread drains remaining messages before exiting.
pub struct EventLogger {
    /// `None` for the noop logger.
    sender: Option<mpsc::UnboundedSender<LogMessage>>,
}

impl EventLogger {
    /// Create a new event logger writing to `log_dir/daemon.log`.
    ///
    /// Spawns a background thread for all I/O. Returns `noop()` on failure.
    pub fn new(log_dir: NormalizedPath, max_size: u64, max_files: usize) -> Self {
        match Self::try_new(log_dir, max_size, max_files) {
            Ok(logger) => logger,
            Err(e) => {
                tracing::warn!("event logger init failed: {e} — running without daemon log");
                Self::noop()
            }
        }
    }

    fn try_new(log_dir: NormalizedPath, max_size: u64, max_files: usize) -> std::io::Result<Self> {
        fs::create_dir_all(&log_dir)?;
        let log_path = log_dir.join("daemon.log");
        let current_size = log_path.metadata().map(|m| m.len()).unwrap_or(0);
        let file = open_append(&log_path)?;

        let (tx, rx) = mpsc::unbounded_channel();

        let writer = LogWriter {
            log_dir,
            log_file: file,
            max_size,
            max_files,
            current_size,
        };

        std::thread::Builder::new()
            .name("zccache-event-log".into())
            .spawn(move || writer_thread(rx, writer))
            .map_err(std::io::Error::other)?;

        Ok(Self { sender: Some(tx) })
    }

    /// Create a no-op logger that discards all events.
    #[must_use]
    pub fn noop() -> Self {
        Self { sender: None }
    }

    /// Log a compile event to both daemon log and (if configured) session log.
    pub fn log_event(&self, sessions: &SessionManager, event: &CompileEvent<'_>) {
        let line = format_event(event);
        let session_log_path = event.session_id.and_then(|id| {
            id.parse::<SessionId>()
                .ok()
                .and_then(|sid| sessions.get(&sid).and_then(|s| s.log_file.clone()))
        });
        self.send(line, session_log_path);
    }

    /// Log a diagnostic message to both daemon log and session log.
    pub fn log_diagnostic(&self, sessions: &SessionManager, session_id: &SessionId, message: &str) {
        let line = format_diagnostic(session_id, message);
        let session_log_path = sessions.get(session_id).and_then(|s| s.log_file.clone());
        self.send(line, session_log_path);
    }

    /// Log a daemon-level event (startup, shutdown) with no session.
    pub fn log_daemon_event(&self, message: &str) {
        let ts = format_timestamp(SystemTime::now());
        let line = format!("[{ts}] [DAEMON] {message}");
        self.send(line, None);
    }

    /// Post a message to the background writer. Lock-free, never blocks.
    fn send(&self, line: String, session_log_path: Option<NormalizedPath>) {
        if let Some(tx) = &self.sender {
            let _ = tx.send(LogMessage::Write {
                line,
                session_log_path,
            });
        }
    }

    /// Block the calling thread until the background writer has processed
    /// every message queued before this call.
    ///
    /// Implemented by posting a `Flush` sentinel and waiting on the worker's
    /// acknowledgement. Because `tokio::sync::mpsc::UnboundedSender` preserves
    /// FIFO ordering, all prior `Write` messages — including the ones whose
    /// processing triggers file rotation — have already been applied to disk
    /// by the time the sentinel is observed.
    ///
    /// No-op on a noop logger (no worker thread to synchronize with).
    pub fn flush(&self) {
        let Some(tx) = &self.sender else {
            return;
        };
        let (ack_tx, ack_rx) = std::sync::mpsc::sync_channel::<()>(0);
        if tx.send(LogMessage::Flush(ack_tx)).is_err() {
            // Worker thread is gone — nothing to wait on.
            return;
        }
        // If the worker drops the sender without sending (e.g. panic during
        // shutdown), `recv()` returns Err and we fall through without blocking
        // forever.
        let _ = ack_rx.recv();
    }
}

// ─── Background writer thread ───────────────────────────────────────────────

/// State owned exclusively by the background writer thread.
struct LogWriter {
    log_dir: NormalizedPath,
    log_file: File,
    max_size: u64,
    max_files: usize,
    current_size: u64,
}

/// Entry point for the background writer thread.
///
/// Blocks on `blocking_recv()` until the channel is closed (all senders
/// dropped), then drains remaining messages and exits.
fn writer_thread(mut rx: mpsc::UnboundedReceiver<LogMessage>, mut writer: LogWriter) {
    while let Some(msg) = rx.blocking_recv() {
        match msg {
            LogMessage::Write {
                line,
                session_log_path,
            } => {
                writer.write_daemon_line(&line);
                if let Some(path) = session_log_path {
                    write_to_file(&path, &line);
                }
            }
            LogMessage::Flush(ack) => {
                // All earlier `Write` messages have been drained above —
                // signal completion. Best-effort: if the caller has already
                // dropped the receiver, send() fails and we drop the ack.
                let _ = ack.send(());
            }
        }
    }
    // Channel closed — all senders dropped. Thread exits.
}

impl LogWriter {
    fn write_daemon_line(&mut self, line: &str) {
        if let Ok(()) = writeln!(self.log_file, "{line}") {
            let n = line.len() as u64 + 1; // +1 for newline
            self.current_size += n;
            if self.current_size > self.max_size {
                self.rotate();
            }
        }
    }

    fn rotate(&mut self) {
        let log_path = self.log_dir.join("daemon.log");
        let ts = format_timestamp(SystemTime::now()).replace(':', "-");
        let rotated = self.log_dir.join(format!("daemon.log.{ts}"));

        // On Windows we must close the file handle before renaming.
        // Replace with a dummy handle, rename, then reopen.
        if let Ok(dummy) =
            OpenOptions::new()
                .write(true)
                .open(if cfg!(windows) { "NUL" } else { "/dev/null" })
        {
            self.log_file = dummy;
        }
        let _ = fs::rename(&log_path, &rotated);
        if let Ok(new_file) = open_append(&log_path) {
            self.log_file = new_file;
        }
        self.current_size = 0;
        self.gc_old_logs();
    }

    fn gc_old_logs(&self) {
        let mut rotated: Vec<NormalizedPath> = fs::read_dir(&self.log_dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| {
                let name = e.file_name().to_string_lossy().into_owned();
                if name.starts_with("daemon.log.") {
                    Some(e.path().into())
                } else {
                    None
                }
            })
            .collect();

        if rotated.len() <= self.max_files {
            return;
        }

        // Sort by name (timestamps sort lexicographically) — oldest first.
        rotated.sort();
        let to_remove = rotated.len() - self.max_files;
        for path in rotated.into_iter().take(to_remove) {
            let _ = fs::remove_file(path);
        }
    }
}

// ─── Formatting helpers ─────────────────────────────────────────────────────

/// Format a compile event into a log line (without trailing newline).
fn format_event(event: &CompileEvent<'_>) -> String {
    let ts = format_timestamp(SystemTime::now());
    let sid_tag = match event.session_id {
        Some(id) => format!("s{id}"),
        None => "eph".to_string(),
    };
    let cmd = format_command(event.compiler, event.args);
    let latency = format_latency(event.latency);
    let mut line = format!(
        "[{ts}] [{sid_tag}] [{}] {cmd} | cwd={} | exit={} | {latency}",
        event.outcome, event.cwd, event.exit_code,
    );
    if let Some(reason) = event.reason {
        line.push_str(&format!(" | reason={reason}"));
    }
    line
}

/// Format a diagnostic message into a log line.
fn format_diagnostic(session_id: &SessionId, message: &str) -> String {
    let ts = format_timestamp(SystemTime::now());
    format!("[{ts}] [DIAG] [s{session_id}] {message}")
}

/// Format a command line: compiler binary name + args.
fn format_command(compiler: &str, args: &[String]) -> String {
    let compiler_name = Path::new(compiler)
        .file_name()
        .unwrap_or_default()
        .to_string_lossy();
    let mut parts = vec![compiler_name.into_owned()];
    parts.extend(args.iter().cloned());
    parts.join(" ")
}

/// Format a Duration as a human-readable latency string.
fn format_latency(d: Duration) -> String {
    let micros = d.as_micros();
    if micros < 1_000 {
        format!("{micros}us")
    } else if micros < 1_000_000 {
        format!("{:.1}ms", micros as f64 / 1_000.0)
    } else {
        format!("{:.1}s", micros as f64 / 1_000_000.0)
    }
}

/// Format a `SystemTime` as `YYYY-MM-DDTHH:MM:SS.mmmZ` in UTC.
///
/// Manual decomposition from Unix epoch — no external dependency needed.
pub(crate) fn format_timestamp(time: SystemTime) -> String {
    let dur = time
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default();
    let total_secs = dur.as_secs();
    let millis = dur.subsec_millis();

    let days = total_secs / 86400;
    let day_secs = total_secs % 86400;
    let hour = day_secs / 3600;
    let minute = (day_secs % 3600) / 60;
    let second = day_secs % 60;

    let (year, month, day) = civil_from_days(days as i64);

    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{millis:03}Z")
}

/// Convert days since 1970-01-01 to (year, month, day).
/// Howard Hinnant's algorithm — public domain.
fn civil_from_days(days: i64) -> (i32, u32, u32) {
    let z = days + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u32;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y as i32, m, d)
}

/// Write a single line to a file (append mode). Best-effort, errors ignored.
fn write_to_file(path: &Path, line: &str) {
    if let Ok(mut f) = open_append(path) {
        let _ = writeln!(f, "{line}");
    }
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Block until the background writer thread has processed all events
    /// queued up to this point. Implemented by `EventLogger::flush()`.
    fn flush_logger(logger: &EventLogger) {
        logger.flush();
    }

    #[test]
    fn test_format_event_hit() {
        let event = CompileEvent {
            session_id: Some("42"),
            compiler: "/usr/bin/clang++",
            args: &[
                "-c".to_string(),
                "foo.cpp".to_string(),
                "-o".to_string(),
                "foo.o".to_string(),
            ],
            cwd: "/project",
            exit_code: 0,
            outcome: CompileOutcome::Hit,
            latency: Duration::from_micros(1200),
            reason: None,
        };
        let line = format_event(&event);
        assert!(line.contains("[s42]"), "should contain session tag: {line}");
        assert!(line.contains("[HIT]"), "should contain outcome: {line}");
        assert!(line.contains("clang++"), "should contain compiler: {line}");
        assert!(line.contains("foo.cpp"), "should contain args: {line}");
        assert!(line.contains("cwd=/project"), "should contain cwd: {line}");
        assert!(line.contains("exit=0"), "should contain exit code: {line}");
        assert!(line.contains("1.2ms"), "should contain latency: {line}");
    }

    #[test]
    fn test_format_event_direct_includes_full_command() {
        let event = CompileEvent {
            session_id: None,
            compiler: "/usr/bin/clang++",
            args: &[
                "-x".to_string(),
                "c++-header".to_string(),
                "pch.h".to_string(),
                "-o".to_string(),
                "pch.pch".to_string(),
            ],
            cwd: "/project",
            exit_code: 0,
            outcome: CompileOutcome::Direct,
            latency: Duration::from_millis(1200),
            reason: Some("pch-generation"),
        };
        let line = format_event(&event);
        assert!(line.contains("[eph]"), "ephemeral tag: {line}");
        assert!(line.contains("[DIRECT]"), "outcome: {line}");
        assert!(
            line.contains("-x c++-header"),
            "full args should be present: {line}"
        );
        assert!(line.contains("pch.h"), "source file: {line}");
        assert!(line.contains("reason=pch-generation"), "reason: {line}");
    }

    #[test]
    fn test_flush_is_deterministic_no_sleep_required() {
        // RED-then-GREEN regression for #146.
        //
        // The previous `flush_logger` was a blind 200ms sleep, which races with
        // the background writer on slow CI runners. `EventLogger::flush()` must
        // be a real sync primitive: queue N events, call flush(), and the file
        // contents must be on disk *immediately* — no polling, no tolerance.
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().to_path_buf();
        let logger = EventLogger::new(log_dir.clone().into(), 100, 3);

        for i in 0..20 {
            logger.log_daemon_event(&format!("event {i} with some padding to fill space"));
        }
        logger.flush();

        // No sleep, no retry. If flush() is not deterministic, this fails.
        let main_log = log_dir.join("daemon.log");
        assert!(
            main_log.exists(),
            "daemon.log must exist immediately after flush()"
        );

        let rotated: Vec<_> = fs::read_dir(&log_dir)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().starts_with("daemon.log."))
            .collect();
        assert!(
            !rotated.is_empty(),
            "at least one rotated file must be visible immediately after flush()"
        );
    }

    #[test]
    fn test_rotation_triggers_at_threshold() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().to_path_buf();
        let logger = EventLogger::new(log_dir.clone().into(), 100, 3);

        for i in 0..20 {
            logger.log_daemon_event(&format!("event {i} with some padding to fill space"));
        }
        flush_logger(&logger);

        let main_log = log_dir.join("daemon.log");
        assert!(main_log.exists(), "daemon.log should exist");

        let rotated: Vec<_> = fs::read_dir(&log_dir)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().starts_with("daemon.log."))
            .collect();
        assert!(!rotated.is_empty(), "should have at least one rotated file");
    }

    #[test]
    fn test_gc_keeps_max_files() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().to_path_buf();
        fs::create_dir_all(&log_dir).unwrap();

        // Create 10 fake rotated files.
        for i in 0..10 {
            let name = format!("daemon.log.2026-03-14T{i:02}-00-00.000Z");
            fs::write(log_dir.join(name), "old log data").unwrap();
        }
        fs::write(log_dir.join("daemon.log"), "current").unwrap();

        // Logger construction starts the thread; writing triggers rotation+GC.
        let logger = EventLogger::new(log_dir.clone().into(), 5, 3);
        logger.log_daemon_event("trigger");
        flush_logger(&logger);

        let rotated: Vec<_> = fs::read_dir(&log_dir)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().starts_with("daemon.log."))
            .collect();
        assert!(
            rotated.len() <= 4, // 3 kept + 1 from the rotation we just triggered
            "should keep at most max_files rotated files, got {}",
            rotated.len()
        );
    }

    #[test]
    fn test_session_log_fork() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("logs");
        let session_log = dir.path().join("session.log");

        let logger = EventLogger::new(log_dir.clone().into(), 10 * 1024 * 1024, 5);

        let sessions = SessionManager::new(Duration::from_secs(300));
        let sid = sessions.create(crate::depgraph::SessionConfig {
            client_pid: 42,
            working_dir: "/test".into(),
            log_file: Some(session_log.to_path_buf().into()),
            track_stats: false,
            journal_path: None,
            profile: false,
            private_env: Vec::new(),
            owner_pids: Vec::new(),
        });
        let sid_str = sid.to_string();

        let event = CompileEvent {
            session_id: Some(&sid_str),
            compiler: "/usr/bin/clang++",
            args: &["-c".to_string(), "test.cpp".to_string()],
            cwd: "/test",
            exit_code: 0,
            outcome: CompileOutcome::Hit,
            latency: Duration::from_millis(5),
            reason: None,
        };
        logger.log_event(&sessions, &event);
        flush_logger(&logger);

        let daemon_content = fs::read_to_string(log_dir.join("daemon.log")).unwrap();
        assert!(
            daemon_content.contains("[HIT]"),
            "daemon log should have event: {daemon_content}"
        );

        let session_content = fs::read_to_string(&session_log).unwrap();
        assert!(
            session_content.contains("[HIT]"),
            "session log should have event: {session_content}"
        );
    }

    #[test]
    fn test_noop_logger() {
        let logger = EventLogger::noop();
        logger.log_daemon_event("test");

        let sessions = SessionManager::new(Duration::from_secs(300));
        let event = CompileEvent {
            session_id: None,
            compiler: "clang++",
            args: &[],
            cwd: "/tmp",
            exit_code: 0,
            outcome: CompileOutcome::Miss,
            latency: Duration::from_millis(100),
            reason: None,
        };
        logger.log_event(&sessions, &event);
    }

    #[test]
    fn test_format_timestamp() {
        let time = SystemTime::UNIX_EPOCH + Duration::from_secs(1_767_225_600);
        let ts = format_timestamp(time);
        assert_eq!(ts, "2026-01-01T00:00:00.000Z");
    }

    #[test]
    fn test_format_latency_micros() {
        assert_eq!(format_latency(Duration::from_micros(500)), "500us");
    }

    #[test]
    fn test_format_latency_millis() {
        assert_eq!(format_latency(Duration::from_micros(1200)), "1.2ms");
    }

    #[test]
    fn test_format_latency_seconds() {
        assert_eq!(format_latency(Duration::from_millis(2500)), "2.5s");
    }
}
