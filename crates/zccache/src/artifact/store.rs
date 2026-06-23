//! In-memory artifact index, snapshotted to a bincode blob.
//!
//! Stores lightweight metadata (`ArtifactIndex`) for each cached artifact in
//! a `DashMap` for O(1) sharded concurrent access. The map is periodically
//! flushed to a single on-disk blob at `~/.zccache/index.bin` by the daemon's
//! background WAL writer (see `zccache_daemon::server::run_index_writer`).
//!
//! The output payloads for each entry live on disk as `{key}_0`, `{key}_1`,
//! ... (or `{key}.pack` under pack mode) and are loaded lazily on cache hit.
//!
//! ## Why not redb
//!
//! redb's MVCC + per-txn fsync is overkill for this use case: the daemon
//! already keeps a complete authoritative copy of the index in memory
//! (`SharedState::artifacts`), so the on-disk file is only consulted at
//! startup and only persists the runtime DashMap. A bincode blob is:
//!
//!   * faster to write (one sequential write per flush instead of one fsync
//!     per commit), and
//!   * trivial to read at startup (single `fs::read` + `bincode::deserialize`).
//!
//! The tradeoff is that a crash *between* flushes can lose the whole delta
//! (whereas redb could recover up to the last committed txn). The artifact
//! files themselves remain durable on disk, so the worst case is a re-miss
//! on the keys that hadn't been flushed — the daemon repopulates them on
//! next access. Graceful shutdown flushes synchronously.

use crate::core::NormalizedPath;
use std::path::Path;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use dashmap::DashMap;
use serde::{Deserialize, Serialize};

/// Lightweight metadata stored in the index for each cached artifact.
///
/// Contains everything needed to serve a cache hit response EXCEPT the output
/// file bytes (which are loaded lazily from `{key}_i` files on disk).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtifactIndex {
    /// Output filenames, e.g. `["foo.o"]`.
    /// Arc-wrapped so cache-hit clones are O(1) refcount bumps.
    pub output_names: Arc<[String]>,
    /// Output file sizes in bytes (parallel to `output_names`).
    pub output_sizes: Vec<u64>,
    /// Captured compiler stdout.
    /// Arc-wrapped so clones between CachedArtifact and persist tasks are O(1).
    pub stdout: Arc<Vec<u8>>,
    /// Captured compiler stderr.
    /// Arc-wrapped so clones between CachedArtifact and persist tasks are O(1).
    pub stderr: Arc<Vec<u8>>,
    /// Compiler exit code.
    pub exit_code: i32,
    /// Sum of all output file sizes (for eviction budget accounting).
    pub total_size: u64,
    /// Unix epoch seconds when this artifact was stored.
    pub stored_at_secs: u64,
}

impl ArtifactIndex {
    /// Create an `ArtifactIndex` from output names, sizes, and compiler results.
    pub fn new(
        output_names: Vec<String>,
        output_sizes: Vec<u64>,
        stdout: impl Into<Arc<Vec<u8>>>,
        stderr: impl Into<Arc<Vec<u8>>>,
        exit_code: i32,
    ) -> Self {
        let total_size = output_sizes.iter().sum();
        let stored_at_secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        Self {
            output_names: Arc::from(output_names),
            output_sizes,
            stdout: stdout.into(),
            stderr: stderr.into(),
            exit_code,
            total_size,
            stored_at_secs,
        }
    }
}

/// In-memory artifact index backed by a periodic bincode blob on disk.
///
/// All mutation methods (`insert`, `insert_many`, `remove`, `remove_batch`,
/// `clear`) are infallible — they only touch the in-memory DashMap. Disk
/// I/O happens exclusively in `flush()`, called by the daemon's background
/// WAL writer on its timer.
pub struct ArtifactStore {
    path: NormalizedPath,
    entries: DashMap<String, ArtifactIndex>,
}

impl ArtifactStore {
    /// Open the index at `path`. Reads the entire blob into memory; treats a
    /// missing or corrupt file as an empty index (the file will be created on
    /// the next `flush()`).
    ///
    /// Always logs the outcome (loaded N / file not found / corrupt) at INFO
    /// or WARN so a warm-after-restore daemon's log line tells you whether
    /// the on-disk index survived the snapshot/load round-trip. Silent
    /// "started empty" was the symptom that hid the cold-tar-untar-warm
    /// 0-hit-rate bug across two perf-cluster runs.
    pub fn open(path: &Path) -> std::io::Result<Self> {
        let store = Self::open_empty(path);
        store.load_from_disk()?;
        Ok(store)
    }

    /// Construct an empty store rooted at `path` without touching disk.
    ///
    /// Issue #784 phase 2d: lets `DaemonServer::bind_with_cache_dir`
    /// build the store in microseconds (no `std::fs::read` of the
    /// index blob), with the actual entry-loading deferred to a
    /// background `spawn_blocking` that calls [`Self::load_from_disk`]
    /// after the readiness lockfile is written. Until the load runs,
    /// `get` returns `None` for every key — `lookup_artifact_with_disk_fallback`
    /// (see `daemon::server::util`) triggers a synchronous
    /// `load_from_disk` on the first cache-miss in that window so the
    /// disk-fallback contract (perf test
    /// `perf_artifact_lookup_hits_before_background_load_completes`)
    /// is preserved.
    pub fn open_empty(path: &Path) -> Self {
        Self {
            path: NormalizedPath::new(path),
            entries: DashMap::new(),
        }
    }

    /// Read the on-disk index blob (if any) and insert every entry
    /// into the live `DashMap`. Idempotent: re-keys overwrite. Safe
    /// to call concurrently with request-handler inserts (DashMap
    /// `insert` is `&self`); safe to call multiple times (a redundant
    /// second call just re-inserts the same entries with the same
    /// values).
    ///
    /// Issue #784 phase 2d. Called once from `tokio::task::spawn_blocking`
    /// in the daemon binary after the readiness lockfile is written.
    /// Also called synchronously by
    /// `lookup_artifact_with_disk_fallback` on the first cache-miss
    /// during the load window — the synchronous path makes the
    /// existing on-disk-fallback test pass without paying the load
    /// cost at bind time.
    ///
    /// # Errors
    ///
    /// Returns any `std::fs::read` error other than `NotFound`. A
    /// missing file and a corrupt blob are both logged and treated
    /// as "empty" — same shape as the inline path in [`Self::open`].
    pub fn load_from_disk(&self) -> std::io::Result<()> {
        let path = self.path.as_path();
        match std::fs::read(path) {
            Ok(bytes) => match bincode::deserialize::<Vec<(String, ArtifactIndex)>>(&bytes) {
                Ok(rows) => {
                    let count = rows.len();
                    for (k, v) in rows {
                        self.entries.insert(k, v);
                    }
                    if count > 0 {
                        tracing::info!(
                            path = %path.display(),
                            loaded = count,
                            "artifact index loaded"
                        );
                    } else {
                        tracing::info!(
                            path = %path.display(),
                            "artifact index loaded as empty (file present, 0 entries)"
                        );
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        path = %path.display(),
                        "artifact index blob is corrupt, starting empty: {e}"
                    );
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                tracing::info!(
                    path = %path.display(),
                    "artifact index file not found, starting empty"
                );
            }
            Err(e) => return Err(e),
        };
        Ok(())
    }

    /// Insert or update an artifact entry. In-memory only.
    pub fn insert(&self, key: &str, meta: &ArtifactIndex) {
        self.entries.insert(key.to_string(), meta.clone());
    }

    /// Insert or update many entries. In-memory only.
    pub fn insert_many<I, K>(&self, entries: I) -> usize
    where
        I: IntoIterator<Item = (K, ArtifactIndex)>,
        K: AsRef<str>,
    {
        let mut count = 0usize;
        for (k, v) in entries {
            self.entries.insert(k.as_ref().to_string(), v);
            count += 1;
        }
        count
    }

    /// Look up a single artifact entry.
    pub fn get(&self, key: &str) -> Option<ArtifactIndex> {
        self.entries.get(key).map(|e| e.value().clone())
    }

    /// Remove a single entry. Returns `true` if it existed.
    pub fn remove(&self, key: &str) -> bool {
        self.entries.remove(key).is_some()
    }

    /// Remove a batch of entries. Returns the number actually removed.
    pub fn remove_batch(&self, keys: &[&str]) -> usize {
        let mut removed = 0usize;
        for key in keys {
            if self.entries.remove(*key).is_some() {
                removed += 1;
            }
        }
        removed
    }

    /// Snapshot every entry as `(key, ArtifactIndex)` pairs. O(n).
    pub fn load_all(&self) -> Vec<(String, ArtifactIndex)> {
        self.entries
            .iter()
            .map(|e| (e.key().clone(), e.value().clone()))
            .collect()
    }

    /// Number of entries currently held.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the index is empty.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Remove every entry. Returns the number removed.
    pub fn clear(&self) -> usize {
        let n = self.entries.len();
        self.entries.clear();
        n
    }

    /// Persist the current in-memory snapshot to disk atomically.
    ///
    /// Writes to a temp file in the same directory, then renames into place,
    /// so a partially-written file is never observable. Concurrent mutations
    /// during the snapshot are tolerated — each shard's lock is held only
    /// during its iter step, so the result is a "fuzzy snapshot" reflecting
    /// some serialisation of in-flight inserts (acceptable for the
    /// crash-recovery contract).
    pub fn flush(&self) -> std::io::Result<()> {
        let snapshot: Vec<(String, ArtifactIndex)> = self
            .entries
            .iter()
            .map(|e| (e.key().clone(), e.value().clone()))
            .collect();
        let bytes = bincode::serialize(&snapshot)
            .map_err(|e| std::io::Error::other(format!("bincode serialize: {e}")))?;
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let name = self
            .path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "index.bin".into());
        let tmp = self
            .path
            .as_path()
            .with_file_name(format!(".{name}.tmp-{}", std::process::id()));
        let target = self.path.as_path();
        let result = write_atomic_durable(&tmp, target, &bytes);
        if result.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        if result.is_ok() {
            tracing::info!(
                path = %self.path.display(),
                count = snapshot.len(),
                bytes = bytes.len(),
                "artifact index flushed to disk"
            );
        }
        result
    }

    /// Async wrapper for [`Self::flush`] that keeps the durable atomic write
    /// path off Tokio runtime threads.
    pub async fn flush_async(self: Arc<Self>) -> std::io::Result<()> {
        tokio::task::spawn_blocking(move || self.flush())
            .await
            .map_err(|e| std::io::Error::other(format!("artifact store flush task join: {e}")))?
    }

    /// Backwards-compatible alias for callers not yet renamed.
    pub async fn flush_blocking(self: Arc<Self>) -> std::io::Result<()> {
        self.flush_async().await
    }
}

/// Atomic, durable write: write to `tmp`, fsync the file, rename to
/// `target`, then best-effort fsync the parent directory so the rename
/// is durable.
///
/// Required by both `ArtifactStore::flush` (index.bin) and
/// `MetadataCache::save_to_disk` (metadata.bin) — without the file fsync,
/// the daemon can exit before the rename's data block is committed to
/// disk, leaving `soldr save` tarring a 0-byte (or stale) file. Without
/// the parent-dir fsync, the rename itself can be lost on a power-cut
/// before the directory entry is durable.
///
/// Parent-dir fsync is best-effort: opening a directory for fsync is
/// unsupported on Windows and on some test filesystems, but the rename's
/// metadata commit is the dominant durability concern there too. The
/// data fsync is the one that matters for the soldr save/load case
/// surfaced by perf-cluster runs 26255457227 + 26258412256.
fn write_atomic_durable(tmp: &Path, target: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
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

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_store() -> (tempfile::TempDir, ArtifactStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = ArtifactStore::open(&dir.path().join("index.bin")).unwrap();
        (dir, store)
    }

    fn sample_meta() -> ArtifactIndex {
        ArtifactIndex::new(
            vec!["foo.o".to_string()],
            vec![1234],
            b"compiler stdout".to_vec(),
            b"compiler stderr".to_vec(),
            0,
        )
    }

    #[test]
    fn open_creates_empty_index() {
        let (_dir, store) = temp_store();
        assert_eq!(store.len(), 0);
        assert!(store.is_empty());
    }

    #[test]
    fn insert_and_get() {
        let (_dir, store) = temp_store();
        store.insert("abc123", &sample_meta());
        let loaded = store.get("abc123").unwrap();
        assert_eq!(&*loaded.output_names, &["foo.o".to_string()]);
        assert_eq!(loaded.output_sizes, vec![1234]);
        assert_eq!(&*loaded.stdout, b"compiler stdout");
        assert_eq!(loaded.exit_code, 0);
        assert_eq!(loaded.total_size, 1234);
    }

    #[test]
    fn get_missing_returns_none() {
        let (_dir, store) = temp_store();
        assert!(store.get("nonexistent").is_none());
    }

    #[test]
    fn insert_overwrites() {
        let (_dir, store) = temp_store();
        let m1 = ArtifactIndex::new(vec!["a.o".into()], vec![100], vec![], vec![], 0);
        let m2 = ArtifactIndex::new(vec!["b.o".into()], vec![200], vec![], vec![], 1);
        store.insert("key", &m1);
        store.insert("key", &m2);
        assert_eq!(store.len(), 1);
        assert_eq!(store.get("key").unwrap().exit_code, 1);
    }

    #[test]
    fn remove_existing_and_missing() {
        let (_dir, store) = temp_store();
        store.insert("k", &sample_meta());
        assert!(store.remove("k"));
        assert!(!store.remove("k"));
        assert!(store.get("k").is_none());
    }

    #[test]
    fn remove_batch_multiple() {
        let (_dir, store) = temp_store();
        for i in 0..5 {
            store.insert(&format!("k{i}"), &sample_meta());
        }
        let removed = store.remove_batch(&["k0", "k2", "k4", "missing"]);
        assert_eq!(removed, 3);
        assert_eq!(store.len(), 2);
    }

    #[test]
    fn insert_many_and_load_all() {
        let (_dir, store) = temp_store();
        let entries: Vec<(String, ArtifactIndex)> = (0..50)
            .map(|i| (format!("batch-{i:03}"), sample_meta()))
            .collect();
        let n = store.insert_many(entries);
        assert_eq!(n, 50);
        assert_eq!(store.load_all().len(), 50);
    }

    #[test]
    fn insert_many_empty_is_noop() {
        let (_dir, store) = temp_store();
        let n = store.insert_many(std::iter::empty::<(String, ArtifactIndex)>());
        assert_eq!(n, 0);
        assert_eq!(store.len(), 0);
    }

    #[test]
    fn clear_removes_all() {
        let (_dir, store) = temp_store();
        for i in 0..10 {
            store.insert(&format!("k{i}"), &sample_meta());
        }
        let removed = store.clear();
        assert_eq!(removed, 10);
        assert!(store.is_empty());
    }

    #[test]
    fn flush_and_reopen_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.bin");
        {
            let store = ArtifactStore::open(&path).unwrap();
            store.insert("persist_test", &sample_meta());
            store.insert("another", &sample_meta());
            store.flush().unwrap();
        }
        let store = ArtifactStore::open(&path).unwrap();
        assert_eq!(store.len(), 2);
        assert!(store.get("persist_test").is_some());
        assert!(store.get("another").is_some());
    }

    #[test]
    fn open_corrupt_file_starts_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.bin");
        std::fs::write(&path, b"not valid bincode").unwrap();
        let store = ArtifactStore::open(&path).unwrap();
        assert_eq!(store.len(), 0);
    }

    #[test]
    fn flush_without_parent_dir_creates_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/deeply/index.bin");
        let store = ArtifactStore::open(&path).unwrap();
        store.insert("k", &sample_meta());
        store.flush().unwrap();
        assert!(path.exists());
    }

    #[test]
    fn flush_replaces_atomically() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.bin");
        let store = ArtifactStore::open(&path).unwrap();
        store.insert("a", &sample_meta());
        store.flush().unwrap();
        let first = std::fs::metadata(&path).unwrap().len();
        store.insert("b", &sample_meta());
        store.flush().unwrap();
        let second = std::fs::metadata(&path).unwrap().len();
        assert!(second > first);
    }
}
