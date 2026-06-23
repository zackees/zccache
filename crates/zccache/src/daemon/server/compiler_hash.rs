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
//! that the in-memory `get_or_hash_with` already enforces — a (mtime, size)
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

    /// Drain entries from a freshly loaded `CompilerHashCache` into `self`
    /// using `DashMap::insert` (which is `&self`).
    ///
    /// Issue #784: lets a background `spawn_blocking` task load the on-disk
    /// snapshot AFTER the daemon has written its readiness lockfile, then
    /// populate the live cache without holding up bind. Readers during the
    /// merge window either see no entry (cold-path miss — safe; the next
    /// call to `get_or_hash_with` re-hashes) or a loaded entry (stat-verify
    /// at the call site rejects stale (mtime, size) before trusting the
    /// hash, so a partially-loaded snapshot cannot poison cache keys).
    pub(super) fn merge_from(&self, other: Self) {
        for (k, v) in other.entries {
            self.entries.insert(k, v);
        }
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

    pub(super) async fn get_or_hash_with_async<F, Fut>(
        &self,
        path: &Path,
        hasher: F,
    ) -> Option<ContentHash>
    where
        F: FnOnce(std::path::PathBuf) -> Fut,
        Fut: std::future::Future<Output = Option<ContentHash>>,
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

        let hash = hasher(path.to_path_buf()).await?;
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
    /// entries on disk are harmless: `get_or_hash_with` re-stats every key
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
    /// empty. Stat-verification at the `get_or_hash_with` call site re-checks
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

/// Compute a content hash that uniquely identifies a rustc /
/// clippy-driver / rustfmt build, preferring `<compiler> -vV` output
/// over a full blake3 over the binary. `-vV` prints the toolchain
/// version + commit hash + LLVM version + host triple — all the bits
/// the cache key must vary on — and runs in ~10 ms vs ~50-60 ms for
/// the ~150 MB binary blake3 (issue #517).
///
/// Falls back to the file-content hash on spawn failure, non-zero
/// exit, or empty stdout so cache keys are still well-defined for
/// stubbed binaries (unit tests) or broken toolchains.
pub(super) fn hash_rustc_identity(path: &Path) -> Option<ContentHash> {
    match std::process::Command::new(path).arg("-vV").output() {
        Ok(output) if output.status.success() && !output.stdout.is_empty() => {
            Some(crate::hash::hash_bytes(&output.stdout))
        }
        // Spawn error, non-zero exit, or empty stdout - fall through to
        // the file-content hash so cache keys stay well-defined for
        // stubbed binaries (unit tests) and broken toolchains.
        _ => crate::hash::hash_file(path).ok(),
    }
}

pub(super) async fn hash_rustc_identity_async(path: std::path::PathBuf) -> Option<ContentHash> {
    match tokio::process::Command::new(&path)
        .arg("-vV")
        .output()
        .await
    {
        Ok(output) if output.status.success() && !output.stdout.is_empty() => {
            Some(crate::hash::hash_bytes(&output.stdout))
        }
        // Spawn error, non-zero exit, or empty stdout - fall through to
        // the file-content hash so cache keys stay well-defined for
        // stubbed binaries (unit tests) and broken toolchains.
        _ => crate::hash::hash_file(&path).ok(),
    }
}
