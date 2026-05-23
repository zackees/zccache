//! JSONL compile journal for build replay.
//!
//! Records every compile/link command with enough detail to replay the entire
//! build. One JSON object per line, written to `{cache_dir}/logs/compile_journal.jsonl`.
//!
//! Architecture: same lock-free channel + background `std::thread` pattern as
//! `EventLogger`. Serialization happens on the caller's tokio task; the
//! background thread does file I/O only. Zero contention on the hot path.
//!
//! Module layout (originally a single 2K-LOC file; split per `README.md`):
//! - this `mod.rs` — public types and the [`CompileJournal`] handle
//! - `derive` — pure rustc-argv -> schema-string helpers
//! - `outcome` — `Response` -> journal-tuple translator
//! - `journal_thread` — background writer thread, rotation, GC
//! - `tests/` — all `#[cfg(test)]` tests, grouped per subject

use std::fs;
use std::path::Path;
use std::time::SystemTime;

use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use zccache_monocrate::core::NormalizedPath;

use super::event_log::{format_timestamp, open_append};

mod derive;
mod journal_thread;
mod outcome;

#[cfg(test)]
mod tests;

pub use derive::{derive_crate_name, derive_crate_type, derive_output_ext};
pub use outcome::extract_outcome;

use journal_thread::{journal_thread, JournalMessage};

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
