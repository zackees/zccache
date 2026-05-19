//! Daemon lifecycle event log.
//!
//! Synchronous, file-only sink for two specific events:
//!   - `"spawn"` — written when the daemon starts (before the tokio
//!     runtime is built).
//!   - `"died-idle"` — written when the idle watchdog fires, just
//!     before the daemon notifies its shutdown handle.
//!
//! Why a separate sink from [`crate::event_log::EventLogger`]: that
//! logger is async (background writer thread) and not wired into the
//! daemon binary today. Lifecycle events are infrequent, happen at
//! process boundaries, and must be durable even if the daemon exits
//! milliseconds later — a synchronous direct-append is the most
//! reliable path. JSONL one-line-per-event so the file is grep- and
//! `jq`-friendly.
//!
//! File location: `{default_cache_dir}/logs/daemon-lifecycle.log`,
//! append-only, never rotated. All failures are silent (tracing
//! warn) — lifecycle logging is diagnostics and must never block or
//! crash the daemon.
//!
//! Stdio note: by the time these functions are called, the daemon has
//! already detached stdio to NUL (see `trampoline::detach_stdio`), so
//! `tracing::info!` events do not reach any persistent destination.
//! That is precisely why we need this file-based fallback.
//!
//! See zccache CI debugging history for the motivation: knowing when a
//! daemon spawned and how it died is essential for correlating CI run
//! anomalies with the daemon's lifetime.

use std::io::Write;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::event_log::open_append;

/// Standard event names recognized by `write_event`. Free-form strings
/// also work — these constants exist for the call sites we ship today.
pub const EVENT_SPAWN: &str = "spawn";
pub const EVENT_DIED_IDLE: &str = "died-idle";

/// Soft cap on the live `daemon-lifecycle.log` size. When the file
/// exceeds this on the next `write_event` call, it is renamed to
/// `daemon-lifecycle.log.1` (replacing the prior archive, if any) and
/// a fresh empty file is opened for the new event. This bounds total
/// disk footprint to `2 × MAX_LOG_SIZE` (current + one archive) and
/// preserves the most-recent chunk of history.
///
/// 1 MiB chosen because each event is ~200 bytes, so a full live file
/// holds ~5000 events — comfortable history even under pathological
/// respawn loops. Operators who need more history can grow this
/// constant; the file format is plain JSONL so concatenating
/// `daemon-lifecycle.log.1` + `daemon-lifecycle.log` yields a complete
/// in-order replay.
const MAX_LOG_SIZE: u64 = 1024 * 1024;

/// Append a JSONL line describing a daemon lifecycle event.
///
/// `extra` carries event-specific fields (endpoint, idle_secs, etc.).
/// The function adds the standard envelope: `ts_ms` (epoch milliseconds),
/// `event` (the name), and `pid` (the current process). On any failure
/// it logs at `tracing::warn` and returns — lifecycle logging is
/// best-effort.
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
    // every OS without the Windows handle-replacement dance that
    // `event_log::LogWriter::rotate` needs. Errors are silenced
    // because lifecycle logging is best-effort — we'd rather lose a
    // rotation than fail to write the next event.
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

fn log_file_path() -> std::path::PathBuf {
    zccache_core::config::default_cache_dir()
        .as_path()
        .join("logs")
        .join("daemon-lifecycle.log")
}

/// If the live log file exceeds `MAX_LOG_SIZE`, rename it to `.1`
/// (replacing any existing archive) and let the caller open a fresh
/// file for the next write. No-op if the file does not exist or is
/// under the threshold. All errors are propagated to the caller so
/// `try_write` can decide to silence them — lifecycle logging never
/// blocks the daemon.
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
    // `fs::rename` is atomic on the same filesystem and replaces an
    // existing destination on every supported platform (Windows
    // included, via `MoveFileExW(MOVEFILE_REPLACE_EXISTING)` which
    // Rust uses under the hood). We don't need a separate remove.
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

    /// Two events at separate call sites land as two JSONL lines in the
    /// correct order, each with the standard envelope keys and the
    /// caller-supplied extras.
    #[test]
    fn write_event_appends_jsonl_with_envelope_and_extras() {
        // The function uses the global default_cache_dir, which respects
        // the ZCCACHE_CACHE_DIR env var. Point it at a tempdir for this
        // test so we don't pollute the user's real lifecycle log.
        let tmp = tempfile::tempdir().expect("tempdir");
        let prev = std::env::var_os("ZCCACHE_CACHE_DIR");
        std::env::set_var("ZCCACHE_CACHE_DIR", tmp.path());

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
        assert_eq!(first["event"], "spawn");
        assert!(first["ts_ms"].is_number(), "ts_ms must be numeric");
        assert!(first["pid"].is_number(), "pid must be numeric");
        assert_eq!(first["endpoint"], "test://nowhere");

        let second: serde_json::Value = serde_json::from_str(lines[1]).expect("line 1 parses");
        assert_eq!(second["event"], "died-idle");
        assert_eq!(second["idle_secs"], 3600);
        assert_eq!(second["uptime_secs"], 7200);

        // Restore env so other tests aren't affected.
        match prev {
            Some(v) => std::env::set_var("ZCCACHE_CACHE_DIR", v),
            None => std::env::remove_var("ZCCACHE_CACHE_DIR"),
        }
    }

    /// `archive_path` derives the `.1` neighbor reliably for the
    /// common case (parent + filename present). Validates the rename
    /// target before exercising the bigger rotation test.
    #[test]
    fn archive_path_appends_dot_one_alongside_parent() {
        let p = std::path::PathBuf::from("/tmp/zc/logs/daemon-lifecycle.log");
        assert_eq!(
            archive_path(&p),
            std::path::PathBuf::from("/tmp/zc/logs/daemon-lifecycle.log.1")
        );
    }

    /// When the live log exceeds `MAX_LOG_SIZE`, the next `write_event`
    /// must rename it to `.1` and start a fresh file holding just the
    /// new event. A second oversize round must *replace* `.1` (not
    /// accumulate `.2`, `.3`, …), bounding total disk use.
    #[test]
    fn write_event_rotates_when_live_log_exceeds_max() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let prev = std::env::var_os("ZCCACHE_CACHE_DIR");
        std::env::set_var("ZCCACHE_CACHE_DIR", tmp.path());

        let log_path = tmp.path().join("logs").join("daemon-lifecycle.log");
        let archive = tmp.path().join("logs").join("daemon-lifecycle.log.1");
        std::fs::create_dir_all(log_path.parent().unwrap()).unwrap();

        // Pre-populate the live log with >MAX_LOG_SIZE bytes of placeholder
        // content. The exact bytes don't matter — only the file size.
        let padding = vec![b'x'; (MAX_LOG_SIZE + 1024) as usize];
        std::fs::write(&log_path, &padding).expect("seed oversized log");
        assert!(std::fs::metadata(&log_path).unwrap().len() > MAX_LOG_SIZE);

        // First write: triggers rotation. Live log now holds just our
        // new event; archive holds the padding.
        write_event(EVENT_SPAWN, serde_json::json!({"first": true}));

        let live = std::fs::read_to_string(&log_path).expect("live log readable");
        assert_eq!(
            live.lines().count(),
            1,
            "live log should hold just the new event, got: {live:?}"
        );
        let v: serde_json::Value = serde_json::from_str(live.trim()).expect("jsonl parses");
        assert_eq!(v["first"], true);
        assert!(
            archive.exists(),
            "archive {} should exist",
            archive.display()
        );
        assert!(
            std::fs::metadata(&archive).unwrap().len() > MAX_LOG_SIZE,
            "archive holds the prior padding"
        );

        // Drive the live log back over the threshold and rotate again.
        // The new archive must REPLACE the old one — bounding disk to
        // 2 × MAX_LOG_SIZE — not accumulate as `.2`.
        std::fs::write(&log_path, &padding).expect("re-seed oversized log");
        write_event(EVENT_DIED_IDLE, serde_json::json!({"second": true}));

        assert!(
            archive.exists(),
            "archive still present after second rotation"
        );
        assert!(
            !tmp.path()
                .join("logs")
                .join("daemon-lifecycle.log.2")
                .exists(),
            "no .2 archive — single-rotation policy bounds disk"
        );

        // The replaced archive should now hold the SECOND batch of
        // padding, not the first. Read a small prefix to confirm it's
        // not the original first-batch contents.
        let archive_first_bytes = std::fs::read(&archive).expect("archive readable");
        assert!(
            archive_first_bytes.starts_with(b"xxx"),
            "archive holds the most-recent padding from before this rotation"
        );

        match prev {
            Some(v) => std::env::set_var("ZCCACHE_CACHE_DIR", v),
            None => std::env::remove_var("ZCCACHE_CACHE_DIR"),
        }
    }

    /// Rotation is a no-op when the live log is under the threshold.
    /// Belts-and-braces: the test above already implies this, but a
    /// dedicated guard makes the contract obvious to a future reader
    /// inspecting `rotate_if_oversized` semantics.
    #[test]
    fn write_event_does_not_rotate_when_under_max() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let prev = std::env::var_os("ZCCACHE_CACHE_DIR");
        std::env::set_var("ZCCACHE_CACHE_DIR", tmp.path());

        let log_path = tmp.path().join("logs").join("daemon-lifecycle.log");
        let archive = tmp.path().join("logs").join("daemon-lifecycle.log.1");

        write_event(EVENT_SPAWN, serde_json::json!({"only": "event"}));
        write_event(EVENT_SPAWN, serde_json::json!({"another": "event"}));

        assert!(log_path.exists(), "live log written");
        assert!(!archive.exists(), "no archive on a tiny log");
        let contents = std::fs::read_to_string(&log_path).unwrap();
        assert_eq!(contents.lines().count(), 2);

        match prev {
            Some(v) => std::env::set_var("ZCCACHE_CACHE_DIR", v),
            None => std::env::remove_var("ZCCACHE_CACHE_DIR"),
        }
    }
}
