//! JSONL compile journal for build replay.
//!
//! Records every compile/link command with enough detail to replay the entire
//! build. One JSON object per line, written to `{cache_dir}/logs/compile_journal.jsonl`.
//!
//! Architecture: same lock-free channel + background `std::thread` pattern as
//! `EventLogger`. Serialization happens on the caller's tokio task; the
//! background thread does file I/O only. Zero contention on the hot path.

use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::Path;
use std::time::SystemTime;

use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use zccache_core::NormalizedPath;
use zccache_protocol::Response;

use crate::event_log::{format_timestamp, open_append};

/// Closed enum of `miss_reason` values written to the compile journal.
///
/// Per issue #322 the set is finite so consumers can build histograms over
/// it. The constants are `&'static str` rather than a real enum because the
/// JSON serialization is a flat string — modeling them as an enum would add
/// a `to_str()` shim with no extra type safety in the daemon. The `ALL`
/// slice is the canonical iteration order; new variants must be appended.
///
/// Mapping (the *why* of each bucket):
/// - `context_not_found` — daemon has no dep-graph context for this compile
///   unit (cold cache, first time the daemon has seen this crate).
/// - `input_fingerprint_mismatch` — source files, headers, or flags changed
///   between the cached entry and the current invocation.
/// - `no_artifact_for_key` — cache key was computed and a context existed,
///   but the on-disk artifact is gone (e.g. GC'd, never persisted).
/// - `version_skew` — compiler/toolchain or zccache schema version differs
///   from the cached entry.
/// - `uncacheable_input` — invocation parsed but is intrinsically
///   uncacheable (rustc emits PGO profile, `-C link-arg=…` host-specific,
///   etc.).
/// - `unknown` — fallback; emitted whenever the daemon detected a miss but
///   has not (yet) attributed a precise reason. Follow-up work narrows
///   `unknown` into the concrete buckets above. Consumers should still
///   treat the field as present so dashboards don't crash.
pub mod miss_reason {
    pub const CONTEXT_NOT_FOUND: &str = "context_not_found";
    pub const INPUT_FINGERPRINT_MISMATCH: &str = "input_fingerprint_mismatch";
    pub const NO_ARTIFACT_FOR_KEY: &str = "no_artifact_for_key";
    pub const VERSION_SKEW: &str = "version_skew";
    pub const UNCACHEABLE_INPUT: &str = "uncacheable_input";
    pub const UNKNOWN: &str = "unknown";

    /// Closed iteration over all documented buckets. Append-only.
    pub const ALL: &[&str] = &[
        CONTEXT_NOT_FOUND,
        INPUT_FINGERPRINT_MISMATCH,
        NO_ARTIFACT_FOR_KEY,
        VERSION_SKEW,
        UNCACHEABLE_INPUT,
        UNKNOWN,
    ];
}

/// A single journal entry serialized as one JSON line.
///
/// The fields below the legacy block are populated only when `--profile`
/// mode is wired up (see issue #256, Wave 2). All extended fields skip
/// serialization when absent so legacy journal lines remain unchanged.
#[derive(Debug, Serialize)]
pub struct JournalEntry {
    /// ISO 8601 UTC timestamp.
    pub ts: String,
    /// Outcome: "hit", "miss", "error", "link_hit", "link_miss".
    pub outcome: &'static str,
    /// Full path to compiler/tool.
    pub compiler: String,
    /// Full argument list (for replay).
    pub args: Vec<String>,
    /// Working directory.
    pub cwd: String,
    /// Environment variables as `[key, value]` pairs. Omitted when `None`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub env: Option<Vec<(String, String)>>,
    /// Process exit code (-1 for errors).
    pub exit_code: i32,
    /// Session UUID or null for ephemeral.
    pub session_id: Option<String>,
    /// Wall-clock nanoseconds.
    pub latency_ns: u128,

    // ─── Extended profile-mode fields (issue #256). ─────────────────────
    // All optional; emission is gated behind `--profile` in a follow-up PR.
    /// Crate name parsed from `--crate-name`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub crate_name: Option<String>,
    /// Canonical crate kind: one of
    /// `"lib"`, `"bin"`, `"proc-macro"`, `"build-script"`, `"test"`,
    /// `"bench"`, `"example"`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub crate_type: Option<String>,
    /// Canonical output extension: one of
    /// `"rlib"`, `"rmeta"`, `"so"`, `"dylib"`, `"exe"`, `"a"`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_ext: Option<String>,
    /// Categorical reason for `outcome: miss` (issue #322). Always populated
    /// on misses; always omitted on hits/errors. Allowed values are the
    /// finite set documented in the [`miss_reason`] module (see also
    /// `docs/journal-schema.md`). `String` (not `&'static str`) so the
    /// derive(Deserialize) round-trip used in analyzer tooling works.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub miss_reason: Option<String>,
    /// Evidence bucket — only the dimension that flipped is populated.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub miss_diff: Option<MissDiff>,
    /// Subdivided self-profile timings in nanoseconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub self_profile_ns: Option<SelfProfileNs>,
}

/// Evidence for a cache miss: only the dimension that actually changed
/// is populated. Empty vectors are omitted from the JSON entirely
/// (so an empty `MissDiff` serializes as `{}`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct MissDiff {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub changed_files: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub changed_flags: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub changed_deps: Vec<String>,
}

/// Per-compile self-profile spans, in nanoseconds (matching the
/// `_ns` convention used throughout zccache).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SelfProfileNs {
    pub hash_inputs: u128,
    pub lookup: u128,
    pub decompress: u128,
    pub store: u128,
}

/// Pre-captured request metadata for journal logging.
pub struct JournalContext {
    pub compiler: String,
    pub args: Vec<String>,
    pub cwd: String,
    pub env: Option<Vec<(String, String)>>,
    pub session_id: Option<String>,
}

impl JournalEntry {
    /// Create a new journal entry with the current UTC timestamp.
    ///
    /// `miss_reason` must be `Some(_)` when `outcome == "miss" | "link_miss"`
    /// and `None` otherwise. The canonical caller threads the value from
    /// [`extract_outcome`], which enforces this invariant. The string must
    /// be one of the constants in the [`miss_reason`] module.
    pub fn new(
        ctx: JournalContext,
        outcome: &'static str,
        exit_code: i32,
        latency_ns: u128,
        miss_reason: Option<&'static str>,
    ) -> Self {
        Self {
            ts: format_timestamp(SystemTime::now()),
            outcome,
            compiler: ctx.compiler,
            args: ctx.args,
            cwd: ctx.cwd,
            env: ctx.env,
            exit_code,
            session_id: ctx.session_id,
            latency_ns,
            crate_name: None,
            crate_type: None,
            output_ext: None,
            miss_reason: miss_reason.map(str::to_string),
            miss_diff: None,
            self_profile_ns: None,
        }
    }

    /// Issue #256: populate the extended profile-mode fields on an entry.
    ///
    /// Called only when the owning session opted in via
    /// `session-start --profile`. Pure transformation - no I/O.
    /// `spans` is owned by the compile handler; passing `None`
    /// emits a record without `self_profile_ns`.
    #[must_use]
    pub fn with_profile_fields(mut self, spans: Option<SelfProfileSpans>) -> Self {
        let derived_name = derive_crate_name(&self.args);
        let derived_type = derive_crate_type(&self.args);
        self.output_ext = derive_output_ext(derived_type).map(str::to_string);
        self.crate_type = derived_type.map(str::to_string);
        self.crate_name = derived_name;
        self.self_profile_ns = spans.map(SelfProfileSpans::finish);
        self
    }
}

/// Issue #256: accumulator for the four `self_profile_ns` span buckets.
///
/// Use `handle_compile` to time the hashing, lookup, decompress,
/// and store phases and call the matching `add_*_ns` method.
/// Buckets that never receive a sample serialize as `0`.
#[derive(Debug, Default, Clone, Copy)]
pub struct SelfProfileSpans {
    pub hash_inputs_ns: u128,
    pub lookup_ns: u128,
    pub decompress_ns: u128,
    pub store_ns: u128,
}

impl SelfProfileSpans {
    /// Freeze the accumulated counters into the wire shape.
    #[must_use]
    pub fn finish(self) -> SelfProfileNs {
        SelfProfileNs {
            hash_inputs: self.hash_inputs_ns,
            lookup: self.lookup_ns,
            decompress: self.decompress_ns,
            store: self.store_ns,
        }
    }

    /// Add to the `hash_inputs` bucket.
    pub fn add_hash_inputs_ns(&mut self, ns: u128) {
        self.hash_inputs_ns = self.hash_inputs_ns.saturating_add(ns);
    }
    /// Add to the `lookup` bucket.
    pub fn add_lookup_ns(&mut self, ns: u128) {
        self.lookup_ns = self.lookup_ns.saturating_add(ns);
    }
    /// Add to the `decompress` bucket.
    pub fn add_decompress_ns(&mut self, ns: u128) {
        self.decompress_ns = self.decompress_ns.saturating_add(ns);
    }
    /// Add to the `store` bucket.
    pub fn add_store_ns(&mut self, ns: u128) {
        self.store_ns = self.store_ns.saturating_add(ns);
    }
}

// ─── Derivation helpers (pure functions, no daemon state) ──────────────────
//
// Per issue #256 part J: these parse rustc-style argument vectors into
// the canonical strings the extended journal schema uses. They live in
// this module so the writer can call them without crossing crate
// boundaries. They are public so the eventual `--profile` plumbing
// (Wave 2) and any analyzer-side tooling can reuse them.

/// Find `--crate-name <name>` or `--crate-name=<name>` in a rustc-style
/// argument vector. Returns `None` when the flag is missing or appears
/// at the end of the vector with no following value.
#[must_use]
pub fn derive_crate_name(args: &[String]) -> Option<String> {
    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        if let Some(rest) = a.strip_prefix("--crate-name=") {
            if !rest.is_empty() {
                return Some(rest.to_string());
            }
        } else if a == "--crate-name" {
            if let Some(next) = args.get(i + 1) {
                return Some(next.clone());
            }
            return None;
        }
        i += 1;
    }
    None
}

/// Find `--crate-type <type>` or `--crate-type=<type>` and normalize to
/// one of the seven canonical kinds the schema enumerates. Returns
/// `None` if no value is present or the value is unrecognized.
///
/// Special case: cargo invokes `build.rs` as
/// `--crate-name build_script_build --crate-type bin`. When we detect
/// the build-script crate-name, the kind is reported as
/// `"build-script"` regardless of the literal `--crate-type`.
#[must_use]
pub fn derive_crate_type(args: &[String]) -> Option<&'static str> {
    if derive_crate_name(args).as_deref() == Some("build_script_build") {
        return Some("build-script");
    }

    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        let raw: Option<&str> = if let Some(rest) = a.strip_prefix("--crate-type=") {
            Some(rest)
        } else if a == "--crate-type" {
            args.get(i + 1).map(String::as_str)
        } else {
            None
        };
        if let Some(raw) = raw {
            // `--crate-type lib,rlib` is legal; take the first segment
            // since the journal field is scalar.
            let first = raw.split(',').next().unwrap_or(raw).trim();
            return match first {
                // Canonical seven (schema enum).
                "lib" => Some("lib"),
                "bin" => Some("bin"),
                "proc-macro" | "proc_macro" => Some("proc-macro"),
                "test" => Some("test"),
                "bench" => Some("bench"),
                "example" => Some("example"),
                _ => None,
            };
        }
        i += 1;
    }
    None
}

/// Map a canonical crate-type to the output-extension that rustc emits.
/// Returns `None` when the crate-type is missing or outside the
/// schema-recognized set.
#[must_use]
pub fn derive_output_ext(crate_type: Option<&str>) -> Option<&'static str> {
    match crate_type? {
        "lib" => Some("rlib"),
        "bin" | "build-script" | "test" | "bench" | "example" => Some("exe"),
        "proc-macro" => Some("so"),
        _ => None,
    }
}

/// Message sent to the background journal writer thread.
enum JournalMessage {
    /// Write a line to the global journal and optionally to a session journal.
    Entry {
        line: String,
        session_path: Option<NormalizedPath>,
    },
    /// Close a session journal file handle.
    CloseSession { path: NormalizedPath },
}

/// JSONL compile journal backed by a lock-free channel and background writer thread.
pub struct CompileJournal {
    sender: Option<mpsc::UnboundedSender<JournalMessage>>,
}

impl CompileJournal {
    /// Create a new compile journal writing to `log_dir/compile_journal.jsonl`.
    ///
    /// Spawns a background thread for all I/O. Returns `noop()` on failure.
    pub fn new(log_dir: NormalizedPath) -> Self {
        match Self::try_new(log_dir) {
            Ok(journal) => journal,
            Err(e) => {
                tracing::warn!("compile journal init failed: {e} — running without journal");
                Self::noop()
            }
        }
    }

    fn try_new(log_dir: NormalizedPath) -> std::io::Result<Self> {
        fs::create_dir_all(&log_dir)?;
        let path = log_dir.join("compile_journal.jsonl");
        let file = open_append(&path)?;

        let (tx, rx) = mpsc::unbounded_channel();

        std::thread::Builder::new()
            .name("zccache-journal".into())
            .spawn(move || journal_thread(rx, path, file))
            .map_err(std::io::Error::other)?;

        Ok(Self { sender: Some(tx) })
    }

    /// Create a no-op journal that discards all entries.
    #[must_use]
    pub fn noop() -> Self {
        Self { sender: None }
    }

    /// Log a journal entry. Serialization happens on the caller; file I/O
    /// happens on the background thread. Never blocks.
    ///
    /// If `session_path` is provided, the entry is also written to that
    /// per-session JSONL file.
    pub fn log(&self, entry: &JournalEntry, session_path: Option<&Path>) {
        if let Some(tx) = &self.sender {
            // Serialize on caller's thread (tokio task).
            match serde_json::to_string(entry) {
                Ok(line) => {
                    let _ = tx.send(JournalMessage::Entry {
                        line,
                        session_path: session_path.map(Into::into),
                    });
                }
                Err(e) => {
                    tracing::debug!("journal serialize error: {e}");
                }
            }
        }
    }

    /// Close a session journal file handle. Call this when a session ends
    /// so the background thread can release the file.
    pub fn close_session(&self, path: &Path) {
        if let Some(tx) = &self.sender {
            let _ = tx.send(JournalMessage::CloseSession { path: path.into() });
        }
    }
}

/// Maximum global journal size before rotation (50 MB).
const JOURNAL_MAX_SIZE: u64 = 50 * 1024 * 1024;
/// Maximum number of rotated journal files to keep.
const JOURNAL_MAX_FILES: usize = 3;

/// Background thread: receives journal messages and writes to files.
fn journal_thread(
    mut rx: mpsc::UnboundedReceiver<JournalMessage>,
    global_path: NormalizedPath,
    mut global_file: std::fs::File,
) {
    let mut session_files: HashMap<NormalizedPath, std::fs::File> = HashMap::new();
    let mut current_size: u64 = global_path.metadata().map(|m| m.len()).unwrap_or(0);

    while let Some(msg) = rx.blocking_recv() {
        match msg {
            JournalMessage::Entry { line, session_path } => {
                // Rotate if over size limit.
                if current_size > JOURNAL_MAX_SIZE {
                    if let Some((new_file, new_size)) = rotate_journal(&global_path) {
                        global_file = new_file;
                        current_size = new_size;
                    }
                }

                // Write to global journal.
                let line_bytes = line.len() as u64 + 1; // +1 for newline
                if writeln!(global_file, "{line}").is_err() {
                    if let Ok(f) = open_append(&global_path) {
                        global_file = f;
                        let _ = writeln!(global_file, "{line}");
                    }
                }
                current_size += line_bytes;
                // Write to session journal if requested.
                if let Some(ref path) = session_path {
                    let file = session_files.entry(path.clone()).or_insert_with(|| {
                        match open_append(path) {
                            Ok(f) => f,
                            Err(e) => {
                                tracing::debug!("session journal open error: {e}");
                                // Return a dummy that will fail writes — we'll
                                // skip silently via is_err() below.
                                open_append(path).unwrap_or_else(|_| {
                                    // Last resort: /dev/null equivalent. The HashMap
                                    // entry will be cleaned up on CloseSession.
                                    std::fs::File::open(if cfg!(windows) {
                                        "NUL"
                                    } else {
                                        "/dev/null"
                                    })
                                    .expect("cannot open null device")
                                })
                            }
                        }
                    });
                    let _ = writeln!(file, "{line}");
                }
            }
            JournalMessage::CloseSession { path } => {
                session_files.remove(&path);
            }
        }
    }
}

/// Rotate the global journal file: rename to timestamped backup, GC old backups.
/// Returns the new file handle and initial size, or `None` on failure.
fn rotate_journal(path: &Path) -> Option<(std::fs::File, u64)> {
    let ts = crate::event_log::format_timestamp(std::time::SystemTime::now()).replace(':', "-");
    let rotated = path.with_file_name(format!("compile_journal.jsonl.{ts}"));
    // Rename current file to rotated name.
    if fs::rename(path, &rotated).is_err() {
        return None;
    }
    // Open a fresh file.
    let file = open_append(path).ok()?;
    gc_journal_files(path);
    Some((file, 0))
}

/// Keep only the newest `JOURNAL_MAX_FILES` rotated journal files.
fn gc_journal_files(path: &Path) {
    let dir = match path.parent() {
        Some(d) => d,
        None => return,
    };
    let mut rotated: Vec<NormalizedPath> = fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            if name.starts_with("compile_journal.jsonl.") {
                Some(e.path().into())
            } else {
                None
            }
        })
        .collect();

    if rotated.len() <= JOURNAL_MAX_FILES {
        return;
    }

    // Sort lexicographically (timestamps sort correctly) — oldest first.
    rotated.sort();
    let to_remove = rotated.len() - JOURNAL_MAX_FILES;
    for p in rotated.into_iter().take(to_remove) {
        let _ = fs::remove_file(p);
    }
}

/// Extract outcome string, exit code, and default miss reason from a Response.
///
/// Returns `None` for non-compile/link responses (Ping, Status, etc.). The
/// third tuple element is `Some(_)` only when the outcome is `"miss"` or
/// `"link_miss"` (issue #322 acceptance criteria #1: every miss carries a
/// reason). Concrete attributions (`context_not_found`, etc.) are layered
/// on by the compile-handler in follow-up work; this function is the
/// canonical *default* so the journal-writer cannot accidentally omit the
/// field.
pub fn extract_outcome(response: &Response) -> Option<(&'static str, i32, Option<&'static str>)> {
    match response {
        Response::CompileResult {
            exit_code, cached, ..
        } => {
            if *exit_code != 0 {
                Some(("error", *exit_code, None))
            } else if *cached {
                Some(("hit", *exit_code, None))
            } else {
                Some(("miss", *exit_code, Some(miss_reason::UNKNOWN)))
            }
        }
        Response::LinkResult {
            exit_code, cached, ..
        } => {
            if *exit_code != 0 {
                Some(("error", *exit_code, None))
            } else if *cached {
                Some(("link_hit", *exit_code, None))
            } else {
                Some(("link_miss", *exit_code, Some(miss_reason::UNKNOWN)))
            }
        }
        Response::Error { .. } => Some(("error", -1, None)),
        _ => None,
    }
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    /// Poll `path` until it contains at least `expected` lines, or up to ~5 s.
    /// The journal writer is a background thread that flushes asynchronously,
    /// so a fixed sleep races on slow runners (notably Windows CI). Polling
    /// keeps the fast path fast while staying deterministic.
    fn wait_for_lines(path: &std::path::Path, expected: usize) {
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
    fn legacy_entry(
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

    #[test]
    fn test_journal_entry_serialization() {
        let entry = legacy_entry(
            "2026-03-17T10:30:00.123Z",
            "hit",
            "/usr/bin/clang++",
            vec!["-c".to_string(), "foo.cpp".to_string()],
            "/project/build",
            Some(vec![("CC".to_string(), "clang".to_string())]),
            0,
            Some("test-uuid".to_string()),
            1_234_567,
        );
        let json = serde_json::to_string(&entry).unwrap();
        assert!(json.contains("\"outcome\":\"hit\""), "json: {json}");
        assert!(
            json.contains("\"compiler\":\"/usr/bin/clang++\""),
            "json: {json}"
        );
        assert!(json.contains("\"latency_ns\":1234567"), "json: {json}");
        assert!(
            json.contains("\"env\":[[\"CC\",\"clang\"]]"),
            "json: {json}"
        );
        // Verify it's valid JSON
        let _: serde_json::Value = serde_json::from_str(&json).unwrap();
    }

    #[test]
    fn test_journal_entry_env_omitted_when_none() {
        let entry = legacy_entry(
            "2026-03-17T10:30:00.123Z",
            "miss",
            "clang",
            vec![],
            "/tmp",
            None,
            0,
            None,
            0,
        );
        let json = serde_json::to_string(&entry).unwrap();
        assert!(!json.contains("\"env\""), "env should be omitted: {json}");
        assert!(json.contains("\"session_id\":null"), "json: {json}");
    }

    #[test]
    fn test_journal_file_write() {
        let dir = tempfile::tempdir().unwrap();
        let journal = CompileJournal::new(dir.path().to_path_buf().into());

        let ctx = JournalContext {
            compiler: "/usr/bin/clang++".to_string(),
            args: vec!["-c".to_string(), "test.cpp".to_string()],
            cwd: "/project".to_string(),
            env: None,
            session_id: Some("session-1".to_string()),
        };
        let entry = JournalEntry::new(ctx, "hit", 0, 5_000_000, None);
        journal.log(&entry, None);

        // Give the background thread time to write.
        std::thread::sleep(Duration::from_millis(200));

        let content = fs::read_to_string(dir.path().join("compile_journal.jsonl")).unwrap();
        assert!(!content.is_empty(), "journal should have content");
        // Each line should be valid JSON.
        for line in content.lines() {
            let v: serde_json::Value = serde_json::from_str(line).unwrap();
            assert_eq!(v["outcome"], "hit");
            assert_eq!(v["compiler"], "/usr/bin/clang++");
        }
    }

    #[test]
    fn test_noop_journal() {
        let journal = CompileJournal::noop();
        let ctx = JournalContext {
            compiler: "clang".to_string(),
            args: vec![],
            cwd: "/tmp".to_string(),
            env: None,
            session_id: None,
        };
        let entry = JournalEntry::new(ctx, "miss", 0, 0, Some(miss_reason::UNKNOWN));
        // Should not panic.
        journal.log(&entry, None);
    }

    #[test]
    fn test_session_journal_file_write() {
        let dir = tempfile::tempdir().unwrap();
        let session_dir = dir.path().join("sessions");
        fs::create_dir_all(&session_dir).unwrap();
        let session_path = session_dir.join("test-session.jsonl");

        let journal = CompileJournal::new(dir.path().to_path_buf().into());

        let ctx = JournalContext {
            compiler: "/usr/bin/clang++".to_string(),
            args: vec!["-c".to_string(), "test.cpp".to_string()],
            cwd: "/project".to_string(),
            env: None,
            session_id: Some("test-session".to_string()),
        };
        let entry = JournalEntry::new(ctx, "miss", 0, 2_000_000, Some(miss_reason::UNKNOWN));
        journal.log(&entry, Some(&session_path));

        // Give the background thread time to write.
        std::thread::sleep(Duration::from_millis(200));

        // Global journal should have the entry.
        let global = fs::read_to_string(dir.path().join("compile_journal.jsonl")).unwrap();
        assert!(!global.is_empty(), "global journal should have content");

        // Session journal should also have the entry.
        let session = fs::read_to_string(&session_path).unwrap();
        assert!(!session.is_empty(), "session journal should have content");
        let v: serde_json::Value = serde_json::from_str(session.trim()).unwrap();
        assert_eq!(v["outcome"], "miss");
        assert_eq!(v["session_id"], "test-session");
    }

    #[test]
    fn test_close_session_releases_handle() {
        let dir = tempfile::tempdir().unwrap();
        let session_dir = dir.path().join("sessions");
        fs::create_dir_all(&session_dir).unwrap();
        let session_path = session_dir.join("close-test.jsonl");

        let journal = CompileJournal::new(dir.path().to_path_buf().into());

        let ctx = JournalContext {
            compiler: "clang".to_string(),
            args: vec![],
            cwd: "/tmp".to_string(),
            env: None,
            session_id: Some("close-test".to_string()),
        };
        let entry = JournalEntry::new(ctx, "hit", 0, 100, None);
        journal.log(&entry, Some(&session_path));
        journal.close_session(&session_path);

        std::thread::sleep(Duration::from_millis(200));

        // File should exist and have content.
        let content = fs::read_to_string(&session_path).unwrap();
        assert!(!content.is_empty());
    }

    #[test]
    fn test_extract_outcome_compile_hit() {
        let resp = Response::CompileResult {
            exit_code: 0,
            stdout: Arc::new(vec![]),
            stderr: Arc::new(vec![]),
            cached: true,
        };
        assert_eq!(extract_outcome(&resp), Some(("hit", 0, None)));
    }

    #[test]
    fn test_extract_outcome_compile_miss() {
        let resp = Response::CompileResult {
            exit_code: 0,
            stdout: Arc::new(vec![]),
            stderr: Arc::new(vec![]),
            cached: false,
        };
        assert_eq!(
            extract_outcome(&resp),
            Some(("miss", 0, Some(miss_reason::UNKNOWN)))
        );
    }

    #[test]
    fn test_extract_outcome_compile_error() {
        let resp = Response::CompileResult {
            exit_code: 1,
            stdout: Arc::new(vec![]),
            stderr: Arc::new(vec![]),
            cached: false,
        };
        assert_eq!(extract_outcome(&resp), Some(("error", 1, None)));
    }

    #[test]
    fn test_extract_outcome_link_hit() {
        let resp = Response::LinkResult {
            exit_code: 0,
            stdout: Arc::new(vec![]),
            stderr: Arc::new(vec![]),
            cached: true,
            warning: None,
        };
        assert_eq!(extract_outcome(&resp), Some(("link_hit", 0, None)));
    }

    #[test]
    fn test_extract_outcome_link_miss() {
        let resp = Response::LinkResult {
            exit_code: 0,
            stdout: Arc::new(vec![]),
            stderr: Arc::new(vec![]),
            cached: false,
            warning: None,
        };
        assert_eq!(
            extract_outcome(&resp),
            Some(("link_miss", 0, Some(miss_reason::UNKNOWN)))
        );
    }

    #[test]
    fn test_extract_outcome_error_response() {
        let resp = Response::Error {
            message: "something broke".to_string(),
        };
        assert_eq!(extract_outcome(&resp), Some(("error", -1, None)));
    }

    #[test]
    fn test_extract_outcome_non_compile() {
        assert_eq!(extract_outcome(&Response::Pong), None);
    }

    // ─── Adversarial tests ─────────────────────────────────────────────

    // --- extract_outcome edge cases ---

    #[test]
    fn test_extract_outcome_compile_cached_nonzero_exit() {
        // exit_code != 0 takes priority over cached flag
        let resp = Response::CompileResult {
            exit_code: 1,
            stdout: Arc::new(vec![]),
            stderr: Arc::new(vec![]),
            cached: true,
        };
        assert_eq!(extract_outcome(&resp), Some(("error", 1, None)));
    }

    #[test]
    fn test_extract_outcome_link_cached_nonzero_exit() {
        let resp = Response::LinkResult {
            exit_code: 2,
            stdout: Arc::new(vec![]),
            stderr: Arc::new(vec![]),
            cached: true,
            warning: None,
        };
        assert_eq!(extract_outcome(&resp), Some(("error", 2, None)));
    }

    #[test]
    fn test_extract_outcome_link_error() {
        let resp = Response::LinkResult {
            exit_code: 1,
            stdout: Arc::new(vec![]),
            stderr: Arc::new(vec![]),
            cached: false,
            warning: None,
        };
        assert_eq!(extract_outcome(&resp), Some(("error", 1, None)));
    }

    #[test]
    fn test_extract_outcome_all_non_journalable() {
        use zccache_core::NormalizedPath;
        use zccache_protocol::{DaemonStatus, LookupResult as LR, SessionStats, StoreResult as SR};

        let non_journalable: Vec<Response> = vec![
            Response::Pong,
            Response::ShuttingDown,
            Response::Status(DaemonStatus {
                version: String::new(),
                artifact_count: 0,
                cache_size_bytes: 0,
                metadata_entries: 0,
                uptime_secs: 0,
                cache_hits: 0,
                cache_misses: 0,
                total_compilations: 0,
                non_cacheable: 0,
                compile_errors: 0,
                time_saved_ms: 0,
                total_links: 0,
                link_hits: 0,
                link_misses: 0,
                link_non_cacheable: 0,
                dep_graph_contexts: 0,
                dep_graph_files: 0,
                sessions_total: 0,
                sessions_active: 0,
                cache_dir: NormalizedPath::from(""),
                dep_graph_version: 0,
                dep_graph_disk_size: 0,
                dep_graph_persisted: false,
            }),
            Response::LookupResult(LR::Miss),
            Response::StoreResult(SR::Stored),
            Response::SessionStarted {
                session_id: "x".into(),
                journal_path: None,
            },
            Response::SessionEnded { stats: None },
            Response::Cleared {
                artifacts_removed: 0,
                metadata_cleared: 0,
                dep_graph_contexts_cleared: 0,
                on_disk_bytes_freed: 0,
            },
            Response::SessionStatsResult {
                stats: Some(SessionStats {
                    duration_ms: 0,
                    compilations: 0,
                    hits: 0,
                    misses: 0,
                    non_cacheable: 0,
                    errors: 0,
                    time_saved_ms: 0,
                    unique_sources: 0,
                    bytes_read: 0,
                    bytes_written: 0,
                    phase_profile: None,
                }),
            },
        ];
        for (i, resp) in non_journalable.iter().enumerate() {
            assert_eq!(
                extract_outcome(resp),
                None,
                "variant {i} should not be journalable"
            );
        }
    }

    #[test]
    fn test_extract_outcome_negative_exit_codes() {
        let resp_neg1 = Response::CompileResult {
            exit_code: -1,
            stdout: Arc::new(vec![]),
            stderr: Arc::new(vec![]),
            cached: false,
        };
        assert_eq!(extract_outcome(&resp_neg1), Some(("error", -1, None)));

        let resp_min = Response::CompileResult {
            exit_code: i32::MIN,
            stdout: Arc::new(vec![]),
            stderr: Arc::new(vec![]),
            cached: true,
        };
        assert_eq!(extract_outcome(&resp_min), Some(("error", i32::MIN, None)));

        let resp_link_neg = Response::LinkResult {
            exit_code: -1,
            stdout: Arc::new(vec![]),
            stderr: Arc::new(vec![]),
            cached: false,
            warning: None,
        };
        assert_eq!(extract_outcome(&resp_link_neg), Some(("error", -1, None)));
    }

    // --- Serialization edge cases ---

    #[test]
    fn test_serialization_empty_fields() {
        let entry = legacy_entry("", "miss", "", vec![], "", None, 0, None, 0);
        let json = serde_json::to_string(&entry).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["compiler"], "");
        assert_eq!(v["args"].as_array().unwrap().len(), 0);
        assert_eq!(v["cwd"], "");
    }

    #[test]
    fn test_serialization_special_characters() {
        let entry = legacy_entry(
            "2026-03-17T10:30:00Z",
            "hit",
            r"C:\Program Files\LLVM\bin\clang++.exe",
            vec![
                "-DFOO=\"bar baz\"".to_string(),
                "-I/path/with spaces".to_string(),
                "file\twith\ttabs.cpp".to_string(),
            ],
            "/home/用户/项目",
            Some(vec![("PATH".to_string(), r"C:\a;C:\b".to_string())]),
            0,
            None,
            42,
        );
        let json = serde_json::to_string(&entry).unwrap();
        // Must parse back to identical values
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["compiler"], r"C:\Program Files\LLVM\bin\clang++.exe");
        assert_eq!(v["cwd"], "/home/用户/项目");
        assert_eq!(v["args"][0], "-DFOO=\"bar baz\"");
    }

    #[test]
    fn test_serialization_large_args() {
        let args: Vec<String> = (0..10_000).map(|i| format!("-DVAR_{i}=val")).collect();
        let entry = legacy_entry(
            "2026-03-17T10:30:00Z",
            "miss",
            "clang",
            args,
            "/tmp",
            None,
            0,
            None,
            0,
        );
        let json = serde_json::to_string(&entry).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["args"].as_array().unwrap().len(), 10_000);
    }

    #[test]
    fn test_serialization_u128_max_latency() {
        let entry = legacy_entry(
            "2026-03-17T10:30:00Z",
            "miss",
            "clang",
            vec![],
            "/tmp",
            None,
            0,
            None,
            u128::MAX,
        );
        // Serialization itself must succeed.
        let json = serde_json::to_string(&entry).unwrap();
        assert!(json.contains("latency_ns"));

        // But parsing back through serde_json::Value loses precision:
        // u128::MAX > 2^53, so the JSON number becomes f64, and as_u64() returns None.
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert!(
            v["latency_ns"].as_u64().is_none(),
            "u128::MAX should not round-trip through serde_json::Value as u64"
        );
        // The value exists but is a lossy float
        assert!(v["latency_ns"].is_number(), "should still be a number");
    }

    // --- JSONL integrity ---

    #[test]
    fn test_multiple_entries_valid_jsonl() {
        let dir = tempfile::tempdir().unwrap();
        let journal = CompileJournal::new(dir.path().to_path_buf().into());

        for i in 0..50 {
            let ctx = JournalContext {
                compiler: format!("clang-{i}"),
                args: vec![format!("file{i}.c")],
                cwd: "/build".to_string(),
                env: None,
                session_id: None,
            };
            let entry =
                JournalEntry::new(ctx, "miss", 0, i as u128 * 1000, Some(miss_reason::UNKNOWN));
            journal.log(&entry, None);
        }

        std::thread::sleep(Duration::from_millis(500));

        let content = fs::read_to_string(dir.path().join("compile_journal.jsonl")).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 50, "expected 50 lines, got {}", lines.len());
        for (i, line) in lines.iter().enumerate() {
            let v: serde_json::Value =
                serde_json::from_str(line).unwrap_or_else(|e| panic!("line {i} invalid JSON: {e}"));
            assert_eq!(v["outcome"], "miss");
        }
    }

    #[test]
    fn test_concurrent_logging() {
        let dir = tempfile::tempdir().unwrap();
        let journal = Arc::new(CompileJournal::new(dir.path().to_path_buf().into()));

        let mut handles = vec![];
        for t in 0..10 {
            let j = Arc::clone(&journal);
            handles.push(std::thread::spawn(move || {
                for i in 0..100 {
                    let ctx = JournalContext {
                        compiler: format!("clang-t{t}"),
                        args: vec![format!("file{i}.c")],
                        cwd: "/build".to_string(),
                        env: None,
                        session_id: Some(format!("thread-{t}")),
                    };
                    let entry = JournalEntry::new(ctx, "hit", 0, i as u128, None);
                    j.log(&entry, None);
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }

        std::thread::sleep(Duration::from_millis(500));

        let content = fs::read_to_string(dir.path().join("compile_journal.jsonl")).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(
            lines.len(),
            1000,
            "expected 1000 lines, got {}",
            lines.len()
        );
        for (i, line) in lines.iter().enumerate() {
            serde_json::from_str::<serde_json::Value>(line)
                .unwrap_or_else(|e| panic!("line {i} invalid JSON: {e}"));
        }
    }

    // --- Session journal behavior ---

    #[test]
    fn test_session_multiple_entries_same_path() {
        let dir = tempfile::tempdir().unwrap();
        let session_dir = dir.path().join("sessions");
        fs::create_dir_all(&session_dir).unwrap();
        let session_path = session_dir.join("multi-entry.jsonl");

        let journal = CompileJournal::new(dir.path().to_path_buf().into());

        for i in 0..5 {
            let ctx = JournalContext {
                compiler: format!("clang-{i}"),
                args: vec![],
                cwd: "/tmp".to_string(),
                env: None,
                session_id: Some("multi".to_string()),
            };
            let entry = JournalEntry::new(ctx, "miss", 0, i as u128, Some(miss_reason::UNKNOWN));
            journal.log(&entry, Some(&session_path));
        }

        wait_for_lines(&session_path, 5);

        let content = fs::read_to_string(&session_path).unwrap();
        assert_eq!(content.lines().count(), 5, "session should have 5 entries");
    }

    #[test]
    fn test_multiple_sessions_correct_routing() {
        let dir = tempfile::tempdir().unwrap();
        let session_dir = dir.path().join("sessions");
        fs::create_dir_all(&session_dir).unwrap();
        let path_a = session_dir.join("session-a.jsonl");
        let path_b = session_dir.join("session-b.jsonl");

        let journal = CompileJournal::new(dir.path().to_path_buf().into());

        // Interleave entries between two sessions
        for i in 0..6 {
            let (sid, path) = if i % 2 == 0 {
                ("session-a", path_a.as_path())
            } else {
                ("session-b", path_b.as_path())
            };
            let ctx = JournalContext {
                compiler: "clang".to_string(),
                args: vec![],
                cwd: "/tmp".to_string(),
                env: None,
                session_id: Some(sid.to_string()),
            };
            let entry = JournalEntry::new(ctx, "hit", 0, 0, None);
            journal.log(&entry, Some(path));
        }

        wait_for_lines(&path_a, 3);
        wait_for_lines(&path_b, 3);

        let content_a = fs::read_to_string(&path_a).unwrap();
        let content_b = fs::read_to_string(&path_b).unwrap();

        assert_eq!(
            content_a.lines().count(),
            3,
            "session-a should have 3 entries"
        );
        assert_eq!(
            content_b.lines().count(),
            3,
            "session-b should have 3 entries"
        );

        // Verify routing by session_id
        for line in content_a.lines() {
            let v: serde_json::Value = serde_json::from_str(line).unwrap();
            assert_eq!(v["session_id"], "session-a");
        }
        for line in content_b.lines() {
            let v: serde_json::Value = serde_json::from_str(line).unwrap();
            assert_eq!(v["session_id"], "session-b");
        }
    }

    #[test]
    fn test_close_session_then_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let session_dir = dir.path().join("sessions");
        fs::create_dir_all(&session_dir).unwrap();
        let session_path = session_dir.join("reopen.jsonl");

        let journal = CompileJournal::new(dir.path().to_path_buf().into());

        // Write first entry
        let ctx1 = JournalContext {
            compiler: "clang".to_string(),
            args: vec![],
            cwd: "/tmp".to_string(),
            env: None,
            session_id: Some("reopen".to_string()),
        };
        let entry1 = JournalEntry::new(ctx1, "miss", 0, 100, Some(miss_reason::UNKNOWN));
        journal.log(&entry1, Some(&session_path));

        // Close session — releases file handle
        journal.close_session(&session_path);

        // Write second entry — should re-open the file via or_insert_with
        let ctx2 = JournalContext {
            compiler: "clang".to_string(),
            args: vec![],
            cwd: "/tmp".to_string(),
            env: None,
            session_id: Some("reopen".to_string()),
        };
        let entry2 = JournalEntry::new(ctx2, "hit", 0, 200, None);
        journal.log(&entry2, Some(&session_path));

        wait_for_lines(&session_path, 2);

        let content = fs::read_to_string(&session_path).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 2, "should have 2 entries after close+reopen");

        let v0: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        let v1: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(v0["outcome"], "miss");
        assert_eq!(v1["outcome"], "hit");
    }

    // --- Noop edge case ---

    #[test]
    fn test_noop_close_session() {
        let journal = CompileJournal::noop();
        // Must not panic
        journal.close_session(Path::new("/nonexistent/session.jsonl"));
    }

    // --- Additional adversarial tests (beyond plan) ---

    #[test]
    fn test_serialization_newline_injection() {
        // Newlines in fields must not corrupt JSONL (one JSON object per line).
        // serde_json should escape them as \n in the JSON string.
        let entry = legacy_entry(
            "2026-03-17T10:30:00Z",
            "miss",
            "clang",
            vec!["-DMSG=\"line1\nline2\"".to_string()],
            "/tmp",
            None,
            0,
            None,
            0,
        );
        let json = serde_json::to_string(&entry).unwrap();
        // The serialized JSON must be a single line (no raw newlines)
        assert_eq!(
            json.lines().count(),
            1,
            "JSON output must be single-line for JSONL: {json}"
        );
        // Round-trip preserves the embedded newline
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert!(v["args"][0].as_str().unwrap().contains('\n'));
    }

    #[test]
    fn test_serialization_control_chars_and_null_bytes() {
        // Strings with control characters (including NUL) must serialize to valid JSON.
        let entry = legacy_entry(
            "2026-03-17T10:30:00Z",
            "hit",
            "clang",
            vec![
                "has\0null".to_string(),
                "has\x01ctrl".to_string(),
                "has\x7fDEL".to_string(),
            ],
            "/tmp",
            None,
            0,
            None,
            0,
        );
        let json = serde_json::to_string(&entry).unwrap();
        // Must produce valid single-line JSON
        assert_eq!(json.lines().count(), 1);
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert!(v["args"][0].as_str().unwrap().contains('\0'));
    }

    #[test]
    fn test_double_close_session() {
        let dir = tempfile::tempdir().unwrap();
        let session_dir = dir.path().join("sessions");
        fs::create_dir_all(&session_dir).unwrap();
        let session_path = session_dir.join("double-close.jsonl");

        let journal = CompileJournal::new(dir.path().to_path_buf().into());

        let ctx = JournalContext {
            compiler: "clang".to_string(),
            args: vec![],
            cwd: "/tmp".to_string(),
            env: None,
            session_id: Some("dc".to_string()),
        };
        let entry = JournalEntry::new(ctx, "hit", 0, 0, None);
        journal.log(&entry, Some(&session_path));

        // Close twice — second close removes from empty map, must not panic
        journal.close_session(&session_path);
        journal.close_session(&session_path);

        std::thread::sleep(Duration::from_millis(200));

        let content = fs::read_to_string(&session_path).unwrap();
        assert_eq!(content.lines().count(), 1);
    }

    #[test]
    fn test_extract_outcome_i32_max_exit_code() {
        let resp = Response::CompileResult {
            exit_code: i32::MAX,
            stdout: Arc::new(vec![]),
            stderr: Arc::new(vec![]),
            cached: false,
        };
        assert_eq!(extract_outcome(&resp), Some(("error", i32::MAX, None)));
    }

    #[test]
    fn test_serialization_exit_code_boundary_values() {
        // Ensure extreme exit codes serialize and round-trip correctly
        for exit_code in [i32::MIN, -1, 0, 1, 127, 255, i32::MAX] {
            let entry = legacy_entry(
                "2026-03-17T10:30:00Z",
                "error",
                "clang",
                vec![],
                "/tmp",
                None,
                exit_code,
                None,
                0,
            );
            let json = serde_json::to_string(&entry).unwrap();
            let v: serde_json::Value = serde_json::from_str(&json).unwrap();
            assert_eq!(
                v["exit_code"].as_i64().unwrap(),
                exit_code as i64,
                "exit_code {exit_code} round-trip failed"
            );
        }
    }

    #[test]
    fn test_serialization_latency_precision_boundary() {
        // serde_json::Value stores numbers as i64/u64/f64 internally.
        // Values up to u64::MAX round-trip exactly (stored as u64).
        // Values above u64::MAX (u128 range) fall back to f64 and lose precision.
        // This is better than most JSON parsers (Python/JS lose precision at 2^53).

        // u64::MAX round-trips exactly through serde_json::Value
        let entry_u64max = legacy_entry(
            "2026-03-17T10:30:00Z",
            "miss",
            "clang",
            vec![],
            "/tmp",
            None,
            0,
            None,
            u64::MAX as u128,
        );
        let json = serde_json::to_string(&entry_u64max).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(
            v["latency_ns"].as_u64(),
            Some(u64::MAX),
            "u64::MAX should round-trip exactly through serde_json::Value"
        );

        // u64::MAX + 1 does NOT round-trip (falls to f64)
        let entry_above = legacy_entry(
            "2026-03-17T10:30:00Z",
            "miss",
            "clang",
            vec![],
            "/tmp",
            None,
            0,
            None,
            u64::MAX as u128 + 1,
        );
        let json2 = serde_json::to_string(&entry_above).unwrap();
        let v2: serde_json::Value = serde_json::from_str(&json2).unwrap();
        assert!(
            v2["latency_ns"].as_u64().is_none(),
            "u64::MAX+1 should NOT parse as u64 through serde_json::Value"
        );
    }

    #[test]
    fn test_noop_log_with_session_path() {
        // Noop journal with session_path must not panic or create files
        let journal = CompileJournal::noop();
        let ctx = JournalContext {
            compiler: "clang".to_string(),
            args: vec![],
            cwd: "/tmp".to_string(),
            env: None,
            session_id: Some("x".to_string()),
        };
        let entry = JournalEntry::new(ctx, "miss", 0, 0, Some(miss_reason::UNKNOWN));
        journal.log(&entry, Some(Path::new("/nonexistent/path.jsonl")));
    }

    #[test]
    fn test_journal_rotation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("compile_journal.jsonl");

        // Create a journal file that exceeds JOURNAL_MAX_SIZE equivalent
        // by directly calling rotate_journal.
        fs::write(&path, vec![b'x'; 100]).unwrap();
        let result = rotate_journal(&path);
        assert!(result.is_some());

        // Original path should exist (fresh file).
        assert!(path.exists());

        // A rotated file should exist.
        let rotated: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with("compile_journal.jsonl.")
            })
            .collect();
        assert_eq!(rotated.len(), 1);
    }

    // ─── Profile-mode schema extension tests (issue #256 part J) ──────────

    #[test]
    fn serializes_extended_fields_when_present() {
        let entry = JournalEntry {
            ts: "2026-03-17T10:30:00.123Z".to_string(),
            outcome: "miss",
            compiler: "/usr/bin/rustc".to_string(),
            args: vec!["--crate-name".into(), "soldr_cli".into()],
            cwd: "/project".to_string(),
            env: None,
            exit_code: 0,
            session_id: Some("sess-1".to_string()),
            latency_ns: 1_234_567,
            crate_name: Some("soldr_cli".to_string()),
            crate_type: Some("bin".to_string()),
            output_ext: Some("exe".to_string()),
            miss_reason: Some("inputs".to_string()),
            miss_diff: Some(MissDiff {
                changed_files: vec!["src/main.rs".to_string(), "build.rs".to_string()],
                changed_flags: vec!["-C".to_string(), "debuginfo=2".to_string()],
                changed_deps: vec!["serde@1.0.213".to_string()],
            }),
            self_profile_ns: Some(SelfProfileNs {
                hash_inputs: 12_400_000,
                lookup: 410_000,
                decompress: 14_100_000,
                store: 203_000_000,
            }),
        };

        let json = serde_json::to_string(&entry).unwrap();

        // Legacy fields still serialize.
        assert!(json.contains("\"outcome\":\"miss\""), "json: {json}");
        assert!(
            json.contains("\"compiler\":\"/usr/bin/rustc\""),
            "json: {json}"
        );
        assert!(json.contains("\"latency_ns\":1234567"), "json: {json}");

        // Each new key appears exactly once.
        assert!(
            json.contains("\"crate_name\":\"soldr_cli\""),
            "json: {json}"
        );
        assert!(json.contains("\"crate_type\":\"bin\""), "json: {json}");
        assert!(json.contains("\"output_ext\":\"exe\""), "json: {json}");
        assert!(json.contains("\"miss_reason\":\"inputs\""), "json: {json}");
        assert!(json.contains("\"miss_diff\""), "json: {json}");
        assert!(json.contains("\"self_profile_ns\""), "json: {json}");

        // Round-trip through serde_json::Value to confirm structure.
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["crate_name"], "soldr_cli");
        assert_eq!(v["crate_type"], "bin");
        assert_eq!(v["output_ext"], "exe");
        assert_eq!(v["miss_reason"], "inputs");
        assert_eq!(v["miss_diff"]["changed_files"][0], "src/main.rs");
        assert_eq!(v["miss_diff"]["changed_files"][1], "build.rs");
        assert_eq!(v["miss_diff"]["changed_flags"][0], "-C");
        assert_eq!(v["miss_diff"]["changed_flags"][1], "debuginfo=2");
        assert_eq!(v["miss_diff"]["changed_deps"][0], "serde@1.0.213");
        assert_eq!(v["self_profile_ns"]["hash_inputs"], 12_400_000_u64);
        assert_eq!(v["self_profile_ns"]["lookup"], 410_000_u64);
        assert_eq!(v["self_profile_ns"]["decompress"], 14_100_000_u64);
        assert_eq!(v["self_profile_ns"]["store"], 203_000_000_u64);
    }

    #[test]
    fn omits_extended_fields_when_none() {
        // A legacy-shape JournalEntry (constructed via JournalEntry::new) must
        // not emit any of the new keys. This protects the default-off invariant.
        let ctx = JournalContext {
            compiler: "/usr/bin/clang++".to_string(),
            args: vec!["-c".to_string(), "test.cpp".to_string()],
            cwd: "/project".to_string(),
            env: None,
            session_id: Some("s".to_string()),
        };
        let entry = JournalEntry::new(ctx, "hit", 0, 1000, None);
        let json = serde_json::to_string(&entry).unwrap();

        for forbidden in [
            "\"crate_name\"",
            "\"crate_type\"",
            "\"output_ext\"",
            "\"miss_reason\"",
            "\"miss_diff\"",
            "\"self_profile_ns\"",
        ] {
            assert!(
                !json.contains(forbidden),
                "legacy journal must omit {forbidden}: {json}"
            );
        }
    }

    #[test]
    fn miss_diff_omits_empty_arrays() {
        // An entirely empty MissDiff must serialize as `{}` so we never burn
        // bytes on noise.
        let diff = MissDiff {
            changed_files: vec![],
            changed_flags: vec![],
            changed_deps: vec![],
        };
        let json = serde_json::to_string(&diff).unwrap();
        assert_eq!(
            json, "{}",
            "empty MissDiff should serialize as {{}}: {json}"
        );
    }

    #[test]
    fn miss_diff_round_trips_populated_arrays() {
        let diff = MissDiff {
            changed_files: vec!["src/main.rs".to_string(), "src/lib.rs".to_string()],
            changed_flags: vec!["-C".to_string(), "opt-level=3".to_string()],
            changed_deps: vec!["serde@1.0.0".to_string()],
        };
        let json = serde_json::to_string(&diff).unwrap();
        let parsed: MissDiff = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.changed_files, diff.changed_files);
        assert_eq!(parsed.changed_flags, diff.changed_flags);
        assert_eq!(parsed.changed_deps, diff.changed_deps);
    }

    #[test]
    fn miss_diff_partial_population_omits_empty() {
        // Only changed_files populated — the other two arrays must be absent
        // from the JSON (skipped, not emitted as []).
        let diff = MissDiff {
            changed_files: vec!["src/main.rs".to_string()],
            changed_flags: vec![],
            changed_deps: vec![],
        };
        let json = serde_json::to_string(&diff).unwrap();
        assert!(json.contains("\"changed_files\""), "json: {json}");
        assert!(!json.contains("\"changed_flags\""), "json: {json}");
        assert!(!json.contains("\"changed_deps\""), "json: {json}");
    }

    #[test]
    fn derive_crate_name_spaced_form() {
        let args = vec![
            "--edition".to_string(),
            "2021".to_string(),
            "--crate-name".to_string(),
            "foo".to_string(),
            "src/lib.rs".to_string(),
        ];
        assert_eq!(derive_crate_name(&args), Some("foo".to_string()));
    }

    #[test]
    fn derive_crate_name_equals_form() {
        let args = vec!["--crate-name=bar_baz".to_string(), "src/lib.rs".to_string()];
        assert_eq!(derive_crate_name(&args), Some("bar_baz".to_string()));
    }

    #[test]
    fn derive_crate_name_missing_returns_none() {
        let args = vec!["-c".to_string(), "foo.cpp".to_string()];
        assert_eq!(derive_crate_name(&args), None);
    }

    #[test]
    fn derive_crate_name_dangling_flag_returns_none() {
        // `--crate-name` with no following value must not panic / out-of-bounds.
        let args = vec!["--crate-name".to_string()];
        assert_eq!(derive_crate_name(&args), None);
    }

    #[test]
    fn derive_crate_type_lib() {
        let args = vec![
            "--crate-name".to_string(),
            "foo".to_string(),
            "--crate-type".to_string(),
            "lib".to_string(),
        ];
        assert_eq!(derive_crate_type(&args), Some("lib"));
    }

    #[test]
    fn derive_crate_type_bin() {
        let args = vec!["--crate-type".to_string(), "bin".to_string()];
        assert_eq!(derive_crate_type(&args), Some("bin"));
    }

    #[test]
    fn derive_crate_type_proc_macro_normalizes_to_hyphen() {
        // rustc accepts `proc-macro` (canonical). Confirm it stays canonical
        // (no underscore variant emitted).
        let args = vec!["--crate-type=proc-macro".to_string()];
        assert_eq!(derive_crate_type(&args), Some("proc-macro"));
    }

    #[test]
    fn derive_crate_type_build_script_via_crate_name() {
        // Cargo invokes build.rs as `--crate-name build_script_build`.
        // That overrides the literal crate-type (which would be "bin").
        let args = vec![
            "--crate-name".to_string(),
            "build_script_build".to_string(),
            "--crate-type".to_string(),
            "bin".to_string(),
        ];
        assert_eq!(derive_crate_type(&args), Some("build-script"));
    }

    #[test]
    fn derive_crate_type_missing_returns_none() {
        let args = vec!["-c".to_string(), "foo.cpp".to_string()];
        assert_eq!(derive_crate_type(&args), None);
    }

    #[test]
    fn derive_crate_type_unknown_value_returns_none() {
        // An unrecognized crate-type should be dropped, not propagated raw.
        let args = vec!["--crate-type".to_string(), "weirdo".to_string()];
        assert_eq!(derive_crate_type(&args), None);
    }

    #[test]
    fn derive_output_ext_for_each_crate_type() {
        // The full table per the issue: crate_type → output_ext.
        assert_eq!(derive_output_ext(Some("lib")), Some("rlib"));
        assert_eq!(derive_output_ext(Some("bin")), Some("exe"));
        assert_eq!(derive_output_ext(Some("proc-macro")), Some("so"));
        assert_eq!(derive_output_ext(Some("build-script")), Some("exe"));
        assert_eq!(derive_output_ext(Some("test")), Some("exe"));
        assert_eq!(derive_output_ext(Some("bench")), Some("exe"));
        assert_eq!(derive_output_ext(Some("example")), Some("exe"));
        assert_eq!(derive_output_ext(None), None);
    }

    #[test]
    fn derive_output_ext_unknown_returns_none() {
        assert_eq!(derive_output_ext(Some("nonsense")), None);
    }

    // ─── Issue #322 — miss_reason wiring ──────────────────────────────────

    #[test]
    fn miss_reason_constants_match_documented_schema() {
        // Acceptance criteria #2: the finite enum of miss reasons must be a
        // documented public surface so consumers can build histograms.
        assert_eq!(miss_reason::CONTEXT_NOT_FOUND, "context_not_found");
        assert_eq!(
            miss_reason::INPUT_FINGERPRINT_MISMATCH,
            "input_fingerprint_mismatch"
        );
        assert_eq!(miss_reason::NO_ARTIFACT_FOR_KEY, "no_artifact_for_key");
        assert_eq!(miss_reason::VERSION_SKEW, "version_skew");
        assert_eq!(miss_reason::UNCACHEABLE_INPUT, "uncacheable_input");
        assert_eq!(miss_reason::UNKNOWN, "unknown");
        // The all-values slice lets consumers iterate the closed set.
        assert!(miss_reason::ALL.contains(&miss_reason::UNKNOWN));
        assert!(miss_reason::ALL.contains(&miss_reason::CONTEXT_NOT_FOUND));
    }

    #[test]
    fn extract_outcome_compile_miss_supplies_default_reason() {
        // Acceptance criteria #1: every miss must carry a reason. The
        // canonical translation point (extract_outcome) must default to
        // miss_reason::UNKNOWN so the journal-writer side cannot forget.
        let resp = Response::CompileResult {
            exit_code: 0,
            stdout: Arc::new(vec![]),
            stderr: Arc::new(vec![]),
            cached: false,
        };
        let (outcome, exit_code, miss) = extract_outcome(&resp).unwrap();
        assert_eq!(outcome, "miss");
        assert_eq!(exit_code, 0);
        assert_eq!(miss, Some(miss_reason::UNKNOWN));
    }

    #[test]
    fn extract_outcome_link_miss_supplies_default_reason() {
        let resp = Response::LinkResult {
            exit_code: 0,
            stdout: Arc::new(vec![]),
            stderr: Arc::new(vec![]),
            cached: false,
            warning: None,
        };
        let (outcome, _exit, miss) = extract_outcome(&resp).unwrap();
        assert_eq!(outcome, "link_miss");
        assert_eq!(miss, Some(miss_reason::UNKNOWN));
    }

    #[test]
    fn extract_outcome_hit_has_no_miss_reason() {
        // miss_reason must not be set on hits — keeps legacy hit records lean.
        let resp = Response::CompileResult {
            exit_code: 0,
            stdout: Arc::new(vec![]),
            stderr: Arc::new(vec![]),
            cached: true,
        };
        let (outcome, _, miss) = extract_outcome(&resp).unwrap();
        assert_eq!(outcome, "hit");
        assert_eq!(miss, None);
    }

    #[test]
    fn extract_outcome_error_has_no_miss_reason() {
        // Errors are a distinct outcome category — no miss_reason.
        let resp = Response::CompileResult {
            exit_code: 1,
            stdout: Arc::new(vec![]),
            stderr: Arc::new(vec![]),
            cached: false,
        };
        let (outcome, _, miss) = extract_outcome(&resp).unwrap();
        assert_eq!(outcome, "error");
        assert_eq!(miss, None);
    }

    #[test]
    fn journal_entry_new_records_miss_reason() {
        // JournalEntry::new is the canonical builder used by the dispatch
        // site. It must thread miss_reason through to the field.
        let ctx = JournalContext {
            compiler: "rustc".into(),
            args: vec![],
            cwd: "/tmp".into(),
            env: None,
            session_id: None,
        };
        let entry = JournalEntry::new(ctx, "miss", 0, 0, Some(miss_reason::CONTEXT_NOT_FOUND));
        assert_eq!(entry.miss_reason.as_deref(), Some("context_not_found"));
    }

    #[test]
    fn journal_entry_new_hit_omits_miss_reason() {
        let ctx = JournalContext {
            compiler: "rustc".into(),
            args: vec![],
            cwd: "/tmp".into(),
            env: None,
            session_id: None,
        };
        let entry = JournalEntry::new(ctx, "hit", 0, 0, None);
        assert_eq!(entry.miss_reason, None);
        let json = serde_json::to_string(&entry).unwrap();
        assert!(
            !json.contains("\"miss_reason\""),
            "hit entries must omit miss_reason: {json}"
        );
    }

    #[test]
    fn journal_jsonl_miss_record_includes_miss_reason_field() {
        // Integration: writing a Response::CompileResult { cached: false } to
        // the journal must produce a JSONL line that includes "miss_reason".
        // This is the field consumers (setup-soldr) read.
        let dir = tempfile::tempdir().unwrap();
        let journal = CompileJournal::new(dir.path().to_path_buf().into());

        let ctx = JournalContext {
            compiler: "/usr/bin/rustc".into(),
            args: vec!["--crate-name".into(), "foo".into()],
            cwd: "/project".into(),
            env: None,
            session_id: None,
        };
        // Use the canonical extractor so the test exercises the same path the
        // real dispatcher does.
        let resp = Response::CompileResult {
            exit_code: 0,
            stdout: Arc::new(vec![]),
            stderr: Arc::new(vec![]),
            cached: false,
        };
        let (outcome, exit_code, miss) = extract_outcome(&resp).unwrap();
        journal.log(
            &JournalEntry::new(ctx, outcome, exit_code, 1_000_000, miss),
            None,
        );

        let path = dir.path().join("compile_journal.jsonl");
        wait_for_lines(&path, 1);
        let content = fs::read_to_string(&path).unwrap();
        let line = content.lines().next().expect("expected one journal line");
        let v: serde_json::Value = serde_json::from_str(line).unwrap();
        assert_eq!(v["outcome"], "miss");
        assert!(
            v.get("miss_reason").is_some(),
            "miss record must have miss_reason field: {line}"
        );
        // The default value is the documented 'unknown' bucket — concrete
        // attributions are layered on in follow-ups.
        assert_eq!(v["miss_reason"], "unknown");
    }

    #[test]
    fn journal_jsonl_hit_record_omits_miss_reason() {
        let dir = tempfile::tempdir().unwrap();
        let journal = CompileJournal::new(dir.path().to_path_buf().into());

        let ctx = JournalContext {
            compiler: "/usr/bin/rustc".into(),
            args: vec![],
            cwd: "/project".into(),
            env: None,
            session_id: None,
        };
        let resp = Response::CompileResult {
            exit_code: 0,
            stdout: Arc::new(vec![]),
            stderr: Arc::new(vec![]),
            cached: true,
        };
        let (outcome, exit_code, miss) = extract_outcome(&resp).unwrap();
        journal.log(
            &JournalEntry::new(ctx, outcome, exit_code, 1_000_000, miss),
            None,
        );

        let path = dir.path().join("compile_journal.jsonl");
        wait_for_lines(&path, 1);
        let content = fs::read_to_string(&path).unwrap();
        let line = content.lines().next().expect("expected one journal line");
        let v: serde_json::Value = serde_json::from_str(line).unwrap();
        assert_eq!(v["outcome"], "hit");
        assert!(
            v.get("miss_reason").is_none(),
            "hit record must omit miss_reason: {line}"
        );
    }

    #[test]
    fn test_journal_gc_keeps_max_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("compile_journal.jsonl");
        fs::write(&path, b"current").unwrap();

        // Create 5 rotated files (more than JOURNAL_MAX_FILES=3).
        for i in 0..5 {
            let rotated = dir.path().join(format!(
                "compile_journal.jsonl.2026-03-{i:02}T00-00-00.000Z"
            ));
            fs::write(&rotated, format!("data-{i}")).unwrap();
        }

        gc_journal_files(&path);

        let remaining: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with("compile_journal.jsonl.")
            })
            .collect();
        assert!(
            remaining.len() <= JOURNAL_MAX_FILES,
            "expected at most {JOURNAL_MAX_FILES} rotated files, got {}",
            remaining.len()
        );
    }

    // Issue #256 -- with_profile_fields and SelfProfileSpans.

    fn make_ctx(args: Vec<&str>) -> JournalContext {
        JournalContext {
            compiler: "/usr/bin/rustc".to_string(),
            args: args.into_iter().map(String::from).collect(),
            cwd: "/proj".to_string(),
            env: None,
            session_id: Some("session-1".to_string()),
        }
    }

    #[test]
    fn with_profile_fields_populates_crate_name_type_and_ext() {
        let ctx = make_ctx(vec![
            "--crate-name",
            "soldr_cli",
            "--crate-type",
            "bin",
            "src/main.rs",
        ]);
        let entry = JournalEntry::new(ctx, "hit", 0, 1_234, None).with_profile_fields(None);
        assert_eq!(entry.crate_name.as_deref(), Some("soldr_cli"));
        assert_eq!(entry.crate_type.as_deref(), Some("bin"));
        assert_eq!(entry.output_ext.as_deref(), Some("exe"));
        assert!(entry.self_profile_ns.is_none());
    }

    #[test]
    fn with_profile_fields_threads_self_profile_spans() {
        let ctx = make_ctx(vec!["--crate-name", "x", "--crate-type", "lib"]);
        let mut spans = SelfProfileSpans::default();
        spans.add_hash_inputs_ns(11);
        spans.add_lookup_ns(22);
        spans.add_decompress_ns(33);
        spans.add_store_ns(44);
        let entry = JournalEntry::new(ctx, "hit", 0, 0, None).with_profile_fields(Some(spans));
        let sp = entry.self_profile_ns.unwrap();
        assert_eq!(sp.hash_inputs, 11);
        assert_eq!(sp.lookup, 22);
        assert_eq!(sp.decompress, 33);
        assert_eq!(sp.store, 44);
    }

    #[test]
    fn legacy_entry_without_with_profile_fields_omits_all_new_fields() {
        // Issue #256 acceptance criterion: when --profile is OFF the
        // journal record must serialize without crate_name, crate_type,
        // output_ext, self_profile_ns, or miss_diff.
        let ctx = make_ctx(vec!["--crate-name", "x", "--crate-type", "lib"]);
        let entry = JournalEntry::new(ctx, "hit", 0, 5, None);
        let json = serde_json::to_string(&entry).unwrap();
        for absent in [
            "\"crate_name\"",
            "\"crate_type\"",
            "\"output_ext\"",
            "\"miss_diff\"",
            "\"self_profile_ns\"",
        ] {
            assert!(
                !json.contains(absent),
                "non-profile entry must omit {absent}, got: {json}"
            );
        }
    }

    #[test]
    fn profile_entry_roundtrips_through_serde() {
        // Issue #256 acceptance criterion: a journal line with every
        // extended field set must serialize and deserialize losslessly.
        let ctx = make_ctx(vec!["--crate-name", "y", "--crate-type", "proc-macro"]);
        let mut spans = SelfProfileSpans::default();
        spans.add_hash_inputs_ns(100);
        spans.add_lookup_ns(200);
        let mut entry = JournalEntry::new(ctx, "miss", 0, 999, Some(miss_reason::UNKNOWN))
            .with_profile_fields(Some(spans));
        entry.miss_diff = Some(MissDiff {
            changed_files: vec!["src/lib.rs".to_string()],
            changed_flags: vec!["-C".into(), "debuginfo=2".into()],
            changed_deps: vec!["serde@1.0.213".into()],
        });
        let json = serde_json::to_string(&entry).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["crate_name"], "y");
        assert_eq!(v["crate_type"], "proc-macro");
        assert_eq!(v["output_ext"], "so");
        assert_eq!(v["miss_reason"], "unknown");
        assert_eq!(v["miss_diff"]["changed_files"][0], "src/lib.rs");
        assert_eq!(v["miss_diff"]["changed_flags"][1], "debuginfo=2");
        assert_eq!(v["miss_diff"]["changed_deps"][0], "serde@1.0.213");
        assert_eq!(v["self_profile_ns"]["hash_inputs"], 100);
        assert_eq!(v["self_profile_ns"]["lookup"], 200);
    }

    #[test]
    fn self_profile_spans_saturate_on_overflow() {
        // Issue #256: span accumulator must not panic when a malformed
        // measurement returns u128::MAX. Saturating arithmetic preserves
        // the journal contract: a span never observes a smaller value.
        let mut spans = SelfProfileSpans::default();
        spans.add_hash_inputs_ns(u128::MAX);
        spans.add_hash_inputs_ns(5);
        assert_eq!(spans.hash_inputs_ns, u128::MAX);
    }
}
