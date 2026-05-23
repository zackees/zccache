//! Filesystem verification for cached metadata.
//!
//! Bridges the in-memory `MetadataCache` with the real filesystem:
//! stat files, verify cached entries, and compute content hashes on demand.

use crate::metadata::{Confidence, FileMetadata, MetadataCache};
use std::path::Path;
use std::time::Instant;
use zccache_monocrate::core::NormalizedPath;
use zccache_monocrate::core::Result;
use zccache_monocrate::hash::ContentHash;

/// Result of verifying cached metadata against the current filesystem state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerifyResult {
    /// File metadata matches: `(mtime, size)` unchanged.
    Fresh,
    /// File metadata changed: `mtime` or `size` differs.
    Stale,
    /// File no longer exists at this path.
    Gone,
}

impl MetadataCache {
    /// Stat a file and return its current metadata at `High` confidence.
    ///
    /// This does NOT insert into the cache — it only reads from the filesystem.
    ///
    /// # Errors
    ///
    /// Returns an error if the file cannot be stat'd (not found, permission denied, etc.).
    pub fn stat_file(path: &Path) -> Result<FileMetadata> {
        let fs_meta = std::fs::metadata(path)?;
        let mtime = fs_meta.modified()?;
        let size = fs_meta.len();

        Ok(FileMetadata {
            mtime,
            size,
            confidence: Confidence::High,
            last_verified: Instant::now(),
            content_hash: None,
        })
    }

    /// Verify whether the cached entry for `path` still matches the filesystem.
    ///
    /// Compares `(mtime, size)` from the cache against a fresh stat.
    /// Does NOT update the cache — callers decide what to do with the result.
    ///
    /// Returns `Gone` if the file no longer exists.
    /// Returns `Err` if the path is not in the cache (use `stat_file` instead).
    pub fn verify(&self, path: &Path) -> Result<VerifyResult> {
        let normalized = NormalizedPath::from(path);
        let cached = self
            .get(&normalized)
            .ok_or_else(|| zccache_monocrate::core::Error::Cache {
                message: format!("path not in cache: {}", path.display()),
            })?;

        let fresh = match Self::stat_file(path) {
            Ok(m) => m,
            Err(zccache_monocrate::core::Error::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(VerifyResult::Gone);
            }
            Err(e) => return Err(e),
        };

        if cached.mtime != fresh.mtime || cached.size != fresh.size {
            return Ok(VerifyResult::Stale);
        }

        Ok(VerifyResult::Fresh)
    }

    /// Full cache lookup: check cache, stat-verify, hash if needed.
    ///
    /// Implements the lookup flow from the design doc:
    /// - Cache miss → stat, hash, insert at High, return hash.
    /// - High/Medium confidence + metadata match → return cached hash.
    /// - Low confidence + metadata match → re-hash anyway (low trust).
    /// - Metadata mismatch → re-hash.
    ///
    /// Handles the TOCTOU race: stats before and after hashing, retries if unstable.
    ///
    /// # Errors
    ///
    /// Returns an error if the file cannot be read.
    pub fn lookup(&self, path: &Path) -> Result<ContentHash> {
        let normalized = NormalizedPath::from(path);
        let cached = self.get(&normalized);

        // Cache miss → stat, hash, insert.
        let entry = match cached {
            None => return self.hash_and_insert(path),
            Some(e) => e,
        };

        // Always stat-verify before returning a hit.
        // "A wrong cache hit is catastrophic; an extra stat is cheap."
        let fresh = match Self::stat_file(path) {
            Ok(m) => m,
            Err(zccache_monocrate::core::Error::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => {
                self.remove(&normalized);
                return Err(zccache_monocrate::core::Error::FileNotFound(path.into()));
            }
            Err(e) => return Err(e),
        };

        let metadata_matches = entry.mtime == fresh.mtime && entry.size == fresh.size;

        if metadata_matches {
            match entry.confidence {
                Confidence::High | Confidence::Medium => {
                    // Metadata matches → promote to High, reuse cached hash if present.
                    if let Some(hash_bytes) = entry.content_hash {
                        self.insert(
                            path.into(),
                            FileMetadata {
                                confidence: Confidence::High,
                                last_verified: Instant::now(),
                                ..entry
                            },
                        );
                        return Ok(ContentHash::from_bytes(hash_bytes));
                    }
                    // No cached hash — hash now.
                }
                Confidence::Low => {
                    // Low trust: metadata matches but re-hash anyway (per design doc).
                }
            }
        }

        // Stale, Low, or no cached hash — re-hash.
        self.hash_and_insert(path)
    }

    /// Stat the file, hash it, insert at High confidence, return hash.
    fn hash_and_insert(&self, path: &Path) -> Result<ContentHash> {
        let pre_stat = Self::stat_file(path)?;
        let hash = zccache_monocrate::hash::hash_file(path)?;
        let post_stat = Self::stat_file(path)?;

        // TOCTOU check: if file changed during hashing, retry up to 3 times.
        if pre_stat.mtime != post_stat.mtime || pre_stat.size != post_stat.size {
            for _ in 0..3 {
                let pre = Self::stat_file(path)?;
                let h = zccache_monocrate::hash::hash_file(path)?;
                let post = Self::stat_file(path)?;
                if pre.mtime == post.mtime && pre.size == post.size {
                    self.insert(
                        path.into(),
                        FileMetadata {
                            content_hash: Some(*h.as_bytes()),
                            ..post
                        },
                    );
                    return Ok(h);
                }
            }
            // Still unstable after retries — return last hash but at Low confidence.
            let meta = Self::stat_file(path)?;
            self.insert(
                path.into(),
                FileMetadata {
                    confidence: Confidence::Low,
                    content_hash: Some(*hash.as_bytes()),
                    ..meta
                },
            );
            return Ok(hash);
        }

        self.insert(
            path.into(),
            FileMetadata {
                content_hash: Some(*hash.as_bytes()),
                ..post_stat
            },
        );
        Ok(hash)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Confidence;
    use std::fs;
    use std::thread;
    use std::time::Duration;
    use tempfile::TempDir;
    use zccache_monocrate::core::NormalizedPath;

    /// Helper: create a file with given content, return its path.
    fn create_file(dir: &TempDir, name: &str, content: &str) -> NormalizedPath {
        let path = dir.path().join(name);
        fs::write(&path, content).expect("failed to create test file");
        path.into()
    }

    /// Helper: sleep enough for mtime to differ.
    /// Filesystem timestamps have varying granularity (FAT32: 2s, NTFS: 100ns,
    /// ext4: 1ns, HFS+: 1s). We sleep 1.1s to be safe across all platforms.
    fn sleep_for_mtime() {
        thread::sleep(Duration::from_millis(1100));
    }

    // ---------------------------------------------------------------
    // stat_file
    // ---------------------------------------------------------------

    #[test]
    fn stat_file_returns_metadata_for_existing_file() {
        let dir = TempDir::new().unwrap();
        let path = create_file(&dir, "hello.c", "int main() { return 0; }");

        let meta = MetadataCache::stat_file(&path).unwrap();

        assert_eq!(meta.size, 24);
        assert_eq!(meta.confidence, Confidence::High);
        assert!(meta.content_hash.is_none()); // stat doesn't hash
    }

    #[test]
    fn stat_file_fails_for_missing_file() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("nonexistent.c");

        let result = MetadataCache::stat_file(&path);
        assert!(result.is_err());
    }

    #[test]
    fn stat_file_captures_correct_size() {
        let dir = TempDir::new().unwrap();
        let content = "a".repeat(4096);
        let path = create_file(&dir, "big.c", &content);

        let meta = MetadataCache::stat_file(&path).unwrap();
        assert_eq!(meta.size, 4096);
    }

    // ---------------------------------------------------------------
    // verify: unchanged file
    // ---------------------------------------------------------------

    #[test]
    fn verify_unchanged_file_returns_fresh() {
        let dir = TempDir::new().unwrap();
        let path = create_file(&dir, "stable.c", "#include <stdio.h>");

        let cache = MetadataCache::new();
        let meta = MetadataCache::stat_file(&path).unwrap();
        cache.insert(path.clone(), meta);

        let result = cache.verify(&path).unwrap();
        assert_eq!(result, VerifyResult::Fresh);
    }

    // ---------------------------------------------------------------
    // verify: touch (mtime change, same content)
    // ---------------------------------------------------------------

    #[test]
    fn verify_detects_touch() {
        let dir = TempDir::new().unwrap();
        let path = create_file(&dir, "touched.c", "void f() {}");

        let cache = MetadataCache::new();
        let meta = MetadataCache::stat_file(&path).unwrap();
        cache.insert(path.clone(), meta);

        // Touch: rewrite same content after a delay so mtime changes
        sleep_for_mtime();
        fs::write(&path, "void f() {}").unwrap();

        let result = cache.verify(&path).unwrap();
        assert_eq!(result, VerifyResult::Stale);
    }

    // ---------------------------------------------------------------
    // verify: edit (content + size change)
    // ---------------------------------------------------------------

    #[test]
    fn verify_detects_content_edit() {
        let dir = TempDir::new().unwrap();
        let path = create_file(&dir, "edited.c", "int x = 1;");

        let cache = MetadataCache::new();
        let meta = MetadataCache::stat_file(&path).unwrap();
        cache.insert(path.clone(), meta);

        sleep_for_mtime();
        fs::write(&path, "int x = 42; // changed").unwrap();

        let result = cache.verify(&path).unwrap();
        assert_eq!(result, VerifyResult::Stale);
    }

    #[test]
    fn verify_detects_size_change_same_mtime_granularity() {
        let dir = TempDir::new().unwrap();
        let path = create_file(&dir, "sized.c", "short");

        let cache = MetadataCache::new();
        let meta = MetadataCache::stat_file(&path).unwrap();
        cache.insert(path.clone(), meta);

        // Write different-length content immediately (mtime might not change on coarse FS)
        fs::write(&path, "much longer content than before").unwrap();

        let result = cache.verify(&path).unwrap();
        assert_eq!(result, VerifyResult::Stale);
    }

    // ---------------------------------------------------------------
    // verify: file removal
    // ---------------------------------------------------------------

    #[test]
    fn verify_detects_removed_file() {
        let dir = TempDir::new().unwrap();
        let path = create_file(&dir, "doomed.c", "goodbye");

        let cache = MetadataCache::new();
        let meta = MetadataCache::stat_file(&path).unwrap();
        cache.insert(path.clone(), meta);

        fs::remove_file(&path).unwrap();

        let result = cache.verify(&path).unwrap();
        assert_eq!(result, VerifyResult::Gone);
    }

    // ---------------------------------------------------------------
    // verify: file replacement (delete + recreate at same path)
    // ---------------------------------------------------------------

    #[test]
    fn verify_detects_file_replacement() {
        let dir = TempDir::new().unwrap();
        let path = create_file(&dir, "replaced.c", "original");

        let cache = MetadataCache::new();
        let meta = MetadataCache::stat_file(&path).unwrap();
        cache.insert(path.clone(), meta);

        // Replace: delete and recreate — mtime will differ
        fs::remove_file(&path).unwrap();
        sleep_for_mtime();
        fs::write(&path, "original").unwrap(); // same content!

        let result = cache.verify(&path).unwrap();
        assert_eq!(result, VerifyResult::Stale);
    }

    // ---------------------------------------------------------------
    // verify: uncached path returns error
    // ---------------------------------------------------------------

    #[test]
    fn verify_uncached_path_returns_error() {
        let cache = MetadataCache::new();
        let result = cache.verify(Path::new("/no/such/entry"));
        assert!(result.is_err());
    }

    // ---------------------------------------------------------------
    // lookup: cache miss → stat + hash + insert
    // ---------------------------------------------------------------

    #[test]
    fn lookup_miss_stats_hashes_and_caches() {
        let dir = TempDir::new().unwrap();
        let path = create_file(&dir, "new.c", "int main() {}");

        let cache = MetadataCache::new();
        assert!(cache.is_empty());

        let hash = cache.lookup(&path).unwrap();

        assert_eq!(cache.len(), 1);
        let entry = cache.get(&path).unwrap();
        assert_eq!(entry.confidence, Confidence::High);
        assert!(entry.content_hash.is_some());

        let expected = zccache_monocrate::hash::hash_file(&path).unwrap();
        assert_eq!(hash, expected);
    }

    // ---------------------------------------------------------------
    // lookup: cache hit, file unchanged → returns cached hash
    // ---------------------------------------------------------------

    #[test]
    fn lookup_hit_unchanged_returns_cached_hash() {
        let dir = TempDir::new().unwrap();
        let path = create_file(&dir, "stable.c", "const int N = 42;");

        let cache = MetadataCache::new();
        let hash1 = cache.lookup(&path).unwrap();
        let hash2 = cache.lookup(&path).unwrap();

        assert_eq!(hash1, hash2);
    }

    // ---------------------------------------------------------------
    // lookup: cache hit, file edited → re-hashes
    // ---------------------------------------------------------------

    #[test]
    fn lookup_rehashes_after_edit() {
        let dir = TempDir::new().unwrap();
        let path = create_file(&dir, "evolving.c", "v1");

        let cache = MetadataCache::new();
        let hash_v1 = cache.lookup(&path).unwrap();

        sleep_for_mtime();
        fs::write(&path, "v2 with more content").unwrap();

        let hash_v2 = cache.lookup(&path).unwrap();
        assert_ne!(hash_v1, hash_v2);

        let expected = zccache_monocrate::hash::hash_bytes(b"v2 with more content");
        assert_eq!(hash_v2, expected);
    }

    // ---------------------------------------------------------------
    // lookup: file removed between lookups → error
    // ---------------------------------------------------------------

    #[test]
    fn lookup_fails_after_removal() {
        let dir = TempDir::new().unwrap();
        let path = create_file(&dir, "ephemeral.c", "here today");

        let cache = MetadataCache::new();
        let _hash = cache.lookup(&path).unwrap();

        fs::remove_file(&path).unwrap();

        let result = cache.lookup(&path);
        assert!(result.is_err());
    }

    // ---------------------------------------------------------------
    // lookup: downgraded entry forces re-verification
    // ---------------------------------------------------------------

    #[test]
    fn lookup_after_downgrade_reverifies() {
        let dir = TempDir::new().unwrap();
        let path = create_file(&dir, "watched.c", "stable content");

        let cache = MetadataCache::new();
        let hash1 = cache.lookup(&path).unwrap();

        cache.downgrade_all();

        let hash2 = cache.lookup(&path).unwrap();
        assert_eq!(hash1, hash2);

        let entry = cache.get(&path).unwrap();
        assert_eq!(entry.confidence, Confidence::High);
    }

    // ---------------------------------------------------------------
    // lookup: downgraded entry + file changed → detects change
    // ---------------------------------------------------------------

    #[test]
    fn lookup_after_downgrade_detects_change() {
        let dir = TempDir::new().unwrap();
        let path = create_file(&dir, "sneaky.c", "before");

        let cache = MetadataCache::new();
        let hash_before = cache.lookup(&path).unwrap();

        cache.downgrade(&path);

        sleep_for_mtime();
        fs::write(&path, "after").unwrap();

        let hash_after = cache.lookup(&path).unwrap();
        assert_ne!(hash_before, hash_after);
    }

    // ---------------------------------------------------------------
    // multiple files: independence
    // ---------------------------------------------------------------

    #[test]
    fn changes_to_one_file_dont_affect_others() {
        let dir = TempDir::new().unwrap();
        let path_a = create_file(&dir, "a.c", "file a");
        let path_b = create_file(&dir, "b.c", "file b");

        let cache = MetadataCache::new();
        let hash_a = cache.lookup(&path_a).unwrap();
        let hash_b = cache.lookup(&path_b).unwrap();

        sleep_for_mtime();
        fs::write(&path_b, "file b modified").unwrap();

        let hash_a2 = cache.lookup(&path_a).unwrap();
        assert_eq!(hash_a, hash_a2);

        let hash_b2 = cache.lookup(&path_b).unwrap();
        assert_ne!(hash_b, hash_b2);
    }

    // ---------------------------------------------------------------
    // content hash is cleared on re-stat after change
    // ---------------------------------------------------------------

    #[test]
    fn content_hash_invalidated_on_change() {
        let dir = TempDir::new().unwrap();
        let path = create_file(&dir, "hashme.c", "original");

        let cache = MetadataCache::new();
        let _hash = cache.lookup(&path).unwrap();

        let entry = cache.get(&path).unwrap();
        assert!(entry.content_hash.is_some());

        sleep_for_mtime();
        fs::write(&path, "changed content").unwrap();

        let new_hash = cache.lookup(&path).unwrap();
        let expected = zccache_monocrate::hash::hash_bytes(b"changed content");
        assert_eq!(new_hash, expected);
    }

    // ---------------------------------------------------------------
    // sequential edits: cache tracks the full lifecycle
    // ---------------------------------------------------------------

    #[test]
    fn full_lifecycle_create_edit_edit_remove() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("lifecycle.c");

        let cache = MetadataCache::new();

        // 1. File doesn't exist yet
        assert!(cache.lookup(&path).is_err());

        // 2. Create the file
        fs::write(&path, "version 1").unwrap();
        let hash_v1 = cache.lookup(&path).unwrap();
        assert_eq!(hash_v1, zccache_monocrate::hash::hash_bytes(b"version 1"));

        // 3. First edit
        sleep_for_mtime();
        fs::write(&path, "version 2").unwrap();
        let hash_v2 = cache.lookup(&path).unwrap();
        assert_ne!(hash_v1, hash_v2);
        assert_eq!(hash_v2, zccache_monocrate::hash::hash_bytes(b"version 2"));

        // 4. Second edit
        sleep_for_mtime();
        fs::write(&path, "version 3 with extra stuff").unwrap();
        let hash_v3 = cache.lookup(&path).unwrap();
        assert_ne!(hash_v2, hash_v3);
        assert_eq!(
            hash_v3,
            zccache_monocrate::hash::hash_bytes(b"version 3 with extra stuff")
        );

        // 5. Remove
        fs::remove_file(&path).unwrap();
        assert!(cache.lookup(&path).is_err());

        // 6. Cache entry should be gone (or at least lookup returns error)
        assert!(cache.lookup(&path).is_err());
    }

    // ---------------------------------------------------------------
    // append-only edit (size grows, mtime changes)
    // ---------------------------------------------------------------

    #[test]
    fn verify_detects_append() {
        let dir = TempDir::new().unwrap();
        let path = create_file(&dir, "growing.c", "line 1\n");

        let cache = MetadataCache::new();
        let meta = MetadataCache::stat_file(&path).unwrap();
        cache.insert(path.clone(), meta);

        sleep_for_mtime();
        use std::io::Write;
        let mut f = fs::OpenOptions::new().append(true).open(&path).unwrap();
        f.write_all(b"line 2\n").unwrap();
        drop(f);

        let result = cache.verify(&path).unwrap();
        assert_eq!(result, VerifyResult::Stale);
    }

    // ---------------------------------------------------------------
    // truncate to empty
    // ---------------------------------------------------------------

    #[test]
    fn verify_detects_truncation() {
        let dir = TempDir::new().unwrap();
        let path = create_file(&dir, "truncated.c", "lots of content here");

        let cache = MetadataCache::new();
        let meta = MetadataCache::stat_file(&path).unwrap();
        cache.insert(path.clone(), meta);

        sleep_for_mtime();
        fs::write(&path, "").unwrap();

        let result = cache.verify(&path).unwrap();
        assert_eq!(result, VerifyResult::Stale);
    }

    // ---------------------------------------------------------------
    // lookup: empty file
    // ---------------------------------------------------------------

    #[test]
    fn lookup_empty_file() {
        let dir = TempDir::new().unwrap();
        let path = create_file(&dir, "empty.c", "");

        let cache = MetadataCache::new();
        let hash = cache.lookup(&path).unwrap();
        assert_eq!(hash, zccache_monocrate::hash::hash_bytes(b""));
    }

    // ---------------------------------------------------------------
    // lookup: file not found on cache miss
    // ---------------------------------------------------------------

    #[test]
    fn lookup_cache_miss_nonexistent() {
        let cache = MetadataCache::new();
        let result = cache.lookup(Path::new("/no/such/file.c"));
        assert!(result.is_err());
    }

    // ---------------------------------------------------------------
    // rescan_all: Low entries promoted if filesystem matches
    // ---------------------------------------------------------------

    #[test]
    fn rescan_all_promotes_matching_entries() {
        let dir = TempDir::new().unwrap();
        let path = create_file(&dir, "stable.c", "unchanged");

        let cache = MetadataCache::new();
        cache.lookup(&path).unwrap();

        // Simulate overflow: downgrade to Low.
        cache.downgrade_all();
        assert_eq!(cache.get(&path).unwrap().confidence, Confidence::Low);

        // Rescan: file unchanged → promoted back to High.
        let promoted = cache.rescan_all();
        assert_eq!(promoted, 1);
        assert_eq!(cache.get(&path).unwrap().confidence, Confidence::High);
    }

    #[test]
    fn rescan_all_leaves_changed_entries_low() {
        let dir = TempDir::new().unwrap();
        let path = create_file(&dir, "changed.c", "v1");

        let cache = MetadataCache::new();
        cache.lookup(&path).unwrap();
        cache.downgrade_all();

        sleep_for_mtime();
        fs::write(&path, "v2 longer content").unwrap();

        let promoted = cache.rescan_all();
        assert_eq!(promoted, 0);
        assert_eq!(cache.get(&path).unwrap().confidence, Confidence::Low);
    }

    #[test]
    fn rescan_all_skips_high_entries() {
        let dir = TempDir::new().unwrap();
        let path = create_file(&dir, "high.c", "content");

        let cache = MetadataCache::new();
        cache.lookup(&path).unwrap();

        // Entry is High — rescan should not count it.
        let promoted = cache.rescan_all();
        assert_eq!(promoted, 0);
    }

    #[test]
    fn rescan_all_handles_removed_files() {
        let dir = TempDir::new().unwrap();
        let path = create_file(&dir, "gone.c", "bye");

        let cache = MetadataCache::new();
        cache.lookup(&path).unwrap();
        cache.downgrade_all();

        fs::remove_file(&path).unwrap();

        let promoted = cache.rescan_all();
        assert_eq!(promoted, 0);
        assert_eq!(cache.get(&path).unwrap().confidence, Confidence::Low);
    }

    #[test]
    fn rescan_all_mixed_entries() {
        let dir = TempDir::new().unwrap();
        let path_unchanged = create_file(&dir, "same.c", "same");
        let path_changed = create_file(&dir, "diff.c", "old");
        let path_gone = create_file(&dir, "gone.c", "bye");

        let cache = MetadataCache::new();
        cache.lookup(&path_unchanged).unwrap();
        cache.lookup(&path_changed).unwrap();
        cache.lookup(&path_gone).unwrap();

        cache.downgrade_all();

        sleep_for_mtime();
        fs::write(&path_changed, "new content").unwrap();
        fs::remove_file(&path_gone).unwrap();

        let promoted = cache.rescan_all();
        assert_eq!(promoted, 1); // only path_unchanged
        assert_eq!(
            cache.get(&path_unchanged).unwrap().confidence,
            Confidence::High
        );
        assert_eq!(
            cache.get(&path_changed).unwrap().confidence,
            Confidence::Low
        );
        assert_eq!(cache.get(&path_gone).unwrap().confidence, Confidence::Low);
    }

    #[test]
    fn rescan_all_empty_cache() {
        let cache = MetadataCache::new();
        assert_eq!(cache.rescan_all(), 0);
    }

    // ---------------------------------------------------------------
    // lookup: High confidence with cached hash returns it directly
    // ---------------------------------------------------------------

    #[test]
    fn lookup_high_confidence_with_hash_returns_cached() {
        let dir = TempDir::new().unwrap();
        let path = create_file(&dir, "cached.c", "content");

        let cache = MetadataCache::new();
        let hash1 = cache.lookup(&path).unwrap();

        // Entry should be High with hash. Second lookup should reuse it.
        let entry = cache.get(&path).unwrap();
        assert_eq!(entry.confidence, Confidence::High);
        assert!(entry.content_hash.is_some());

        let hash2 = cache.lookup(&path).unwrap();
        assert_eq!(hash1, hash2);
    }

    // ---------------------------------------------------------------
    // verify: all three result variants
    // ---------------------------------------------------------------

    #[test]
    fn verify_result_variants() {
        assert_eq!(VerifyResult::Fresh, VerifyResult::Fresh);
        assert_ne!(VerifyResult::Fresh, VerifyResult::Stale);
        assert_ne!(VerifyResult::Fresh, VerifyResult::Gone);
        assert_ne!(VerifyResult::Stale, VerifyResult::Gone);
    }

    // ---------------------------------------------------------------
    // many files: bulk operations
    // ---------------------------------------------------------------

    #[test]
    fn bulk_lookup_and_selective_invalidation() {
        let dir = TempDir::new().unwrap();
        let mut paths = Vec::new();
        let mut original_hashes = Vec::new();

        for i in 0..20 {
            let path = create_file(&dir, &format!("file_{i}.c"), &format!("content {i}"));
            paths.push(path);
        }

        let cache = MetadataCache::new();

        for path in &paths {
            let hash = cache.lookup(path).unwrap();
            original_hashes.push(hash);
        }
        assert_eq!(cache.len(), 20);

        sleep_for_mtime();
        for (i, path) in paths.iter().enumerate() {
            if i % 3 == 0 {
                fs::write(path, format!("modified content {i}")).unwrap();
            }
        }

        for (i, path) in paths.iter().enumerate() {
            let hash = cache.lookup(path).unwrap();
            if i % 3 == 0 {
                assert_ne!(hash, original_hashes[i], "file {i} should have changed");
            } else {
                assert_eq!(hash, original_hashes[i], "file {i} should be unchanged");
            }
        }
    }
}
