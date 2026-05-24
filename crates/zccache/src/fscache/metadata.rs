//! File metadata types and cache implementation.

use crate::core::NormalizedPath;
use dashmap::DashMap;
use rayon::prelude::*;
use std::time::{Duration, Instant, SystemTime};

/// Confidence level for a cached metadata entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Confidence {
    /// Entry has not been verified recently. Must stat before trusting.
    Low,
    /// Watcher indicates no changes since last verification.
    Medium,
    /// Recently stat-verified or content-hash verified.
    High,
}

/// Cached metadata for a single file.
///
/// Change detection uses `(mtime, size)`. File replacement detection
/// (same path, new inode) is handled by the file watcher (`notify`),
/// not by platform-specific file identity checks.
#[derive(Debug, Clone)]
pub struct FileMetadata {
    /// File modification time.
    pub mtime: SystemTime,
    /// File size in bytes.
    pub size: u64,
    /// Confidence level of this entry.
    pub confidence: Confidence,
    /// When this entry was last verified via stat.
    pub last_verified: Instant,
    /// Cached content hash (blake3, 32 bytes), if computed.
    pub content_hash: Option<[u8; 32]>,
}

/// Concurrent file metadata cache.
///
/// Uses `DashMap` for sharded, concurrent access. Entries are keyed
/// by normalized path.
#[derive(Debug)]
pub struct MetadataCache {
    entries: DashMap<NormalizedPath, FileMetadata>,
    /// Duration after which a High-confidence entry decays to Medium.
    high_decay: Duration,
    /// Duration after which a Medium-confidence entry decays to Low.
    medium_decay: Duration,
}

impl MetadataCache {
    /// Create a new metadata cache with default decay durations.
    #[must_use]
    pub fn new() -> Self {
        Self {
            entries: DashMap::new(),
            high_decay: Duration::from_secs(60),
            medium_decay: Duration::from_secs(30),
        }
    }

    /// Look up metadata for a path, applying confidence decay.
    ///
    /// Returns `None` if the path is not in the cache.
    #[must_use]
    pub fn get(&self, path: &NormalizedPath) -> Option<FileMetadata> {
        self.entries.get(path).map(|entry| {
            let mut meta = entry.clone();
            meta.confidence = self.decayed_confidence(&meta);
            meta
        })
    }

    /// Insert or update metadata for a path.
    pub fn insert(&self, path: NormalizedPath, metadata: FileMetadata) {
        self.entries.insert(path, metadata);
    }

    /// Mark a path's entry as Low confidence (e.g., after watcher overflow).
    pub fn downgrade(&self, path: &NormalizedPath) {
        if let Some(mut entry) = self.entries.get_mut(path) {
            entry.confidence = Confidence::Low;
        }
    }

    /// Downgrade all entries to Low confidence.
    pub fn downgrade_all(&self) {
        for mut entry in self.entries.iter_mut() {
            entry.confidence = Confidence::Low;
        }
    }

    /// Re-stat all Low-confidence entries and promote those whose
    /// filesystem metadata `(mtime, size)` still matches.
    ///
    /// Designed for overflow recovery: after a watcher overflow downgrades
    /// everything to Low, this method cheaply restores High confidence for
    /// files that haven't actually changed — avoiding unnecessary re-hashing
    /// on subsequent lookups.
    ///
    /// Returns the number of entries promoted back to High confidence.
    pub fn rescan_all(&self) -> usize {
        // Collect Low-confidence keys so we can stat them in parallel
        // without holding DashMap shard locks during I/O.
        let low_keys: Vec<NormalizedPath> = self
            .entries
            .iter()
            .filter(|e| e.confidence == Confidence::Low)
            .map(|e| e.key().clone())
            .collect();

        if low_keys.is_empty() {
            return 0;
        }

        // Parallel stat: each file is independent.
        let results: Vec<(NormalizedPath, SystemTime, u64)> = low_keys
            .par_iter()
            .filter_map(|path| {
                Self::stat_file(path)
                    .ok()
                    .map(|fresh| (path.clone(), fresh.mtime, fresh.size))
            })
            .collect();

        // Apply promotions back (fast, in-memory only).
        let mut promoted = 0;
        for (path, mtime, size) in results {
            if let Some(mut entry) = self.entries.get_mut(&path) {
                if entry.confidence == Confidence::Low && entry.mtime == mtime && entry.size == size
                {
                    entry.confidence = Confidence::High;
                    entry.last_verified = Instant::now();
                    promoted += 1;
                }
            }
        }
        promoted
    }

    /// Remove a path from the cache.
    pub fn remove(&self, path: &NormalizedPath) {
        self.entries.remove(path);
    }

    /// Returns the number of entries in the cache.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Returns whether the cache is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Remove all entries from the cache.
    pub fn clear(&self) {
        self.entries.clear();
    }

    /// Return a cached content hash if the entry has High or Medium confidence.
    ///
    /// This is the clock-aware fast path: the caller has journal-based
    /// assurance that the file hasn't changed, so time-based decay is
    /// not applied. Returns `None` if the entry is missing, Low confidence,
    /// or has no cached hash.
    #[must_use]
    pub fn get_cached_hash(&self, path: &NormalizedPath) -> Option<crate::hash::ContentHash> {
        self.entries
            .get(path)
            .and_then(|entry| match entry.confidence {
                Confidence::High | Confidence::Medium => {
                    entry.content_hash.map(crate::hash::ContentHash::from_bytes)
                }
                Confidence::Low => None,
            })
    }

    /// Return a cached content hash only if `(mtime, size)` still match the
    /// filesystem — one `stat()` syscall, zero hashing.
    ///
    /// This is the safety net for the journal fast path: even when the journal
    /// reports no changes (watcher latency), a single stat catches files that
    /// were modified underneath us.
    ///
    /// Returns `None` if the entry is missing, Low confidence, has no cached
    /// hash, or the stat check fails / shows a mismatch.
    #[must_use]
    pub fn get_cached_hash_if_stat_valid(
        &self,
        path: &NormalizedPath,
    ) -> Option<crate::hash::ContentHash> {
        let entry = self.entries.get(path)?;
        match entry.confidence {
            Confidence::High | Confidence::Medium => {}
            Confidence::Low => return None,
        }
        let hash = entry
            .content_hash
            .map(crate::hash::ContentHash::from_bytes)?;

        // One stat syscall to verify mtime + size still match.
        let fs_meta = std::fs::metadata(path).ok()?;
        let mtime = fs_meta.modified().ok()?;
        let size = fs_meta.len();
        if entry.mtime == mtime && entry.size == size {
            Some(hash)
        } else {
            None
        }
    }

    /// Trim entries whose `last_verified` is older than `max_age`.
    /// Returns the number of entries removed.
    pub fn trim(&self, max_age: Duration) -> usize {
        let now = Instant::now();
        let mut removed = 0;
        self.entries.retain(|_, entry| {
            // Use saturating_duration_since to avoid panic if Instant is
            // non-monotonic (documented edge case on some platforms/VMs).
            if now.saturating_duration_since(entry.last_verified) > max_age {
                removed += 1;
                false
            } else {
                true
            }
        });
        removed
    }

    /// Evict the `count` oldest entries by `last_verified`.
    /// Returns the number actually removed (may be less than `count`).
    pub fn evict_oldest(&self, count: usize) -> usize {
        if count == 0 {
            return 0;
        }
        // Collect (path, last_verified) then sort oldest first.
        let mut entries: Vec<(NormalizedPath, Instant)> = self
            .entries
            .iter()
            .map(|e| (e.key().clone(), e.value().last_verified))
            .collect();
        entries.sort_by_key(|(_path, ts)| *ts);
        let to_remove = entries.len().min(count);
        for (path, _) in entries.into_iter().take(to_remove) {
            self.entries.remove(&path);
        }
        to_remove
    }

    /// Iterate all cached paths.
    pub fn paths(&self) -> Vec<NormalizedPath> {
        self.entries.iter().map(|e| e.key().clone()).collect()
    }

    fn decayed_confidence(&self, meta: &FileMetadata) -> Confidence {
        // Use saturating_duration_since to avoid panic if Instant is
        // non-monotonic (documented edge case on some platforms/VMs).
        let elapsed = Instant::now().saturating_duration_since(meta.last_verified);
        match meta.confidence {
            Confidence::High if elapsed > self.high_decay => Confidence::Medium,
            Confidence::Medium if elapsed > self.medium_decay => Confidence::Low,
            other => other,
        }
    }
}

impl Default for MetadataCache {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn insert_and_get() {
        let cache = MetadataCache::new();
        let path = NormalizedPath::from("/tmp/test.c");
        let meta = FileMetadata {
            mtime: SystemTime::now(),
            size: 100,
            confidence: Confidence::High,
            last_verified: Instant::now(),
            content_hash: None,
        };
        cache.insert(path.clone(), meta);
        assert!(cache.get(&path).is_some());
    }

    #[test]
    fn get_returns_none_for_missing_path() {
        let cache = MetadataCache::new();
        assert!(cache.get(&NormalizedPath::from("/no/such/path")).is_none());
    }

    #[test]
    fn insert_overwrites_existing() {
        let cache = MetadataCache::new();
        let path = NormalizedPath::from("/tmp/overwrite.c");

        let meta1 = FileMetadata {
            mtime: SystemTime::now(),
            size: 100,
            confidence: Confidence::High,
            last_verified: Instant::now(),
            content_hash: None,
        };
        cache.insert(path.clone(), meta1);
        assert_eq!(cache.get(&path).unwrap().size, 100);

        let meta2 = FileMetadata {
            mtime: SystemTime::now(),
            size: 999,
            confidence: Confidence::Medium,
            last_verified: Instant::now(),
            content_hash: None,
        };
        cache.insert(path.clone(), meta2);
        assert_eq!(cache.get(&path).unwrap().size, 999);
    }

    #[test]
    fn downgrade_single_path() {
        let cache = MetadataCache::new();
        let path_a = NormalizedPath::from("/tmp/a.c");
        let path_b = NormalizedPath::from("/tmp/b.c");

        for path in [&path_a, &path_b] {
            cache.insert(
                path.clone(),
                FileMetadata {
                    mtime: SystemTime::now(),
                    size: 10,
                    confidence: Confidence::High,
                    last_verified: Instant::now(),
                    content_hash: None,
                },
            );
        }

        cache.downgrade(&path_a);
        assert_eq!(cache.get(&path_a).unwrap().confidence, Confidence::Low);
        assert_eq!(cache.get(&path_b).unwrap().confidence, Confidence::High);
    }

    #[test]
    fn downgrade_nonexistent_is_noop() {
        let cache = MetadataCache::new();
        cache.downgrade(&NormalizedPath::from("/no/such/path")); // should not panic
    }

    #[test]
    fn remove_entry() {
        let cache = MetadataCache::new();
        let path = NormalizedPath::from("/tmp/removable.c");
        cache.insert(
            path.clone(),
            FileMetadata {
                mtime: SystemTime::now(),
                size: 10,
                confidence: Confidence::High,
                last_verified: Instant::now(),
                content_hash: None,
            },
        );
        assert_eq!(cache.len(), 1);

        cache.remove(&path);
        assert!(cache.get(&path).is_none());
        assert_eq!(cache.len(), 0);
    }

    #[test]
    fn remove_nonexistent_is_noop() {
        let cache = MetadataCache::new();
        cache.remove(&NormalizedPath::from("/no/such/path")); // should not panic
    }

    #[test]
    fn is_empty_and_len() {
        let cache = MetadataCache::new();
        assert!(cache.is_empty());
        assert_eq!(cache.len(), 0);

        cache.insert(
            NormalizedPath::from("/tmp/x.c"),
            FileMetadata {
                mtime: SystemTime::now(),
                size: 1,
                confidence: Confidence::High,
                last_verified: Instant::now(),
                content_hash: None,
            },
        );
        assert!(!cache.is_empty());
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn get_cached_hash_high_confidence() {
        let cache = MetadataCache::new();
        let path = NormalizedPath::from("/tmp/hashed.c");
        let hash_bytes = [42u8; 32];
        cache.insert(
            path.clone(),
            FileMetadata {
                mtime: SystemTime::now(),
                size: 10,
                confidence: Confidence::High,
                last_verified: Instant::now(),
                content_hash: Some(hash_bytes),
            },
        );

        let result = cache.get_cached_hash(&path);
        assert!(result.is_some());
        assert_eq!(*result.unwrap().as_bytes(), hash_bytes);
    }

    #[test]
    fn get_cached_hash_medium_confidence() {
        let cache = MetadataCache::new();
        let path = NormalizedPath::from("/tmp/med.c");
        let hash_bytes = [7u8; 32];
        cache.insert(
            path.clone(),
            FileMetadata {
                mtime: SystemTime::now(),
                size: 10,
                confidence: Confidence::Medium,
                last_verified: Instant::now(),
                content_hash: Some(hash_bytes),
            },
        );

        let result = cache.get_cached_hash(&path);
        assert!(result.is_some());
    }

    #[test]
    fn get_cached_hash_low_confidence_returns_none() {
        let cache = MetadataCache::new();
        let path = NormalizedPath::from("/tmp/low.c");
        cache.insert(
            path.clone(),
            FileMetadata {
                mtime: SystemTime::now(),
                size: 10,
                confidence: Confidence::Low,
                last_verified: Instant::now(),
                content_hash: Some([1u8; 32]),
            },
        );

        assert!(cache.get_cached_hash(&path).is_none());
    }

    #[test]
    fn get_cached_hash_no_hash_returns_none() {
        let cache = MetadataCache::new();
        let path = NormalizedPath::from("/tmp/nohash.c");
        cache.insert(
            path.clone(),
            FileMetadata {
                mtime: SystemTime::now(),
                size: 10,
                confidence: Confidence::High,
                last_verified: Instant::now(),
                content_hash: None,
            },
        );

        assert!(cache.get_cached_hash(&path).is_none());
    }

    #[test]
    fn get_cached_hash_missing_path_returns_none() {
        let cache = MetadataCache::new();
        assert!(cache
            .get_cached_hash(&NormalizedPath::from("/no/such"))
            .is_none());
    }

    #[test]
    fn clear_removes_all_entries() {
        let cache = MetadataCache::new();
        for i in 0..5 {
            cache.insert(
                NormalizedPath::from(format!("/tmp/clear{i}.c")),
                FileMetadata {
                    mtime: SystemTime::now(),
                    size: i as u64,
                    confidence: Confidence::High,
                    last_verified: Instant::now(),
                    content_hash: None,
                },
            );
        }
        assert_eq!(cache.len(), 5);
        cache.clear();
        assert!(cache.is_empty());
    }

    #[test]
    fn trim_removes_old_entries() {
        let cache = MetadataCache::new();
        let old = Instant::now() - Duration::from_secs(120);
        cache.insert(
            NormalizedPath::from("/tmp/old.c"),
            FileMetadata {
                mtime: SystemTime::now(),
                size: 10,
                confidence: Confidence::High,
                last_verified: old,
                content_hash: None,
            },
        );
        cache.insert(
            NormalizedPath::from("/tmp/new.c"),
            FileMetadata {
                mtime: SystemTime::now(),
                size: 10,
                confidence: Confidence::High,
                last_verified: Instant::now(),
                content_hash: None,
            },
        );
        let removed = cache.trim(Duration::from_secs(60));
        assert_eq!(removed, 1);
        assert_eq!(cache.len(), 1);
        assert!(cache.get(&NormalizedPath::from("/tmp/old.c")).is_none());
        assert!(cache.get(&NormalizedPath::from("/tmp/new.c")).is_some());
    }

    #[test]
    fn trim_keeps_recent_entries() {
        let cache = MetadataCache::new();
        for i in 0..5 {
            cache.insert(
                NormalizedPath::from(format!("/tmp/recent{i}.c")),
                FileMetadata {
                    mtime: SystemTime::now(),
                    size: 10,
                    confidence: Confidence::High,
                    last_verified: Instant::now(),
                    content_hash: None,
                },
            );
        }
        let removed = cache.trim(Duration::from_secs(60));
        assert_eq!(removed, 0);
        assert_eq!(cache.len(), 5);
    }

    #[test]
    fn evict_oldest_removes_n() {
        let cache = MetadataCache::new();
        let base = Instant::now() - Duration::from_secs(100);
        for i in 0..5 {
            cache.insert(
                NormalizedPath::from(format!("/tmp/e{i}.c")),
                FileMetadata {
                    mtime: SystemTime::now(),
                    size: 10,
                    confidence: Confidence::High,
                    last_verified: base + Duration::from_secs(i * 10),
                    content_hash: None,
                },
            );
        }
        let removed = cache.evict_oldest(2);
        assert_eq!(removed, 2);
        assert_eq!(cache.len(), 3);
    }

    #[test]
    fn evict_oldest_zero_noop() {
        let cache = MetadataCache::new();
        cache.insert(
            NormalizedPath::from("/tmp/z.c"),
            FileMetadata {
                mtime: SystemTime::now(),
                size: 10,
                confidence: Confidence::High,
                last_verified: Instant::now(),
                content_hash: None,
            },
        );
        let removed = cache.evict_oldest(0);
        assert_eq!(removed, 0);
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn evict_oldest_exceeds_count() {
        let cache = MetadataCache::new();
        cache.insert(
            NormalizedPath::from("/tmp/only.c"),
            FileMetadata {
                mtime: SystemTime::now(),
                size: 10,
                confidence: Confidence::High,
                last_verified: Instant::now(),
                content_hash: None,
            },
        );
        let removed = cache.evict_oldest(100);
        assert_eq!(removed, 1);
        assert!(cache.is_empty());
    }

    #[test]
    fn paths_returns_all() {
        let cache = MetadataCache::new();
        let expected: HashSet<NormalizedPath> = (0..3)
            .map(|i| NormalizedPath::from(format!("/tmp/p{i}.c")))
            .collect();
        for p in &expected {
            cache.insert(
                p.clone(),
                FileMetadata {
                    mtime: SystemTime::now(),
                    size: 10,
                    confidence: Confidence::High,
                    last_verified: Instant::now(),
                    content_hash: None,
                },
            );
        }
        let actual: HashSet<NormalizedPath> = cache.paths().into_iter().collect();
        assert_eq!(actual, expected);
    }

    #[test]
    fn confidence_ordering() {
        assert!(Confidence::Low < Confidence::Medium);
        assert!(Confidence::Medium < Confidence::High);
        assert!(Confidence::Low < Confidence::High);
    }

    #[test]
    fn default_creates_new_cache() {
        let cache = MetadataCache::default();
        assert!(cache.is_empty());
    }

    #[test]
    fn downgrade_all_works() {
        let cache = MetadataCache::new();
        for i in 0..10 {
            let path = NormalizedPath::from(format!("/tmp/test{i}.c"));
            let meta = FileMetadata {
                mtime: SystemTime::now(),
                size: 100,
                confidence: Confidence::High,
                last_verified: Instant::now(),
                content_hash: None,
            };
            cache.insert(path, meta);
        }
        cache.downgrade_all();
        // After downgrade, all entries should be Low confidence
        for entry in cache.entries.iter() {
            assert_eq!(entry.confidence, Confidence::Low);
        }
    }
}
