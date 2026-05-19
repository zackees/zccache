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
use std::time::{SystemTime, UNIX_EPOCH};

use crate::event_log::open_append;

/// Standard event names recognized by `write_event`. Free-form strings
/// also work — these constants exist for the call sites we ship today.
pub const EVENT_SPAWN: &str = "spawn";
pub const EVENT_DIED_IDLE: &str = "died-idle";

/// Append a JSONL line describing a daemon lifecycle event.
///
/// `extra` carries event-specific fields (endpoint, idle_secs, etc.).
/// The function adds the standard envelope: `ts_ms` (epoch milliseconds),
/// `event` (the name), and `pid` (the current process). On any failure
/// it logs at `tracing::warn` and returns — lifecycle logging is
/// best-effort.
pub fn write_event(event_name: &str, extra: serde_json::Value) {
    if let Err(e) = try_write(event_name, &extra) {
        tracing::warn!(
            event = event_name,
            "failed to write lifecycle event: {e}"
        );
    }
}

fn try_write(event_name: &str, extra: &serde_json::Value) -> std::io::Result<()> {
    let log_path = log_file_path();
    if let Some(parent) = log_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

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

        write_event(EVENT_SPAWN, serde_json::json!({"endpoint": "test://nowhere"}));
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
}
