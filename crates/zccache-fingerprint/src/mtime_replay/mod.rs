//! Content-verified mtime snapshot/replay (zccache#1595).
//!
//! ## The problem
//!
//! A cache restore (or any checkout) stamps every source file it writes with
//! the moment of the checkout, not the file's original modification time.
//! Build systems that key freshness off mtime comparisons — Ninja, Cargo's
//! incremental fingerprints, Make — see every restored source as "newer than
//! its cached object" and rebuild the world, even when the bytes are
//! byte-for-byte identical to the build that produced the cached object.
//!
//! The naive fix — blanket-touching every restored file to some fixed instant
//! in the past, or blindly restoring whatever mtime a prior manifest
//! recorded — trades one bug for a worse one: a workspace where the content
//! genuinely changed (a real edit, a merge, a `git checkout` onto a different
//! branch) would silently receive a stale, pre-edit mtime. Ninja and Cargo
//! would then treat the edited source as unchanged and reuse an object built
//! from the *old* content — a correctness bug, not a performance one.
//!
//! ## The rule
//!
//! [`snapshot`] records, for each file, the mtime it had *at the moment its
//! content was hashed* — verified by re-stating the file before and after
//! hashing so a concurrent edit can never be attributed to the wrong bytes.
//! [`replay`] later restores that mtime to a file **only when the file's
//! current size and blake3 hash both still match** what was recorded.
//! Every other outcome — a missing file, a size mismatch, a hash mismatch,
//! or any I/O error along the way — leaves the file's mtime exactly as the
//! checkout left it (fresh), which biases the downstream build tool toward a
//! rebuild instead of a stale reuse. An absent or unrestored mtime can only
//! ever cost a wasted rebuild; a wrongly-restored one can produce a wrong
//! binary. See [`replay_one`] for the exact decision order.

use std::path::Path;

use kernal_api::platform::fs::{DirectoryWalk, DirectoryWalkParallelism, FileTime};
use rayon::prelude::*;
use zccache_core::NormalizedPath;

use super::error::{FingerprintError, Result};

/// On-disk schema version for [`MtimeManifest`]. Bumped whenever the JSON
/// shape changes in a way that is not forward-compatible.
pub const MANIFEST_VERSION: u32 = 1;

/// One recorded file: its content-verified size/mtime/hash at snapshot time.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct MtimeEntry {
    /// Workspace-relative POSIX path ('/' separators, no '.', '..', empty or '\\' components).
    pub path: String,
    pub size: u64,
    /// Modification time as signed nanoseconds since the Unix epoch.
    pub mtime_ns: i64,
    /// Lowercase BLAKE3 hex (zccache_hash::ContentHash::to_hex()).
    pub blake3: String,
}

/// The full snapshot written by [`snapshot`] and consumed by [`replay`].
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct MtimeManifest {
    pub version: u32,
    pub entries: Vec<MtimeEntry>,
}

/// Per-file result of [`replay_one`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReplayOutcome {
    /// Content verified; the recorded mtime was restored.
    Applied,
    /// The path does not exist, is not a regular file, or the manifest path
    /// itself is unsafe (escapes the workspace).
    Missing,
    /// The file exists but its size no longer matches the manifest — checked
    /// before hashing, so a resized file is never hashed.
    SizeMismatch,
    /// Size matched but the content hash did not, hashing failed, or setting
    /// the mtime itself failed. The file's current (fresh) mtime is left
    /// untouched in every one of these cases.
    Modified,
}

/// Aggregate counters over a [`replay`] run.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize)]
pub struct ReplayReport {
    pub total: u64,
    pub applied: u64,
    pub missing: u64,
    pub size_mismatch: u64,
    pub modified: u64,
}

impl ReplayReport {
    /// `applied / total`; `0.0` when `total == 0` (an empty restore never
    /// counts as "took").
    #[must_use]
    pub fn applied_ratio(&self) -> f64 {
        if self.total == 0 {
            0.0
        } else {
            self.applied as f64 / self.total as f64
        }
    }

    fn record(&mut self, outcome: ReplayOutcome) {
        self.total += 1;
        match outcome {
            ReplayOutcome::Applied => self.applied += 1,
            ReplayOutcome::Missing => self.missing += 1,
            ReplayOutcome::SizeMismatch => self.size_mismatch += 1,
            ReplayOutcome::Modified => self.modified += 1,
        }
    }
}

/// Walk `workspace` and record a content-verified mtime snapshot of every
/// regular file, excluding `.git` / `node_modules` (by name, any depth) and
/// anything under one of the resolved `excludes` (by resolved path — never
/// by name, so a source directory merely *named* `build` elsewhere in the
/// tree is kept; mirrors soldr's `walk_workspace_files` target-dir
/// resolution, soldr-cache#1547).
///
/// `excludes` entries that are relative are resolved against the
/// canonicalized `workspace`; entries that don't exist on disk are silently
/// dropped (nothing to exclude).
pub fn snapshot(workspace: &Path, excludes: &[&Path]) -> Result<MtimeManifest> {
    let root = workspace
        .canonicalize()
        .map_err(|e| FingerprintError::Scan {
            path: NormalizedPath::new(workspace),
            message: format!("cannot canonicalize root: {e}"),
        })?;

    let resolved_excludes: Vec<NormalizedPath> = excludes
        .iter()
        .copied()
        .filter_map(|exclude| {
            let candidate = if exclude.is_absolute() {
                exclude.to_path_buf()
            } else {
                root.join(exclude)
            };
            candidate.canonicalize().ok().map(NormalizedPath::new)
        })
        .collect();

    let candidates = walk_candidates(&root, resolved_excludes)?;

    let results: Result<Vec<Option<MtimeEntry>>> = candidates
        .par_iter()
        .map(|(absolute, relative)| snapshot_one(absolute, relative))
        .collect();
    let mut entries: Vec<MtimeEntry> = results?.into_iter().flatten().collect();
    entries.sort_by(|a, b| a.path.cmp(&b.path));

    Ok(MtimeManifest {
        version: MANIFEST_VERSION,
        entries,
    })
}

/// Walk `root`, returning `(absolute, relative-posix)` for every regular
/// file that survives the `.git` / `node_modules` / resolved-exclude prune.
fn walk_candidates(
    root: &Path,
    resolved_excludes: Vec<NormalizedPath>,
) -> Result<Vec<(NormalizedPath, String)>> {
    // The facade offers each candidate directory (never the root itself) to
    // the prune predicate before reading it, which is the old `depth > 0`
    // rule for `.git` / `node_modules`.
    let walker = DirectoryWalk::new(root.to_path_buf())
        .follow_symbolic_links(false)
        .include_hidden_entries(true)
        // soldr#2760: a walk on the shared pool can be aborted by its busy
        // timeout when the machine is loaded -- a hard failure purely because
        // the host was busy. A dedicated pool (threads: 0 = default size) has
        // no such timeout, at the cost of one throwaway pool per snapshot.
        .parallelism(DirectoryWalkParallelism::DedicatedPool { threads: 0 })
        .prune_directories(move |directory| {
            if directory
                .file_name()
                .is_some_and(|name| name == ".git" || name == "node_modules")
            {
                return false;
            }
            let candidate = NormalizedPath::new(directory);
            !resolved_excludes
                .iter()
                .any(|excluded| excluded == &candidate)
        });

    let mut candidates = Vec::new();
    for entry in walker.walk() {
        let entry = entry.map_err(|err| FingerprintError::Scan {
            path: NormalizedPath::new(root),
            message: format!("directory walk error: {err}"),
        })?;

        // Symlinks are skipped, not followed: an absent manifest entry only
        // ever means "no mtime replayed", i.e. a conservative rebuild.
        if !entry.is_file() {
            continue;
        }

        let abs = entry.path();
        let rel = abs.strip_prefix(root).map_err(|_| FingerprintError::Scan {
            path: NormalizedPath::new(abs),
            message: "path is not under root".to_string(),
        })?;
        let relative = rel_to_posix(rel);
        candidates.push((NormalizedPath::new(abs), relative));
    }
    Ok(candidates)
}

/// Convert a relative path to a POSIX string by joining its `Normal`
/// components with `/` (mirrors soldr's `rel_to_posix` / `scan::normalize_slashes`).
fn rel_to_posix(rel: &Path) -> String {
    rel.components()
        .filter_map(|c| match c {
            std::path::Component::Normal(part) => Some(part.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/")
}

/// Content-verified stat+hash+stat for one candidate file. Returns `Ok(None)`
/// when the file vanished, or when its size/mtime changed between the two
/// stats bracketing the hash — either way the file is silently dropped from
/// the manifest rather than risk recording a pre-edit mtime against
/// post-edit content (exactly the #1595 stale-object bug).
fn snapshot_one(absolute: &NormalizedPath, relative: &str) -> Result<Option<MtimeEntry>> {
    let path = absolute.as_path();
    let before = match std::fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(FingerprintError::Io(err)),
    };
    let hash = match zccache_hash::hash_file(path) {
        Ok(hash) => hash,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(FingerprintError::Io(err)),
    };
    let after = match std::fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(FingerprintError::Io(err)),
    };

    if before.len() != after.len() {
        return Ok(None);
    }
    let before_mtime = FileTime::from_last_modification_time(&before);
    let after_mtime = FileTime::from_last_modification_time(&after);
    if before_mtime != after_mtime {
        return Ok(None);
    }

    Ok(Some(MtimeEntry {
        path: relative.to_string(),
        size: after.len(),
        mtime_ns: filetime_to_ns(after_mtime),
        blake3: hash.to_hex(),
    }))
}

/// `FileTime` -> signed nanoseconds since the Unix epoch, clamped (not
/// truncated) to the `i64` range.
fn filetime_to_ns(ft: FileTime) -> i64 {
    let total: i128 = i128::from(ft.unix_seconds()) * 1_000_000_000 + i128::from(ft.nanoseconds());
    i64::try_from(total).unwrap_or(if total > 0 { i64::MAX } else { i64::MIN })
}

/// Write a manifest atomically (write `.tmp` + rename).
pub fn write_manifest(path: &Path, manifest: &MtimeManifest) -> Result<()> {
    super::persist::write_atomic(path, manifest)
}

/// Read a manifest, rejecting an unsupported [`MANIFEST_VERSION`].
///
/// A missing file surfaces as [`FingerprintError::Io`]; malformed JSON as
/// [`FingerprintError::Json`]; a version mismatch as
/// [`FingerprintError::Manifest`].
pub fn read_manifest(path: &Path) -> Result<MtimeManifest> {
    let bytes = std::fs::read(path)?;
    let manifest: MtimeManifest = serde_json::from_slice(&bytes)?;
    if manifest.version != MANIFEST_VERSION {
        return Err(FingerprintError::Manifest {
            path: NormalizedPath::new(path),
            message: format!(
                "unsupported manifest version {} (expected {MANIFEST_VERSION})",
                manifest.version
            ),
        });
    }
    Ok(manifest)
}

/// Replay one manifest entry's mtime onto `workspace`.
///
/// Decision order (must not be reordered — see `replay_one_with`):
///
/// 1. `entry.path` must be a safe workspace-relative path (non-empty,
///    every `/`-separated part is non-empty, not `.`/`..`, and free of `\`
///    and `:`) — otherwise [`ReplayOutcome::Missing`], and nothing outside
///    the workspace is ever touched.
/// 2. The path must resolve (via `symlink_metadata`, which does NOT follow
///    the final component) to a regular file — a directory, a symlink, or a
///    missing path is [`ReplayOutcome::Missing`].
/// 3. The on-disk size must match `entry.size` — checked strictly before
///    hashing — otherwise [`ReplayOutcome::SizeMismatch`].
/// 4. The blake3 hash must match `entry.blake3` (case-insensitively) —
///    otherwise [`ReplayOutcome::Modified`].
/// 5. Setting the mtime (the access time is left alone) must succeed —
///    otherwise [`ReplayOutcome::Modified`].
///
/// Every error path leaves the file's current mtime untouched and returns
/// something other than [`ReplayOutcome::Applied`].
pub fn replay_one(workspace: &Path, entry: &MtimeEntry) -> ReplayOutcome {
    replay_one_with(workspace, entry, kernal_api::platform::fs::set_file_mtime)
}

/// `replay_one` with the mtime-setting step injected, so tests can force a
/// failure on that final step without needing a read-only filesystem.
fn replay_one_with(
    workspace: &Path,
    entry: &MtimeEntry,
    set_time: impl Fn(&Path, FileTime) -> std::io::Result<()>,
) -> ReplayOutcome {
    if !is_safe_relative_path(&entry.path) {
        return ReplayOutcome::Missing;
    }
    let target = workspace.join(&entry.path);

    let meta = match std::fs::symlink_metadata(&target) {
        Ok(meta) => meta,
        Err(_) => return ReplayOutcome::Missing,
    };
    if !meta.is_file() {
        return ReplayOutcome::Missing;
    }
    if meta.len() != entry.size {
        return ReplayOutcome::SizeMismatch;
    }

    let hash = match zccache_hash::hash_file(&target) {
        Ok(hash) => hash,
        Err(_) => return ReplayOutcome::Modified,
    };
    if !hash.to_hex().eq_ignore_ascii_case(&entry.blake3) {
        return ReplayOutcome::Modified;
    }

    let time = FileTime::from_unix_time(
        entry.mtime_ns.div_euclid(1_000_000_000),
        entry.mtime_ns.rem_euclid(1_000_000_000) as u32,
    );
    if set_time(&target, time).is_err() {
        return ReplayOutcome::Modified;
    }
    ReplayOutcome::Applied
}

/// Whether `path` is safe to join onto a workspace root: non-empty, and
/// every `/`-separated part is non-empty, not `.`/`..`, and free of `\`/`:`.
fn is_safe_relative_path(path: &str) -> bool {
    if path.is_empty() {
        return false;
    }
    path.split('/').all(|part| {
        !part.is_empty()
            && part != "."
            && part != ".."
            && !part.contains('\\')
            && !part.contains(':')
    })
}

/// Replay every entry in `manifest` onto `workspace` in parallel, returning
/// aggregate counts.
pub fn replay(workspace: &Path, manifest: &MtimeManifest) -> ReplayReport {
    let outcomes: Vec<ReplayOutcome> = manifest
        .entries
        .par_iter()
        .map(|entry| replay_one(workspace, entry))
        .collect();
    let mut report = ReplayReport::default();
    for outcome in outcomes {
        report.record(outcome);
    }
    report
}

#[cfg(test)]
mod tests;
