//! Daemon-side fingerprint manager.
//!
//! Tracks per-watch dirty state in memory. FS watcher events flow through
//! `on_batch()` to set watches dirty; CLI queries via IPC get sub-millisecond
//! answers from the in-memory state.

use crate::core::NormalizedPath;
use std::collections::{HashMap, HashSet};
use std::path::Path;

use dashmap::DashMap;

use self::verify::{verify_filesystem, FileObservation};

mod verify;

#[cfg(test)]
mod tests;

/// Key identifying a unique fingerprint watch.
#[derive(Debug, Clone, Hash, Eq, PartialEq)]
pub(crate) struct WatchKey {
    /// Canonicalized root directory being watched.
    pub root: NormalizedPath,
    /// Canonical path to the cache file.
    pub cache_file: NormalizedPath,
}

/// Per-file tracked entry within a watch.
#[derive(Debug, Clone, Default)]
struct TrackedFile {
    mtime_ns: u64,
    size: u64,
    /// File identity, used by `verify_filesystem` to catch a path replacement
    /// that preserved mtime and size (#1897).
    file_id: Option<crate::platform::fs::identity::FileIdentity>,
    /// Unix `ctime` in nanoseconds, or `0` where the platform exposes none.
    inode_change_ns: u64,
    hash_hex: String,
}

impl TrackedFile {
    /// Record a full content-changing observation.
    fn observe(&mut self, observed: &FileObservation, hash_hex: String) {
        self.apply(observed);
        self.hash_hex = hash_hex;
    }

    /// Record a content-preserving ("smart touch") observation.
    fn refresh(&mut self, observed: &FileObservation) {
        self.apply(observed);
    }

    /// Adopt the cheap stat signals, which `verify_filesystem` re-reads
    /// before deciding the file is unchanged.
    fn apply(&mut self, observed: &FileObservation) {
        self.mtime_ns = observed.mtime_ns;
        self.size = observed.size;
        self.file_id = observed.file_id;
        self.inode_change_ns = observed.inode_change_ns;
    }
}

/// Pre-computed metadata for a single changed path in an `on_batch` call.
///
/// Built once per watcher batch *outside* the watch-map lock so the per-watch
/// update loop never holds a DashMap shard lock across filesystem I/O (issue #724).
struct ChangedMeta {
    /// Canonicalized absolute path of the changed file.
    canon: NormalizedPath,
    /// Cheap stat signals, gathered outside the watch-map lock (#724).
    observed: FileObservation,
    hash_hex: String,
}

/// State of a single fingerprint watch.
#[allow(dead_code)]
struct WatchState {
    /// Per-file state keyed by relative path (forward slashes).
    files: HashMap<String, TrackedFile>,
    /// Whether any file has changed since last mark-success.
    dirty: bool,
    /// Relative paths of files changed since last mark-success (Bug A fix).
    dirty_files: HashSet<String>,
    /// Monotonic counter bumped on each content-changing `on_batch` (Bug B fix).
    generation: u64,
    /// Generation at the time of the last `check` that returned "run".
    checked_generation: u64,
    /// "success", "pending", or "failure".
    status: String,
    /// Cache algorithm: "hash" or "two-layer".
    cache_type: String,
    /// Root directory (canonical).
    root: NormalizedPath,
}

/// Result of a fingerprint check.
pub(crate) struct FpCheckResult {
    /// "skip" or "run".
    pub decision: String,
    /// Reason string when decision is "run".
    pub reason: Option<String>,
    /// Changed file paths (relative).
    pub changed_files: Vec<String>,
}

/// Daemon-side fingerprint manager.
///
/// Holds in-memory state for all active fingerprint watches. The FS watcher
/// feeds events through `on_batch()`, and IPC queries get answers from
/// memory without touching the filesystem.
pub(crate) struct FingerprintManager {
    watches: DashMap<WatchKey, WatchState>,
}

/// Strip the `\\?\` extended-length prefix on Windows.
/// No-op on other platforms.
fn strip_win_prefix(path: NormalizedPath) -> NormalizedPath {
    NormalizedPath::new(crate::platform::fs::path::strip_verbatim_prefix(&path))
}

/// Canonicalize a path, stripping the `\\?\` prefix on Windows.
fn canon(path: &Path) -> NormalizedPath {
    match path.canonicalize() {
        Ok(c) => strip_win_prefix(c.into()),
        Err(_) => path.into(),
    }
}

/// Canonicalize a path that may not exist yet.
/// Tries full canonicalization first, then falls back to canonicalizing
/// the parent directory and joining the filename. Used for cache file
/// paths and for watcher event paths (removed files no longer exist).
fn canon_maybe_missing(path: &Path) -> NormalizedPath {
    if let Ok(c) = path.canonicalize() {
        return strip_win_prefix(c.into());
    }
    if let (Some(parent), Some(name)) = (path.parent(), path.file_name()) {
        if let Ok(cp) = parent.canonicalize() {
            return strip_win_prefix(cp.into()).join(name);
        }
    }
    path.into()
}

impl FingerprintManager {
    pub fn new() -> Self {
        Self {
            watches: DashMap::new(),
        }
    }

    /// Check whether files have changed for the given watch.
    ///
    /// If the watch exists and is clean (not dirty, status == success),
    /// returns Skip immediately (<1ms). Otherwise does the initial scan
    /// via the fingerprint library.
    pub fn check(
        &self,
        cache_file: &Path,
        cache_type: &str,
        root: &Path,
        extensions: &[String],
        include_globs: &[String],
        exclude: &[String],
    ) -> FpCheckResult {
        let canon_root = canon(root);
        let canon_cf = canon_maybe_missing(cache_file);
        let key = WatchKey {
            root: canon_root.clone(),
            cache_file: canon_cf,
        };

        // Fast path: existing watch — branch on dirty/status.
        if let Some(watch) = self.watches.get(&key) {
            let dirty = watch.dirty;
            let status = watch.status.clone();
            let changed_snapshot: Vec<String> = watch.dirty_files.iter().cloned().collect();
            let gen = watch.generation;
            drop(watch);

            if !dirty && status == "success" {
                // Verify against filesystem to catch missed watcher events.
                if let Some(mut w) = self.watches.get_mut(&key) {
                    let changed = verify_filesystem(&mut w);
                    if changed.is_empty() {
                        tracing::debug!("fingerprint check: skip (verified, not dirty)");
                        return FpCheckResult {
                            decision: "skip".into(),
                            reason: None,
                            changed_files: vec![],
                        };
                    }
                    // Watcher missed these changes — update state.
                    let new_gen = w.generation + 1;
                    w.generation = new_gen;
                    w.dirty = true;
                    for f in &changed {
                        w.dirty_files.insert(f.clone());
                    }
                    w.status = "pending".into();
                    w.checked_generation = new_gen;
                    drop(w);
                    tracing::debug!("fingerprint check: run (verified, content changed)");
                    return FpCheckResult {
                        decision: "run".into(),
                        reason: Some("content changed".into()),
                        changed_files: changed,
                    };
                }
                // Watch disappeared between get/get_mut — fall through to rescan.
            } else if dirty {
                // Bug A fix: collect the actual dirty file paths.
                // Mark as pending and snapshot the generation (Bug B fix).
                if let Some(mut w) = self.watches.get_mut(&key) {
                    w.status = "pending".into();
                    w.checked_generation = gen;
                }
                tracing::debug!("fingerprint check: run (dirty)");
                return FpCheckResult {
                    decision: "run".into(),
                    reason: Some("content changed".into()),
                    changed_files: changed_snapshot,
                };
            } else if status == "failure" {
                if let Some(mut w) = self.watches.get_mut(&key) {
                    w.status = "pending".into();
                    w.checked_generation = gen;
                }
                tracing::debug!("fingerprint check: run (previous failure)");
                return FpCheckResult {
                    decision: "run".into(),
                    reason: Some("previous failure".into()),
                    changed_files: vec![],
                };
            } else {
                // Bug C fix: status is "pending" (initial scan done, not yet marked).
                // Return "run" without doing a wasteful full rescan.
                tracing::debug!("fingerprint check: run (pending)");
                return FpCheckResult {
                    decision: "run".into(),
                    reason: Some("pending".into()),
                    changed_files: vec![],
                };
            }
        }

        // No existing watch — do initial scan.
        tracing::debug!(
            "fingerprint check: initial scan for {}",
            canon_root.display()
        );
        let files = Self::scan_files(&canon_root, extensions, include_globs, exclude);
        let files = match files {
            Ok(f) => f,
            Err(e) => {
                tracing::warn!("fingerprint scan failed: {e}");
                return FpCheckResult {
                    decision: "run".into(),
                    reason: Some(format!("scan error: {e}")),
                    changed_files: vec![],
                };
            }
        };

        // Hash all files and build tracked state.
        let mut tracked = HashMap::new();
        for file in &files {
            let observed = verify::observe(&file.absolute).unwrap_or_default();
            let hash_hex = match crate::hash::hash_file(&file.absolute) {
                Ok(h) => h.to_hex(),
                Err(_) => String::new(),
            };
            let mut entry = TrackedFile::default();
            entry.observe(&observed, hash_hex);
            tracked.insert(file.relative.clone(), entry);
        }

        let watch = WatchState {
            files: tracked,
            dirty: false,
            dirty_files: HashSet::new(),
            generation: 0,
            checked_generation: 0,
            status: "pending".into(),
            cache_type: cache_type.to_string(),
            root: canon_root,
        };
        self.watches.insert(key, watch);

        FpCheckResult {
            decision: "run".into(),
            reason: Some("no cache file".into()),
            changed_files: vec![],
        }
    }

    /// Mark the watch as successful.
    ///
    /// Bug B fix: only clears dirty if no new events arrived since the last
    /// check (generation == checked_generation). If on_batch bumped the
    /// generation between check and mark_success, dirty stays set.
    pub fn mark_success(&self, cache_file: &Path) {
        let canon_cf = canon_maybe_missing(cache_file);
        for mut entry in self.watches.iter_mut() {
            if entry.key().cache_file == canon_cf {
                let w = entry.value_mut();
                if w.generation == w.checked_generation {
                    w.dirty = false;
                    w.dirty_files.clear();
                }
                w.status = "success".into();
                tracing::debug!("fingerprint mark-success: {}", cache_file.display());
                return;
            }
        }
        tracing::debug!(
            "fingerprint mark-success: no watch for {}",
            cache_file.display()
        );
    }

    /// Mark the watch as failed.
    pub fn mark_failure(&self, cache_file: &Path) {
        let canon_cf = canon_maybe_missing(cache_file);
        for mut entry in self.watches.iter_mut() {
            if entry.key().cache_file == canon_cf {
                entry.value_mut().status = "failure".into();
                tracing::debug!("fingerprint mark-failure: {}", cache_file.display());
                return;
            }
        }
    }

    /// Invalidate (remove) a watch entirely.
    pub fn invalidate(&self, cache_file: &Path) {
        let canon_cf = canon_maybe_missing(cache_file);
        self.watches.retain(|key, _| key.cache_file != canon_cf);
        tracing::debug!("fingerprint invalidate: {}", cache_file.display());
    }

    /// Called by the watcher consumer when files change on disk.
    ///
    /// For each changed/removed path, checks all watches whose root contains
    /// the path, marks them dirty, and re-hashes only the affected file.
    pub fn on_batch(&self, changed: &[NormalizedPath], removed: &[NormalizedPath]) {
        if changed.is_empty() && removed.is_empty() {
            return;
        }

        // Pre-compute canonical path + stat + content hash for each changed path
        // ONCE, before touching the watch map. The previous implementation did this
        // canonicalize()/hash_file() I/O *inside* `watches.iter_mut()`, holding a
        // DashMap write-shard lock across blocking filesystem reads and repeating
        // the same hash once per watch. Large ESP32/LPC builds register hundreds of
        // watch roots, so a single watcher batch starved every RPC handler waiting
        // on those shards and wedged the daemon (issue #724). Hoisting the I/O out
        // of the lock keeps shard hold-times to in-memory map updates only, and
        // hashes each changed file exactly once instead of once per watch.
        let changed_meta: Vec<ChangedMeta> = changed
            .iter()
            .map(|path| {
                let canon_path = canon(path);
                // The stat observation must be built here, outside the watch-map
                // lock, so the loop below stays pure in-memory map work (#724).
                let observed = verify::observe(&canon_path).unwrap_or_default();
                let hash_hex = match crate::hash::hash_file(&canon_path) {
                    Ok(h) => h.to_hex(),
                    Err(_) => String::new(),
                };
                ChangedMeta {
                    canon: canon_path,
                    observed,
                    hash_hex,
                }
            })
            .collect();
        let removed_canon: Vec<NormalizedPath> = removed
            .iter()
            .map(|path| canon_maybe_missing(path))
            .collect();

        for mut entry in self.watches.iter_mut() {
            let watch = entry.value_mut();
            let root = &watch.root;

            for cm in &changed_meta {
                if let Ok(rel) = cm.canon.strip_prefix(root) {
                    let rel_str = rel.to_string_lossy().replace('\\', "/");

                    // Check if content actually changed (using the pre-hashed value).
                    let content_changed = match watch.files.get(&rel_str) {
                        Some(existing) => existing.hash_hex != cm.hash_hex,
                        None => true, // new file
                    };

                    if content_changed {
                        watch.dirty = true;
                        watch.dirty_files.insert(rel_str.clone());
                        watch.generation += 1;
                        let mut tracked = watch
                            .files
                            .remove(&rel_str)
                            .unwrap_or_else(TrackedFile::default);
                        tracked.observe(&cm.observed, cm.hash_hex.clone());
                        watch.files.insert(rel_str, tracked);
                    } else if let Some(tracked) = watch.files.get_mut(&rel_str) {
                        // Just update the stat signals, content unchanged (smart
                        // touch). Identity and `ctime` must be refreshed too, or a
                        // later `verify_filesystem` would compare against a stale
                        // identity and miss a path replacement (#1897).
                        tracked.refresh(&cm.observed);
                    }
                }
            }

            for canon_path in &removed_canon {
                if let Ok(rel) = canon_path.strip_prefix(root) {
                    let rel_str = rel.to_string_lossy().replace('\\', "/");
                    if watch.files.remove(&rel_str).is_some() {
                        watch.dirty = true;
                        watch.dirty_files.insert(rel_str);
                        watch.generation += 1;
                    }
                }
            }
        }
    }

    /// Scan files using the fingerprint library's walk functions.
    fn scan_files(
        root: &Path,
        extensions: &[String],
        include_globs: &[String],
        exclude: &[String],
    ) -> std::result::Result<
        Vec<crate::fingerprint::ScannedFile>,
        crate::fingerprint::FingerprintError,
    > {
        if !include_globs.is_empty() {
            let include_refs: Vec<&str> = include_globs.iter().map(|s| s.as_str()).collect();
            let exclude_refs: Vec<&str> = exclude.iter().map(|s| s.as_str()).collect();
            crate::fingerprint::walk_files_glob(root, &include_refs, &exclude_refs)
        } else {
            let ext_refs: Vec<&str> = extensions.iter().map(|s| s.as_str()).collect();
            let exclude_refs: Vec<&str> = exclude.iter().map(|s| s.as_str()).collect();
            crate::fingerprint::walk_files(root, &ext_refs, &exclude_refs)
        }
    }

    /// Number of active watches (for status/diagnostics).
    #[allow(dead_code)]
    pub fn watch_count(&self) -> usize {
        self.watches.len()
    }
}
