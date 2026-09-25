//! File I/O for the dependency-graph snapshot: save, load, and structured
//! classification of load outcomes for the daemon's startup path.
//!
//! ## Format (v8, zccache#1661)
//!
//! `ZCDG` magic + `DEPGRAPH_VERSION` (LE u32) + payload length (LE u64),
//! followed by a bincode 1 (fixint, little-endian) encoding of
//! [`DepGraphSnapshot`]. Save streams through `serialize_into(BufWriter)` into
//! a tmp file, fsyncs, then atomically renames; load streams through
//! `deserialize_from(BufReader)` with a byte limit equal to the declared
//! payload length. Every failure is an `Err`, never a panic, and a failed
//! save removes its tmp file and leaves the previous snapshot untouched.
//!
//! ## Bounding (zccache#1661)
//!
//! - **TTL — [`GC_TTL`] = 7 days.** Each context persists a wall-clock
//!   `last_accessed_unix_ms`, so ages survive daemon restarts (soldr
//!   restarts the daemon 34-138x/day; before v8 every load re-stamped
//!   `Instant::now()` and nothing was ever trimmed). 7 days rather than the
//!   old 1 day because the age is now real: a project built once a week
//!   keeps its warm depgraph.
//! - **Size budget — [`SNAPSHOT_BUDGET_BYTES`] = 256 MiB.** If the encoded
//!   snapshot would exceed it, least-recently-used contexts are evicted
//!   (from the snapshot *and* the live graph) until it fits; the most
//!   recently used contexts are kept.

use std::io::{BufReader, BufWriter, Read, Write};
use std::path::Path;
use std::time::Duration;

use bincode::Options;
use zccache_core::NormalizedPath;

use super::super::context::ContextKey;
use super::super::graph::DepGraph;

use super::{
    now_unix_ms, DepGraphSnapshot, SnapshotError, DEPGRAPH_MAGIC, DEPGRAPH_VERSION, HEADER_SIZE,
};

/// Contexts not accessed within this wall-clock age are trimmed before
/// persisting and dropped on load. See the module docs for why 7 days.
pub const GC_TTL: Duration = Duration::from_secs(7 * 86_400);

/// Upper bound on the on-disk snapshot size (header + payload). See the
/// module docs.
pub const SNAPSHOT_BUDGET_BYTES: u64 = 256 * 1024 * 1024;

/// Most LRU-eviction rounds attempted before writing whatever remains.
/// Each round re-measures after evicting, so this is only a backstop.
const MAX_EVICTION_ROUNDS: usize = 16;

/// Pending injected save failures (see [`inject_save_failures_for_tests`]):
/// `(path prefix, remaining count)`. Scoped to a path prefix so a test that
/// injects failures cannot break unrelated saves running concurrently in the
/// same process (e.g. under `cargo test`, which runs tests as threads).
static INJECTED_SAVE_FAILURES: std::sync::Mutex<Option<(String, usize)>> =
    std::sync::Mutex::new(None);

/// Test seam: make the next `count` saves whose target path lies under
/// `scope` fail *after* the tmp file has been created, exactly like a
/// mid-write I/O error. Saves to other paths are unaffected. Pass 0 to clear.
#[doc(hidden)]
pub fn inject_save_failures_for_tests(scope: &Path, count: usize) {
    let mut slot = INJECTED_SAVE_FAILURES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    *slot = (count > 0).then(|| (scope.to_string_lossy().into_owned(), count));
}

fn take_injected_failure(path: &Path) -> bool {
    let mut slot = INJECTED_SAVE_FAILURES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some((scope, remaining)) = slot.as_mut() else {
        return false;
    };
    if !path.starts_with(Path::new(scope.as_str())) {
        return false;
    }
    *remaining -= 1;
    if *remaining == 0 {
        *slot = None;
    }
    true
}

/// Options for [`save_to_file_with`]. [`Default`] uses the real clock,
/// [`GC_TTL`] and [`SNAPSHOT_BUDGET_BYTES`].
#[derive(Debug, Clone)]
pub struct SaveOptions {
    /// Wall-clock "now" (Unix epoch milliseconds).
    pub now_unix_ms: u64,
    /// Age beyond which contexts are trimmed before saving.
    pub ttl: Duration,
    /// Maximum snapshot file size in bytes (header + payload).
    pub budget_bytes: u64,
    /// Test seam: fail after creating the tmp file.
    #[doc(hidden)]
    pub fail_injection: bool,
}

impl Default for SaveOptions {
    fn default() -> Self {
        Self {
            now_unix_ms: now_unix_ms(),
            ttl: GC_TTL,
            budget_bytes: SNAPSHOT_BUDGET_BYTES,
            fail_injection: false,
        }
    }
}

/// Options for [`load_from_file_with`]. [`Default`] uses the real clock and
/// [`GC_TTL`].
#[derive(Debug, Clone)]
pub struct LoadOptions {
    /// Wall-clock "now" (Unix epoch milliseconds).
    pub now_unix_ms: u64,
    /// Contexts whose persisted age exceeds this are dropped on load.
    pub ttl: Duration,
}

impl Default for LoadOptions {
    fn default() -> Self {
        Self {
            now_unix_ms: now_unix_ms(),
            ttl: GC_TTL,
        }
    }
}

/// The bincode 1 configuration used for the payload: fixed-width integers,
/// little-endian (same wire shape as `bincode::serialize`).
fn codec() -> impl Options {
    bincode::DefaultOptions::new()
        .with_fixint_encoding()
        .allow_trailing_bytes()
}

fn encoded_len(snapshot: &DepGraphSnapshot) -> Result<u64, SnapshotError> {
    codec()
        .serialized_size(snapshot)
        .map_err(|e| SnapshotError::Corrupt(format!("serialize: {e}")))
}

/// Returns the default path for the depgraph snapshot file.
#[must_use]
pub fn depgraph_file_path() -> NormalizedPath {
    zccache_core::config::depgraph_dir().join("depgraph.bin")
}

/// Save the dependency graph to disk with atomic write.
///
/// GC is applied first ([`GC_TTL`]) and the snapshot is held to
/// [`SNAPSHOT_BUDGET_BYTES`] by LRU eviction. Never panics: any failure
/// returns `Err`, removes the tmp file, and leaves the previous snapshot
/// byte-identical.
pub fn save_to_file(graph: &DepGraph, path: &Path) -> Result<(), SnapshotError> {
    save_to_file_with(graph, path, &SaveOptions::default())
}

/// [`save_to_file`] with an injectable clock, TTL, budget and failure seam.
pub fn save_to_file_with(
    graph: &DepGraph,
    path: &Path,
    opts: &SaveOptions,
) -> Result<(), SnapshotError> {
    // GC: trim stale entries before saving.
    graph.trim(opts.ttl);

    let (snapshot, payload_len) = fit_to_budget(graph, opts)?;

    // Atomic write: write to .tmp, then rename.
    let tmp_path = path.with_extension("bin.tmp");
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let written = write_tmp(&tmp_path, &snapshot, payload_len, opts.fail_injection);
    if let Err(e) = written {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(e);
    }

    // Windows: remove target before rename (rename doesn't overwrite on Windows).
    let _ = std::fs::remove_file(path);
    if let Err(e) = std::fs::rename(&tmp_path, path) {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(e.into());
    }

    Ok(())
}

/// Build the snapshot, evicting least-recently-used contexts from `graph`
/// until header + payload fits in `opts.budget_bytes`.
fn fit_to_budget(
    graph: &DepGraph,
    opts: &SaveOptions,
) -> Result<(DepGraphSnapshot, u64), SnapshotError> {
    let header = HEADER_SIZE as u64;
    let mut snapshot = graph.to_snapshot_at(opts.now_unix_ms);
    let mut payload_len = encoded_len(&snapshot)?;

    for _ in 0..MAX_EVICTION_ROUNDS {
        let total = header.saturating_add(payload_len);
        if total <= opts.budget_bytes || snapshot.contexts.is_empty() {
            break;
        }
        let mut excess = total - opts.budget_bytes;

        // Oldest first.
        let mut order: Vec<usize> = (0..snapshot.contexts.len()).collect();
        order.sort_by_key(|&i| snapshot.contexts[i].last_accessed_unix_ms);

        let mut victims: Vec<ContextKey> = Vec::new();
        for i in order {
            let ctx = &snapshot.contexts[i];
            let size = codec()
                .serialized_size(ctx)
                .map_err(|e| SnapshotError::Corrupt(format!("serialize: {e}")))?;
            victims.push(ContextKey::from_raw(ctx.context_key));
            if size >= excess {
                break;
            }
            excess -= size;
        }
        graph.evict_contexts(&victims);

        snapshot = graph.to_snapshot_at(opts.now_unix_ms);
        payload_len = encoded_len(&snapshot)?;
    }

    Ok((snapshot, payload_len))
}

fn write_tmp(
    tmp_path: &Path,
    snapshot: &DepGraphSnapshot,
    payload_len: u64,
    fail_injection: bool,
) -> Result<(), SnapshotError> {
    let file = std::fs::File::create(tmp_path)?;
    let mut writer = BufWriter::new(file);

    // Header: magic + version (LE u32) + payload len (LE u64).
    writer.write_all(&DEPGRAPH_MAGIC)?;
    writer.write_all(&DEPGRAPH_VERSION.to_le_bytes())?;
    writer.write_all(&payload_len.to_le_bytes())?;

    if fail_injection || take_injected_failure(tmp_path) {
        return Err(SnapshotError::Io(std::io::Error::other(
            "injected depgraph save failure",
        )));
    }

    codec()
        .serialize_into(&mut writer, snapshot)
        .map_err(|e| SnapshotError::Corrupt(format!("serialize: {e}")))?;

    let file = writer
        .into_inner()
        .map_err(|e| SnapshotError::Io(e.into_error()))?;
    file.sync_all()?;
    Ok(())
}

/// Outcome of attempting to load the persisted depgraph from a cache directory.
///
/// Returned by [`classify_load`] so the daemon can both seed its in-memory
/// graph and surface the load result to operators (stderr + `last-session.log`).
/// The variants mirror the failure modes the daemon must handle distinctly:
///
/// - `Loaded` — file present, magic + version + payload all valid; the graph
///   is ready to serve hits from the very first lookup.
/// - `Missing` — no `depgraph.bin` in the cache dir. Genuine cold start.
/// - `VersionMismatch` — file present but the embedded version tag does not
///   match this build. The on-disk format changed since the prior session.
/// - `Corrupt` — magic mismatch, truncated, or payload validation failed.
/// - `IoError` — any other I/O failure reading the file.
#[derive(Debug)]
pub enum DepGraphLoadOutcome {
    Loaded {
        graph: DepGraph,
    },
    Missing,
    VersionMismatch {
        file_version: u32,
        expected_version: u32,
    },
    Corrupt {
        message: String,
    },
    IoError {
        message: String,
    },
}

impl DepGraphLoadOutcome {
    /// Returns the loaded graph if this outcome is `Loaded`, else `None`.
    #[must_use]
    pub fn into_graph(self) -> Option<DepGraph> {
        match self {
            Self::Loaded { graph } => Some(graph),
            _ => None,
        }
    }

    /// Returns a human-readable warning message for non-`Loaded`, non-`Missing`
    /// outcomes. Used by the daemon to emit a clear notice on stderr AND in the
    /// per-session log so operators can see exactly why the warm-load failed
    /// and the session fell back to cold behavior.
    #[must_use]
    pub fn warning(&self, path: &Path) -> Option<String> {
        match self {
            Self::Loaded { .. } | Self::Missing => None,
            Self::VersionMismatch {
                file_version,
                expected_version,
            } => Some(format!(
                "warning: persisted depgraph at {} has version {file_version}, expected {expected_version}; treating session as cold",
                path.display()
            )),
            Self::Corrupt { message } => Some(format!(
                "warning: persisted depgraph at {} is corrupt ({message}); treating session as cold",
                path.display()
            )),
            Self::IoError { message } => Some(format!(
                "warning: failed to read persisted depgraph at {} ({message}); treating session as cold",
                path.display()
            )),
        }
    }
}

/// Classify a load attempt at `path` into a structured outcome.
///
/// This is the load-and-classify helper called by the daemon at startup so a
/// fresh session pointed at a populated cache dir is automatically treated as
/// warm — no caller-side opt-in required. See issue #320.
///
/// On non-`Loaded` outcomes the returned value carries enough information to
/// generate a stderr/session-log warning via [`DepGraphLoadOutcome::warning`].
#[must_use]
pub fn classify_load(path: &Path) -> DepGraphLoadOutcome {
    match load_from_file(path) {
        Ok(graph) => DepGraphLoadOutcome::Loaded { graph },
        Err(SnapshotError::Io(ref e)) if e.kind() == std::io::ErrorKind::NotFound => {
            DepGraphLoadOutcome::Missing
        }
        Err(SnapshotError::Io(e)) => DepGraphLoadOutcome::IoError {
            message: e.to_string(),
        },
        Err(SnapshotError::VersionMismatch { file, expected }) => {
            DepGraphLoadOutcome::VersionMismatch {
                file_version: file,
                expected_version: expected,
            }
        }
        Err(SnapshotError::BadMagic) => DepGraphLoadOutcome::Corrupt {
            message: "bad magic bytes".into(),
        },
        Err(SnapshotError::Corrupt(message)) => DepGraphLoadOutcome::Corrupt { message },
    }
}

/// Load the dependency graph from disk, validating header and payload.
pub fn load_from_file(path: &Path) -> Result<DepGraph, SnapshotError> {
    load_from_file_with(path, &LoadOptions::default())
}

/// [`load_from_file`] with an injectable clock and TTL. Contexts whose
/// persisted wall-clock age exceeds `opts.ttl` are dropped; the rest keep
/// their persisted wall-clock `last_accessed_unix_ms` verbatim.
pub fn load_from_file_with(path: &Path, opts: &LoadOptions) -> Result<DepGraph, SnapshotError> {
    let file = std::fs::File::open(path)?;
    let file_len = file.metadata()?.len();
    let mut reader = BufReader::new(file);

    let mut header = [0u8; HEADER_SIZE];
    let mut filled = 0;
    while filled < HEADER_SIZE {
        let n = reader.read(&mut header[filled..])?;
        if n == 0 {
            return Err(SnapshotError::Corrupt("file too small for header".into()));
        }
        filled += n;
    }

    // Validate magic.
    if header[0..4] != DEPGRAPH_MAGIC {
        return Err(SnapshotError::BadMagic);
    }

    // Validate version.
    let version = u32::from_le_bytes([header[4], header[5], header[6], header[7]]);
    if version != DEPGRAPH_VERSION {
        return Err(SnapshotError::VersionMismatch {
            file: version,
            expected: DEPGRAPH_VERSION,
        });
    }

    let payload_len = u64::from_le_bytes([
        header[8], header[9], header[10], header[11], header[12], header[13], header[14],
        header[15],
    ]);
    let available = file_len.saturating_sub(HEADER_SIZE as u64);
    if available < payload_len {
        return Err(SnapshotError::Corrupt(format!(
            "truncated: expected {payload_len} payload bytes, got {available}",
        )));
    }

    // Stream-decode, bounded to the declared payload so a corrupt length
    // prefix inside the payload cannot drive an unbounded allocation.
    let mut snapshot: DepGraphSnapshot = codec()
        .with_limit(payload_len)
        .deserialize_from((&mut reader).take(payload_len))
        .map_err(|e| SnapshotError::Corrupt(format!("decode: {e}")))?;

    let ttl_ms = u64::try_from(opts.ttl.as_millis()).unwrap_or(u64::MAX);
    let before = snapshot.contexts.len();
    snapshot
        .contexts
        .retain(|c| opts.now_unix_ms.saturating_sub(c.last_accessed_unix_ms) <= ttl_ms);
    let expired_any = snapshot.contexts.len() != before;

    let graph = DepGraph::from_snapshot(snapshot);
    if expired_any {
        // Prune file entries and indexes only the expired contexts referenced.
        graph.evict_contexts(&[]);
    }
    Ok(graph)
}
