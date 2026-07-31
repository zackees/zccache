//! Opt-in bounded file sink for daemon tracing output (issue #1165, item 5).
//!
//! The daemon's `tracing` output goes to stderr and always has. That is the
//! right default — whoever supervises the process owns its stream — but it
//! left operators with no bounded option at all: redirecting stderr to a file
//! yourself grows without limit, and the daemon outlives the shell that
//! started it.
//!
//! Setting `ZCCACHE_LOG_FILE=<path>` adds a **size-capped** file sink
//! alongside stderr. It reuses the lifecycle log's rotation shape — one live
//! file plus one archive, so the footprint is bounded at `2 × cap` — because
//! that is the retention contract the rest of the daemon's logs already have,
//! and a second rotation policy would be one more thing to reason about.
//!
//! ## Operational contract
//!
//! - **stderr is unchanged and still the default.** Service managers
//!   (systemd, launchd, Windows services, CI runners) are expected to rotate
//!   the daemon's stderr; nothing here does it for them.
//! - The file sink is **additive**, not a redirect: turning it on does not
//!   take output away from a supervisor already collecting stderr.
//! - Writes are best-effort. A failing sink degrades to "no file output" and
//!   never blocks or crashes the daemon — diagnostics must not be able to
//!   take down the process they are diagnosing.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// Path of the optional bounded file sink.
pub const LOG_FILE_ENV: &str = "ZCCACHE_LOG_FILE";

/// Override for the per-file cap, in bytes.
pub const LOG_FILE_MAX_BYTES_ENV: &str = "ZCCACHE_LOG_FILE_MAX_BYTES";

/// Default cap for the live file. With one archive kept, the sink's total
/// footprint is bounded at twice this. Chosen an order of magnitude above the
/// lifecycle log's 1 MiB because this carries full `tracing` output, not one
/// JSON line per process-boundary event.
const DEFAULT_MAX_BYTES: u64 = 16 * 1024 * 1024;

/// Suffix of the single retained archive.
const ARCHIVE_SUFFIX: &str = ".1";

/// A size-capped append sink.
///
/// Each write opens, appends and closes, mirroring `lifecycle::write_event`.
/// That is deliberately unoptimised: this sink is opt-in diagnostics rather
/// than a hot path, and per-write open/append/close is what makes concurrent
/// writers safe without holding a file handle across the daemon's lifetime.
pub struct BoundedFileSink {
    path: PathBuf,
    max_bytes: u64,
    /// Serializes the check-size-then-rotate sequence so two threads cannot
    /// both decide to rotate and lose the archive.
    rotate_lock: Mutex<()>,
}

impl BoundedFileSink {
    /// Build a sink from the environment, or `None` when `ZCCACHE_LOG_FILE`
    /// is unset or empty.
    pub fn from_env() -> Option<Self> {
        Self::from_env_with(|name| std::env::var(name).ok())
    }

    fn from_env_with<F>(lookup: F) -> Option<Self>
    where
        F: Fn(&str) -> Option<String>,
    {
        let path = lookup(LOG_FILE_ENV)?;
        let path = path.trim();
        if path.is_empty() {
            return None;
        }
        // An unparseable or zero cap falls back to the default rather than
        // disabling the bound: the whole reason this sink exists is that
        // unbounded growth is the bug.
        let max_bytes = lookup(LOG_FILE_MAX_BYTES_ENV)
            .and_then(|raw| raw.trim().parse::<u64>().ok())
            .filter(|bytes| *bytes > 0)
            .unwrap_or(DEFAULT_MAX_BYTES);
        Some(Self::new(PathBuf::from(path), max_bytes))
    }

    fn new(path: PathBuf, max_bytes: u64) -> Self {
        Self {
            path,
            max_bytes,
            rotate_lock: Mutex::new(()),
        }
    }

    /// Rotate the live file to `.1` once it exceeds the cap, replacing any
    /// previous archive.
    fn rotate_if_oversized(&self) {
        let _guard = self
            .rotate_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Ok(metadata) = std::fs::metadata(&self.path) else {
            return;
        };
        if metadata.len() <= self.max_bytes {
            return;
        }
        let _ = std::fs::rename(&self.path, archive_path(&self.path));
    }

    fn append(&self, buf: &[u8]) {
        self.rotate_if_oversized();
        if let Some(parent) = self.path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
        {
            let _ = file.write_all(buf);
        }
    }
}

fn archive_path(path: &Path) -> PathBuf {
    let mut name = path
        .file_name()
        .map(std::ffi::OsString::from)
        .unwrap_or_default();
    name.push(ARCHIVE_SUFFIX);
    path.parent()
        .map(|parent| parent.join(&name))
        .unwrap_or_else(|| PathBuf::from(name))
}

/// One `tracing` event's worth of bytes.
///
/// `tracing_subscriber` writes an event in several calls, so buffering until
/// `flush`/drop is what keeps a single event on a single line in the file.
pub struct SinkWriter<'a> {
    sink: &'a BoundedFileSink,
    buffer: Vec<u8>,
}

impl Write for SinkWriter<'_> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.buffer.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        if !self.buffer.is_empty() {
            self.sink.append(&self.buffer);
            self.buffer.clear();
        }
        Ok(())
    }
}

impl Drop for SinkWriter<'_> {
    fn drop(&mut self) {
        let _ = self.flush();
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for BoundedFileSink {
    type Writer = SinkWriter<'a>;

    fn make_writer(&'a self) -> Self::Writer {
        SinkWriter {
            sink: self,
            buffer: Vec::with_capacity(256),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_sink_is_off_unless_a_path_is_configured() {
        assert!(BoundedFileSink::from_env_with(|_| None).is_none());
        assert!(BoundedFileSink::from_env_with(|_| Some("   ".to_string())).is_none());
    }

    #[test]
    fn an_unparseable_or_zero_cap_falls_back_to_the_default() {
        // Zero would mean "unbounded" if taken literally, which is the bug
        // this sink exists to fix.
        for raw in ["0", "banana", ""] {
            let sink = BoundedFileSink::from_env_with(|name| {
                Some(if name == LOG_FILE_ENV { "log.txt" } else { raw }.to_string())
            })
            .expect("a configured path enables the sink");
            assert_eq!(sink.max_bytes, DEFAULT_MAX_BYTES);
        }
    }

    /// The load-bearing property: the file stops growing. Without the cap an
    /// operator who points the daemon at a file gets unbounded growth, which
    /// is the finding.
    #[test]
    fn the_live_file_rotates_once_it_exceeds_the_cap() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("daemon.log");
        let sink = BoundedFileSink::new(path.clone(), 64);

        for _ in 0..50 {
            sink.append(b"a line of daemon output that is not especially short\n");
        }

        let live = std::fs::metadata(&path).expect("a live file exists").len();
        assert!(
            live <= 64 + 128,
            "the live file must be near the cap, not the whole history: {live} bytes"
        );
        let archive = std::fs::metadata(archive_path(&path))
            .expect("exactly one archive is retained")
            .len();
        assert!(archive > 0);
        // Total footprint stays bounded: live + one archive, never a third.
        let files = std::fs::read_dir(dir.path()).unwrap().count();
        assert_eq!(files, 2, "one live file plus exactly one archive");
    }

    #[test]
    fn a_sink_under_the_cap_keeps_everything_in_one_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("daemon.log");
        let sink = BoundedFileSink::new(path.clone(), 1024);

        sink.append(b"first\n");
        sink.append(b"second\n");

        let contents = std::fs::read_to_string(&path).unwrap();
        assert_eq!(contents, "first\nsecond\n");
        assert!(!archive_path(&path).exists(), "nothing to archive yet");
    }

    /// Diagnostics must never take down the process they are diagnosing.
    #[test]
    fn an_unwritable_path_is_silently_tolerated() {
        let dir = tempfile::tempdir().unwrap();
        // A path whose parent is a *file* cannot be created.
        let blocker = dir.path().join("not-a-dir");
        std::fs::write(&blocker, b"x").unwrap();
        let sink = BoundedFileSink::new(blocker.join("daemon.log"), 1024);

        sink.append(b"this must not panic\n");
    }

    /// A whole event must land as one write, not be interleaved with another
    /// thread's by the per-call `write`s `tracing_subscriber` makes.
    #[test]
    fn a_writer_buffers_until_flush() {
        use tracing_subscriber::fmt::MakeWriter as _;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("daemon.log");
        let sink = BoundedFileSink::new(path.clone(), 1024);

        {
            let mut writer = sink.make_writer();
            writer.write_all(b"half ").unwrap();
            assert!(
                !path.exists(),
                "nothing should reach disk before the event completes"
            );
            writer.write_all(b"an event\n").unwrap();
        }

        assert_eq!(std::fs::read_to_string(&path).unwrap(), "half an event\n");
    }
}
