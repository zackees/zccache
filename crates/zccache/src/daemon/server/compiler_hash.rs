//! Compiler executable hash memoization.
//!
//! Caches `(mtime, size) -> ContentHash` for compiler binaries to skip
//! the per-request blake3 over multi-MB executables.
//!
//! ## On-disk persistence (issue #517)
//!
//! Hashing a 150 MB rustc binary on the cold path costs ~50-60 ms (Linux,
//! blake3 ~3 GB/s), dominating the `rust-workspace-link Cold` overhead
//! measured in `benchmark-stats/latest.json`. The cache is persisted to
//! disk alongside `metadata.bin` so a daemon restart (CI runner restart,
//! Stop hook tear-down, soldr-driven daemon recycle) does not refill it
//! from zero. The stored `(path, mtime, size, hash)` quad is exactly the
//! in-memory shape; correctness on load relies on the same stat-verify
//! that the in-memory `get_or_hash` already enforces — a (mtime, size)
//! mismatch silently downgrades the loaded entry to a re-hash, so a stale
//! snapshot cannot poison the cache key.

use super::*;
use serde::{Deserialize, Serialize};
use std::io::Write as _;

/// On-disk format version for the persisted compiler-hash cache.
///
/// Bump on any layout change to the `Persisted*` types so the loader
/// rejects older / newer snapshots instead of mis-decoding them.
pub(super) const FORMAT_VERSION: u32 = 1;

#[derive(Clone, Serialize, Deserialize)]
pub(super) struct CompilerHashEntry {
    pub(super) mtime: std::time::SystemTime,
    pub(super) size: u64,
    pub(super) hash: ContentHash,
}

#[derive(Serialize, Deserialize)]
struct PersistedCompilerHashes {
    version: u32,
    entries: Vec<(NormalizedPath, CompilerHashEntry)>,
}

#[derive(Default)]
pub(super) struct CompilerHashCache {
    pub(super) entries: DashMap<NormalizedPath, CompilerHashEntry>,
}

impl CompilerHashCache {
    pub(super) fn new() -> Self {
        Self {
            entries: DashMap::new(),
        }
    }

    #[cfg(test)]
    pub(super) fn len(&self) -> usize {
        self.entries.len()
    }

    pub(super) fn get_or_hash(&self, path: &Path) -> Option<ContentHash> {
        self.get_or_hash_with(path, |path| crate::hash::hash_file(path).ok())
    }

    pub(super) fn get_or_hash_with<F>(&self, path: &Path, hasher: F) -> Option<ContentHash>
    where
        F: FnOnce(&Path) -> Option<ContentHash>,
    {
        let metadata = std::fs::metadata(path).ok()?;
        let mtime = metadata.modified().ok()?;
        let size = metadata.len();
        let key = NormalizedPath::new(path);

        if let Some(entry) = self.entries.get(&key) {
            if entry.mtime == mtime && entry.size == size {
                return Some(entry.hash);
            }
        }

        let hash = hasher(path)?;
        let post_metadata = std::fs::metadata(path).ok()?;
        let post_mtime = post_metadata.modified().ok()?;
        let post_size = post_metadata.len();
        if post_mtime != mtime || post_size != size {
            return Some(hash);
        }

        self.entries
            .insert(key, CompilerHashEntry { mtime, size, hash });
        Some(hash)
    }

    /// Persist the cache to `path` as a versioned bincode snapshot.
    ///
    /// Atomic on success: writes to `<path>.tmp-<pid>`, then renames over
    /// `path`. Empty snapshots short-circuit without touching disk. Stale
    /// entries on disk are harmless: `get_or_hash` re-stats every key
    /// before trusting the hash, so a mismatch silently downgrades to a
    /// re-hash. See module-level doc.
    ///
    /// # Errors
    ///
    /// Returns I/O errors from `create_dir_all`, `write`, `rename`, or
    /// bincode serialization.
    pub(super) fn save_to_disk(&self, path: &Path) -> std::io::Result<()> {
        let entries: Vec<(NormalizedPath, CompilerHashEntry)> = self
            .entries
            .iter()
            .map(|e| (e.key().clone(), e.value().clone()))
            .collect();

        if entries.is_empty() {
            tracing::debug!(
                path = %path.display(),
                "compiler hash cache flush: 0 entries, skipping write"
            );
            return Ok(());
        }

        let entry_count = entries.len();
        let snapshot = PersistedCompilerHashes {
            version: FORMAT_VERSION,
            entries,
        };
        let bytes = bincode::serialize(&snapshot)
            .map_err(|e| std::io::Error::other(format!("bincode serialize: {e}")))?;

        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "compiler_hash.bin".into());
        let tmp = path.with_file_name(format!(".{name}.tmp-{}", std::process::id()));

        let result = write_atomic_durable(&tmp, path, &bytes);
        if result.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        if result.is_ok() {
            tracing::info!(
                path = %path.display(),
                entries = entry_count,
                bytes = bytes.len(),
                "compiler hash cache flushed to disk"
            );
        }
        result
    }

    /// Load a previously persisted snapshot from `path`.
    ///
    /// Returns an empty cache when the file is absent (first run). Any
    /// other I/O error, bincode decode failure, or version mismatch is
    /// surfaced as `Err`; the daemon caller is expected to log and start
    /// empty. Stat-verification at the `get_or_hash` call site re-checks
    /// every loaded entry before use, so a stale on-disk snapshot cannot
    /// produce an incorrect cache key.
    ///
    /// # Errors
    ///
    /// Any I/O error other than `NotFound`, any bincode decode failure,
    /// or any version mismatch.
    pub(super) fn load_from_disk(path: &Path) -> std::io::Result<Self> {
        let bytes = match std::fs::read(path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                tracing::debug!(
                    path = %path.display(),
                    "compiler hash cache file not found, starting empty"
                );
                return Ok(Self::new());
            }
            Err(e) => return Err(e),
        };

        let snapshot: PersistedCompilerHashes = bincode::deserialize(&bytes)
            .map_err(|e| std::io::Error::other(format!("bincode deserialize: {e}")))?;
        if snapshot.version != FORMAT_VERSION {
            return Err(std::io::Error::other(format!(
                "compiler hash cache version mismatch: file={}, expected={}",
                snapshot.version, FORMAT_VERSION
            )));
        }

        let entries = DashMap::with_capacity(snapshot.entries.len());
        let entry_count = snapshot.entries.len();
        for (key, value) in snapshot.entries {
            entries.insert(key, value);
        }
        tracing::info!(
            path = %path.display(),
            entries = entry_count,
            "compiler hash cache restored from disk"
        );
        Ok(Self { entries })
    }
}

fn write_atomic_durable(tmp: &Path, target: &Path, bytes: &[u8]) -> std::io::Result<()> {
    {
        let mut f = std::fs::File::create(tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    std::fs::rename(tmp, target)?;
    if let Some(parent) = target.parent() {
        if let Ok(dir) = std::fs::File::open(parent) {
            let _ = dir.sync_all();
        }
    }
    Ok(())
}

/// Env vars that name compiler executables. Issue #517 Option 2: warm the
/// hash cache for these on daemon startup so the very first compile
/// request — even on a fresh-checkout runner with no `compiler_hash.bin`
/// from a prior daemon — does NOT pay the ~50-60 ms cold blake3.
///
/// The list is intentionally minimal. We don't probe `$PATH` or scan for
/// rustup toolchains because:
/// - false positives (e.g. hashing `/usr/bin/cc` on a runner that only
///   uses `clang++`) cost wall-clock with no later benefit.
/// - the daemon may not have permission to stat arbitrary `PATH` entries.
///
/// If the user's invocations use a compiler whose path is not in any of
/// these env vars, the regular synchronous hash path in `get_or_hash`
/// still works — pre-hashing is purely an optimization, never a
/// correctness gate.
pub(super) const PREHASH_ENV_VARS: &[&str] = &["RUSTC", "CC", "CXX"];

/// Pre-hash the given paths into `cache` in the background. Failures
/// (path missing, stat error, hash error) are silently ignored — each
/// per-path failure just means the next real request for that compiler
/// pays the normal hash cost, the same as before this optimisation.
///
/// Caller is expected to wrap this in `tokio::task::spawn_blocking` so
/// the blake3 over multi-MB binaries does not block the async runtime.
pub(super) fn prehash_paths(cache: &CompilerHashCache, paths: &[std::path::PathBuf]) -> usize {
    let mut hashed = 0;
    for path in paths {
        if cache.get_or_hash(path).is_some() {
            hashed += 1;
        }
    }
    hashed
}

/// Read [`PREHASH_ENV_VARS`] from the process environment and return any
/// non-empty values as paths. Order is preserved so callers can rely on
/// `RUSTC` being warmed before `CC` etc. when iterated.
pub(super) fn prehash_candidates_from_env() -> Vec<std::path::PathBuf> {
    PREHASH_ENV_VARS
        .iter()
        .filter_map(std::env::var_os)
        .filter(|val| !val.is_empty())
        .map(std::path::PathBuf::from)
        .collect()
}
