//! Unit tests for `compile_journal`, grouped per subject so every file
//! stays well under 1,000 LOC. Originally part of a single 2,072-LOC
//! `compile_journal.rs`. See [README](README.md) for the per-file map.

use std::fs;
use std::time::{Duration, Instant};

use super::{JournalContext, JournalEntry};

mod derive;
mod entry;
mod journal_file;
mod miss_reason;
mod outcome;
mod perf;

/// Poll `path` until it contains at least `expected` lines, or up to ~5 s.
/// The journal writer is a background thread that flushes asynchronously,
/// so a fixed sleep races on slow runners (notably Windows CI). Polling
/// keeps the fast path fast while staying deterministic.
pub(super) fn wait_for_lines(path: &std::path::Path, expected: usize) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let count = fs::read_to_string(path)
            .map(|s| s.lines().count())
            .unwrap_or(0);
        if count >= expected {
            return;
        }
        if Instant::now() >= deadline {
            return;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// Helper: build a `JournalEntry` with all profile-mode extension
/// fields set to `None`, so existing tests don't have to enumerate them.
#[allow(clippy::too_many_arguments)]
pub(super) fn legacy_entry(
    ts: &str,
    outcome: &'static str,
    compiler: &str,
    args: Vec<String>,
    cwd: &str,
    env: Option<Vec<(String, String)>>,
    exit_code: i32,
    session_id: Option<String>,
    latency_ns: u128,
) -> JournalEntry {
    JournalEntry {
        ts: ts.to_string(),
        outcome,
        compiler: compiler.to_string(),
        args,
        cwd: cwd.to_string(),
        env,
        exit_code,
        session_id,
        latency_ns,
        crate_name: None,
        crate_type: None,
        output_ext: None,
        miss_reason: None,
        miss_diff: None,
        self_profile_ns: None,
    }
}

/// Build a `JournalContext` from string-literal args. Used by `entry`
/// tests that need to round-trip through `with_profile_fields`.
pub(super) fn make_ctx(args: Vec<&str>) -> JournalContext {
    JournalContext {
        compiler: "/usr/bin/rustc".to_string(),
        args: args.into_iter().map(String::from).collect(),
        cwd: "/proj".to_string(),
        env: None,
        session_id: Some("session-1".to_string()),
    }
}
