//! On-disk persistence for [`MetadataCache`].
//!
//! ## Why persist at all?
//!
//! The `MetadataCache` is a `DashMap<NormalizedPath, FileMetadata>` that
//! lives in the daemon process. Every fresh daemon (including the warm-side
//! daemon spawned by `soldr load` after restoring a cache directory) starts
//! with an empty map, so the very first lookup of every header pays the
//! full stat+blake3 cost even when the file's content is identical to what
//! the previous daemon already hashed.
//!
//! Persisting a versioned snapshot to `cache_dir/metadata.bin` on flush +
//! shutdown, and reading it back at startup, lets the
//! `(mtime, size)`-verified fast path in
//! [`MetadataCache::get_cached_hash_if_stat_valid`] fire on the very first
//! lookup of the new process. This is the warm-side win for the
//! `cold-tar-untar-warm × medium` perf-rust-cluster cell.
//!
//! ## Correctness — the safety net
//!
//! Loaded entries land at [`Confidence::Medium`], **never** `High`. The
//! existing fast path
//! ([`MetadataCache::get_cached_hash_if_stat_valid`]) still performs one
//! `stat()` syscall and compares `(mtime, size)` before trusting the
//! cached hash. So even if the on-disk snapshot is stale (file changed
//! between the previous daemon's save and the new daemon's load) the
//! restored hash is silently discarded and the slow path re-hashes —
//! exactly the same behaviour as a watcher-triggered downgrade.
//!
//! A wrong cache hit is catastrophic; an extra stat is cheap. The stat
//! is the catastrophe-prevention layer; persistence is a pure
//! performance optimisation.
//!
//! ## Forward/backward compatibility
//!
//! The on-disk format embeds a `version: u32` (currently
//! [`FORMAT_VERSION`] = `1`). On a version mismatch
//! [`MetadataCache::load_from_disk`] returns `Err`; callers (the daemon)
//! treat that as "start with an empty cache" and log a warning. Old
//! daemons reading a future snapshot will silently fall back to empty;
//! new daemons reading an old snapshot will do the same. This is purely
//! a local cache file — the IPC wire protocol is untouched, so
//! `PROTOCOL_VERSION` does NOT change with format bumps here.
//!
//! ## Atomicity
//!
//! [`MetadataCache::save_to_disk`] writes to a sibling `*.tmp` file then
//! `rename`s it over the destination. A crash mid-write leaves the prior
//! snapshot intact. An empty cache skips the write entirely (no need to
//! create a zero-entry file).

use super::metadata::{Confidence, FileMetadata, MetadataCache};
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::Path;
use std::time::{Instant, SystemTime};
use crate::core::NormalizedPath;

/// On-disk format version for the persisted [`MetadataCache`] snapshot.
///
/// Bump this whenever the layout of `PersistedMetadata` or
/// `PersistedEntry` changes in a way that older daemons cannot read.
/// Mismatches are handled by [`MetadataCache::load_from_disk`] returning
/// `Err`; the caller (daemon) treats that as "start empty".
pub const FORMAT_VERSION: u32 = 1;

/// Persisted form of one [`FileMetadata`] entry.
///
/// `confidence` is dropped (loaded entries always come back as
/// [`Confidence::Medium`]) and `last_verified` is dropped (loaded
/// entries get `Instant::now()`). `content_hash` is mandatory in the
/// persisted form — entries without a cached hash are useless to
/// restore and are skipped at save time.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct PersistedEntry {
    mtime: SystemTime,
    size: u64,
    content_hash: [u8; 32],
}

/// Top-level on-disk snapshot.
///
/// `version` is checked on load; mismatches are an error so the
/// daemon falls back to an empty cache without misinterpreting bytes.
#[derive(Debug, Serialize, Deserialize)]
struct PersistedMetadata {
    version: u32,
    entries: Vec<(NormalizedPath, PersistedEntry)>,
}

impl MetadataCache {
    /// Persist the cache to `path` as a versioned bincode snapshot.
    ///
    /// Only entries with a cached `content_hash` are written; entries
    /// without a hash are useless to restore and are silently skipped.
    /// An empty result set short-circuits to `Ok(())` without touching
    /// disk — no point creating a zero-entry file.
    ///
    /// Atomic on success: writes to `<path>.tmp-<pid>`, then `rename`s
    /// the temp file over `path`. On any I/O error mid-write the temp
    /// file is removed so the prior snapshot stays intact.
    ///
    /// Callers must NOT hold any [`dashmap`] shard locks while invoking
    /// this — the snapshot collection releases shards before serialising
    /// (each shard lock is held only during its `iter` step), but the
    /// expected caller is the daemon's flush/shutdown path which has no
    /// shards held anyway.
    ///
    /// # Errors
    ///
    /// Returns I/O errors from `create_dir_all`, `write`, or `rename`.
    /// Bincode serialisation errors are wrapped in `io::Error::other`.
    pub fn save_to_disk(&self, path: &Path) -> std::io::Result<()> {
        // Snapshot first so we don't hold DashMap shard locks across the
        // serialise + disk write. The `.clone()` is justified because we
        // need the entries to outlive the iter call for the bincode pass.
        let entries: Vec<(NormalizedPath, PersistedEntry)> = self
            .iter_for_persist()
            .into_iter()
            .filter_map(|(key, value)| {
                value.content_hash.map(|content_hash| {
                    (
                        key,
                        PersistedEntry {
                            mtime: value.mtime,
                            size: value.size,
                            content_hash,
                        },
                    )
                })
            })
            .collect();

        // Empty snapshot: skip the write, no file is better than an
        // empty file (avoids confusing future debugging "did this run?").
        if entries.is_empty() {
            tracing::debug!(
                path = %path.display(),
                "metadata cache flush: 0 persistable entries, skipping write"
            );
            return Ok(());
        }

        let entry_count = entries.len();
        let snapshot = PersistedMetadata {
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
            .unwrap_or_else(|| "metadata.bin".into());
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
                "metadata cache flushed to disk"
            );
        }
        result
    }

    /// Load a previously persisted snapshot from `path`.
    ///
    /// Returns:
    /// - `Ok(MetadataCache)` populated with entries from the snapshot
    ///   when the file exists and decodes cleanly at the current
    ///   [`FORMAT_VERSION`].
    /// - `Ok(MetadataCache::new())` when the file is absent (first run,
    ///   or the daemon was started without a previous save).
    /// - `Err` when the file exists but cannot be read, decoded, or has
    ///   a version mismatch. The daemon caller is expected to log a
    ///   warning and continue with an empty cache.
    ///
    /// Loaded entries land at [`Confidence::Medium`] with
    /// `last_verified = Instant::now()`. The existing fast path
    /// ([`MetadataCache::get_cached_hash_if_stat_valid`]) re-stats the
    /// file before trusting the cached hash, so stale on-disk metadata
    /// is harmless to correctness — a stat mismatch silently downgrades
    /// the response to a re-hash.
    ///
    /// # Errors
    ///
    /// Any I/O error other than `NotFound` from reading `path`, any
    /// bincode decode failure, or any version mismatch.
    pub fn load_from_disk(path: &Path) -> std::io::Result<Self> {
        let bytes = match std::fs::read(path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                tracing::info!(
                    path = %path.display(),
                    "metadata cache file not found, starting empty"
                );
                return Ok(Self::new());
            }
            Err(e) => return Err(e),
        };

        let snapshot: PersistedMetadata = bincode::deserialize(&bytes)
            .map_err(|e| std::io::Error::other(format!("bincode deserialize: {e}")))?;

        if snapshot.version != FORMAT_VERSION {
            return Err(std::io::Error::other(format!(
                "metadata snapshot version mismatch: file={} expected={}",
                snapshot.version, FORMAT_VERSION
            )));
        }

        let cache = Self::new();
        let now = Instant::now();
        let entry_count = snapshot.entries.len();
        for (key, entry) in snapshot.entries {
            cache.insert(
                key,
                FileMetadata {
                    mtime: entry.mtime,
                    size: entry.size,
                    confidence: Confidence::Medium,
                    last_verified: now,
                    content_hash: Some(entry.content_hash),
                },
            );
        }
        tracing::info!(
            path = %path.display(),
            loaded = entry_count,
            "metadata cache loaded from disk"
        );
        Ok(cache)
    }
}

/// Atomic, durable write — same contract as
/// `zccache_artifact::store::write_atomic_durable`. Duplicated here to
/// avoid a cross-crate dep from `zccache-fscache` on `zccache-artifact`;
/// the two crates are intentionally siblings in the dep graph.
///
/// Without the file's `sync_all`, the daemon can exit before the page
/// cache commits the rename's data block, leaving `soldr save` tarring
/// a 0-byte (or stale) snapshot. Parent-dir fsync is best-effort —
/// unsupported on Windows but the dominant durability concern there is
/// the metadata rename itself, which is already atomic.
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

// Internal helper kept private so it cannot accidentally become part of
// `MetadataCache`'s public API. Lets `save_to_disk` snapshot the entries
// without exposing DashMap internals.
trait PersistIter {
    fn iter_for_persist(&self) -> Vec<(NormalizedPath, FileMetadata)>;
}

impl PersistIter for MetadataCache {
    fn iter_for_persist(&self) -> Vec<(NormalizedPath, FileMetadata)> {
        // Snapshot under shard locks; release before serialisation +
        // disk I/O. `paths()` walks the same way for orphan cleanup.
        self.paths()
            .into_iter()
            .filter_map(|p| self.get(&p).map(|m| (p, m)))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::metadata::Confidence;
    use std::fs;
    use tempfile::TempDir;

    fn populated_cache() -> MetadataCache {
        let cache = MetadataCache::new();
        for i in 0..5 {
            cache.insert(
                NormalizedPath::from(format!("/tmp/persist{i}.c")),
                FileMetadata {
                    mtime: SystemTime::UNIX_EPOCH
                        + std::time::Duration::from_secs(1_000 + i as u64),
                    size: 100 + i as u64,
                    confidence: Confidence::High,
                    last_verified: Instant::now(),
                    content_hash: Some([i as u8; 32]),
                },
            );
        }
        cache
    }

    #[test]
    fn save_then_load_roundtrip_preserves_entries() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("metadata.bin");

        let cache = populated_cache();
        cache.save_to_disk(&path).unwrap();
        assert!(path.exists());

        let loaded = MetadataCache::load_from_disk(&path).unwrap();
        assert_eq!(loaded.len(), 5);

        for i in 0..5 {
            let key = NormalizedPath::from(format!("/tmp/persist{i}.c"));
            let entry = loaded.get(&key).unwrap();
            assert_eq!(entry.size, 100 + i as u64);
            assert_eq!(entry.content_hash, Some([i as u8; 32]));
            // Loaded entries land at Medium confidence, per safety-net contract.
            assert_eq!(entry.confidence, Confidence::Medium);
        }
    }

    #[test]
    fn load_missing_file_returns_empty_cache() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("does-not-exist.bin");

        let cache = MetadataCache::load_from_disk(&path).unwrap();
        assert!(cache.is_empty());
    }

    #[test]
    fn save_empty_cache_does_not_create_file() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("metadata.bin");

        let cache = MetadataCache::new();
        cache.save_to_disk(&path).unwrap();
        assert!(
            !path.exists(),
            "empty save must skip the write to avoid littering"
        );
    }

    #[test]
    fn entries_without_content_hash_are_skipped() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("metadata.bin");

        let cache = MetadataCache::new();
        cache.insert(
            NormalizedPath::from("/tmp/hashed.c"),
            FileMetadata {
                mtime: SystemTime::UNIX_EPOCH,
                size: 1,
                confidence: Confidence::High,
                last_verified: Instant::now(),
                content_hash: Some([7u8; 32]),
            },
        );
        cache.insert(
            NormalizedPath::from("/tmp/nohash.c"),
            FileMetadata {
                mtime: SystemTime::UNIX_EPOCH,
                size: 2,
                confidence: Confidence::High,
                last_verified: Instant::now(),
                content_hash: None,
            },
        );

        cache.save_to_disk(&path).unwrap();
        let loaded = MetadataCache::load_from_disk(&path).unwrap();
        assert_eq!(loaded.len(), 1);
        assert!(loaded.get(&NormalizedPath::from("/tmp/hashed.c")).is_some());
        assert!(loaded.get(&NormalizedPath::from("/tmp/nohash.c")).is_none());
    }

    #[test]
    fn load_corrupt_file_returns_err() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("metadata.bin");
        fs::write(&path, b"this is not bincode").unwrap();

        let result = MetadataCache::load_from_disk(&path);
        assert!(result.is_err());
    }

    #[test]
    fn load_wrong_version_returns_err() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("metadata.bin");

        // Hand-craft a snapshot with a bogus version.
        let bad = PersistedMetadata {
            version: FORMAT_VERSION.wrapping_add(999),
            entries: vec![],
        };
        let bytes = bincode::serialize(&bad).unwrap();
        fs::write(&path, &bytes).unwrap();

        let result = MetadataCache::load_from_disk(&path);
        assert!(result.is_err(), "wrong version must surface as an error");
    }

    #[test]
    fn atomic_save_does_not_leave_tmp_file_behind() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("metadata.bin");

        populated_cache().save_to_disk(&path).unwrap();

        // Scan the directory: only the final file should be present.
        let entries: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert!(entries.iter().any(|n| n == "metadata.bin"));
        assert!(
            !entries.iter().any(|n| n.contains(".tmp-")),
            "leftover tmp file: {entries:?}"
        );
    }
}
