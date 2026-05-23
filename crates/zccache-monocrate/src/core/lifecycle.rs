//! Daemon lifecycle event log — shared writer for CLI + daemon.
//!
//! Synchronous, file-only sink for events that happen at process
//! boundaries (spawn attempts, idle exits, graceful shutdowns,
//! version mismatches). One JSONL line per event so the file is grep-
//! and `jq`-friendly.
//!
//! Lives in `zccache-core` so both the CLI (when it spawns or replaces
//! a daemon) and the daemon (when it starts up or exits) write to the
//! same file at `{cache_dir}/logs/daemon-lifecycle.log`. With both
//! sides writing, one parse of the file reconstructs the full daemon
//! lineage of a session — which is the diagnostic gap that zccache
//! issue #323 (multiple daemon spawns within one build) flagged.
//!
//! Each `write_event` call opens the file, writes one short JSONL
//! line, and closes it. On Linux/macOS, POSIX guarantees atomic
//! appends for writes smaller than `PIPE_BUF` (4 KiB) so concurrent
//! writers from CLI + daemon do not interleave lines. On Windows,
//! `OpenOptions::append(true)` translates to `FILE_APPEND_DATA` which
//! seeks to EOF atomically on each write — also safe under contention.
//!
//! All failures are silent (`tracing::warn`) — lifecycle logging is
//! diagnostics and must never block or crash a process.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use super::NormalizedPath;

/// Standard event names. Free-form strings also work — these
/// constants exist for the call sites we ship today.
pub const EVENT_SPAWN: &str = "spawn";
pub const EVENT_SPAWN_ATTEMPT: &str = "spawn-attempt";
pub const EVENT_DIED_IDLE: &str = "died-idle";
pub const EVENT_DIED_SHUTDOWN: &str = "died-shutdown";
pub const EVENT_VERSION_MISMATCH: &str = "version_mismatch";

/// Reasons the CLI emits with `spawn-attempt`. Matches the branches
/// in `ensure_daemon` — keep in sync.
pub const REASON_INITIAL_START: &str = "initial-start";
pub const REASON_REPLACED_STALE_VERSION: &str = "replaced-stale-version";
pub const REASON_REPLACED_COMM_ERROR: &str = "replaced-comm-error";
pub const REASON_REPLACED_UNREACHABLE: &str = "replaced-unreachable";

/// Reasons the daemon emits with `died-*` events.
pub const REASON_GRACEFUL_SHUTDOWN: &str = "graceful-shutdown";
pub const REASON_IDLE_TIMEOUT: &str = "idle-timeout";

/// Soft cap on the live `daemon-lifecycle.log` size. When the file
/// exceeds this on the next `write_event` call, it is renamed to
/// `daemon-lifecycle.log.1` (replacing the prior archive, if any) and
/// a fresh empty file is opened for the new event. This bounds total
/// disk footprint to `2 × MAX_LOG_SIZE` (current + one archive).
pub const MAX_LOG_SIZE: u64 = 1024 * 1024;

/// Filename of the live lifecycle log (no extension suffix). Exposed
/// so callers that sweep `{cache_dir}/logs/` (notably the CLI's
/// `gc_log_directory`) can skip the file the daemon may be writing
/// to and avoid clobbering in-flight events.
pub const LIVE_LOG_FILENAME: &str = "daemon-lifecycle.log";

/// Append a JSONL line describing a daemon lifecycle event.
///
/// `extra` carries event-specific fields (endpoint, idle_secs,
/// reason, etc.). The function adds the standard envelope: `ts_ms`
/// (epoch milliseconds), `event` (the name), and `pid` (the current
/// process). On any failure it logs at `tracing::warn` and returns —
/// lifecycle logging is best-effort.
pub fn write_event(event_name: &str, extra: serde_json::Value) {
    if let Err(e) = try_write(event_name, &extra) {
        tracing::warn!(event = event_name, "failed to write lifecycle event: {e}");
    }
}

fn try_write(event_name: &str, extra: &serde_json::Value) -> std::io::Result<()> {
    let log_path = log_file_path();
    if let Some(parent) = log_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    // Soft-cap rotation. We have no long-lived file handle to juggle
    // (each write opens and closes one), so a plain rename works on
    // every OS without the Windows handle-replacement dance.
    let _ = rotate_if_oversized(&log_path);

    let mut envelope = serde_json::Map::new();
    envelope.insert("ts_ms".to_string(), serde_json::Value::from(now_ms()));
    envelope.insert(
        "event".to_string(),
        serde_json::Value::from(event_name.to_string()),
    );
    envelope.insert(
        "pid".to_string(),
        serde_json::Value::from(std::process::id()),
    );
    if let serde_json::Value::Object(fields) = extra {
        for (k, v) in fields {
            envelope.insert(k.clone(), v.clone());
        }
    }

    let mut line = serde_json::to_string(&serde_json::Value::Object(envelope))?;
    line.push('\n');

    let mut file = open_append(&log_path)?;
    file.write_all(line.as_bytes())?;
    file.flush()?;
    Ok(())
}

/// Open a file in append mode with sharing flags that allow deletion
/// on Windows. Mirrors `zccache_daemon::event_log::open_append`;
/// duplicated here so `zccache-core` doesn't need to depend on the
/// daemon crate.
fn open_append(path: &Path) -> std::io::Result<std::fs::File> {
    let mut opts = std::fs::OpenOptions::new();
    opts.create(true).append(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        // FILE_SHARE_READ (0x1) | FILE_SHARE_WRITE (0x2) | FILE_SHARE_DELETE (0x4)
        opts.share_mode(0x1 | 0x2 | 0x4);
    }
    opts.open(path)
}

/// Absolute path to the live lifecycle log file.
#[must_use]
pub fn log_file_path() -> NormalizedPath {
    log_file_path_in(&super::config::log_dir())
}

/// Same as [`log_file_path`] but rooted at a caller-supplied logs
/// directory. Used by tests and by callers that already resolved a
/// non-default cache root.
#[must_use]
pub fn log_file_path_in(log_dir: &NormalizedPath) -> NormalizedPath {
    log_dir.join(LIVE_LOG_FILENAME)
}

/// If the live log file exceeds `MAX_LOG_SIZE`, rename it to `.1`
/// (replacing any existing archive). No-op if the file is missing or
/// under the threshold.
fn rotate_if_oversized(log_path: &std::path::Path) -> std::io::Result<()> {
    let size = match std::fs::metadata(log_path) {
        Ok(m) => m.len(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    if size <= MAX_LOG_SIZE {
        return Ok(());
    }
    let archive = archive_path(log_path);
    std::fs::rename(log_path, &archive)
}

fn archive_path(log_path: &std::path::Path) -> PathBuf {
    let mut name = log_path
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_default();
    name.push(".1");
    log_path
        .parent()
        .map(|p| p.join(&name))
        .unwrap_or_else(|| PathBuf::from(name))
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;
    use std::sync::{Mutex, MutexGuard};

    /// Serializes the env-var swap critical section so parallel tests
    /// don't observe each other's `ZCCACHE_CACHE_DIR`.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    struct EnvGuard {
        _lock: MutexGuard<'static, ()>,
        previous: Option<OsString>,
    }

    impl EnvGuard {
        fn set_cache_dir(value: &std::path::Path) -> Self {
            let lock = ENV_LOCK
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let previous = std::env::var_os("ZCCACHE_CACHE_DIR");
            std::env::set_var("ZCCACHE_CACHE_DIR", value);
            Self {
                _lock: lock,
                previous,
            }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match &self.previous {
                Some(value) => std::env::set_var("ZCCACHE_CACHE_DIR", value),
                None => std::env::remove_var("ZCCACHE_CACHE_DIR"),
            }
        }
    }

    #[test]
    fn write_event_appends_jsonl_with_envelope_and_extras() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let _env = EnvGuard::set_cache_dir(tmp.path());

        write_event(
            EVENT_SPAWN,
            serde_json::json!({"endpoint": "test://nowhere"}),
        );
        write_event(
            EVENT_DIED_IDLE,
            serde_json::json!({"idle_secs": 3600u64, "uptime_secs": 7200u64}),
        );

        let log_path = tmp.path().join("logs").join("daemon-lifecycle.log");
        let contents = std::fs::read_to_string(&log_path).expect("log file written");
        let lines: Vec<&str> = contents.lines().collect();
        assert_eq!(lines.len(), 2, "expected two events, got: {contents:?}");

        let first: serde_json::Value = serde_json::from_str(lines[0]).expect("line 0 parses");
        assert_eq!(first["event"], EVENT_SPAWN);
        assert!(first["ts_ms"].is_number());
        assert!(first["pid"].is_number());
        assert_eq!(first["endpoint"], "test://nowhere");

        let second: serde_json::Value = serde_json::from_str(lines[1]).expect("line 1 parses");
        assert_eq!(second["event"], EVENT_DIED_IDLE);
        assert_eq!(second["idle_secs"], 3600);
        assert_eq!(second["uptime_secs"], 7200);
    }

    #[test]
    fn archive_path_appends_dot_one_alongside_parent() {
        let p = std::path::PathBuf::from("/tmp/zc/logs/daemon-lifecycle.log");
        assert_eq!(
            archive_path(&p),
            std::path::PathBuf::from("/tmp/zc/logs/daemon-lifecycle.log.1")
        );
    }

    #[test]
    fn write_event_rotates_when_live_log_exceeds_max() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let _env = EnvGuard::set_cache_dir(tmp.path());

        let log_path = tmp.path().join("logs").join("daemon-lifecycle.log");
        let archive = tmp.path().join("logs").join("daemon-lifecycle.log.1");
        std::fs::create_dir_all(log_path.parent().unwrap()).unwrap();

        let padding = vec![b'x'; (MAX_LOG_SIZE + 1024) as usize];
        std::fs::write(&log_path, &padding).expect("seed oversized log");
        assert!(std::fs::metadata(&log_path).unwrap().len() > MAX_LOG_SIZE);

        write_event(EVENT_SPAWN, serde_json::json!({"first": true}));

        let live = std::fs::read_to_string(&log_path).expect("live log readable");
        assert_eq!(live.lines().count(), 1, "live log should hold new event");
        let v: serde_json::Value = serde_json::from_str(live.trim()).expect("jsonl parses");
        assert_eq!(v["first"], true);
        assert!(archive.exists(), "archive should exist");
        assert!(std::fs::metadata(&archive).unwrap().len() > MAX_LOG_SIZE);

        std::fs::write(&log_path, &padding).expect("re-seed oversized log");
        write_event(EVENT_DIED_IDLE, serde_json::json!({"second": true}));

        assert!(archive.exists(), "archive still present");
        assert!(
            !tmp.path()
                .join("logs")
                .join("daemon-lifecycle.log.2")
                .exists(),
            "no .2 archive — single-rotation policy"
        );
    }

    #[test]
    fn write_event_does_not_rotate_when_under_max() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let _env = EnvGuard::set_cache_dir(tmp.path());

        let log_path = tmp.path().join("logs").join("daemon-lifecycle.log");
        let archive = tmp.path().join("logs").join("daemon-lifecycle.log.1");

        write_event(EVENT_SPAWN, serde_json::json!({"only": "event"}));
        write_event(EVENT_SPAWN, serde_json::json!({"another": "event"}));

        assert!(log_path.exists());
        assert!(!archive.exists());
        let contents = std::fs::read_to_string(&log_path).unwrap();
        assert_eq!(contents.lines().count(), 2);
    }
}
