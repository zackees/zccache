use std::path::Path;
use crate::core::NormalizedPath;

use rayon::prelude::*;

use super::decision::{CacheDecision, RunReason};
use super::error::Result;
use super::persist::{self, HashCacheData};
use super::scan::ScannedFile;

/// Compute an aggregate blake3 hash over a sorted list of files.
///
/// Each file contributes its relative path and content to the hash. Domain
/// separation and a file-count trailer prevent ambiguity and detect
/// empty ↔ non-empty transitions.
pub fn compute_aggregate_hash(files: &[ScannedFile]) -> Result<String> {
    // Phase 1: Read all file contents in parallel (I/O bound).
    let contents: std::result::Result<Vec<Vec<u8>>, std::io::Error> = files
        .par_iter()
        .map(|file| std::fs::read(&file.absolute))
        .collect();
    let contents = contents?;

    // Phase 2: Hash sequentially to preserve order-dependent aggregate hash.
    let mut hasher = crate::hash::StreamHasher::new();
    // Domain separation.
    hasher.update(b"zccache-fingerprint-v1");

    // Files are already sorted by relative path from walk_files().
    for (file, content) in files.iter().zip(contents.iter()) {
        hasher.update(file.relative.as_bytes());
        hasher.update(b"\0");
        hasher.update(content);
    }

    // Include file count to detect empty → non-empty transitions.
    hasher.update(b"\0file_count:");
    hasher.update(files.len().to_le_bytes().as_slice());

    Ok(hasher.finalize().to_hex())
}

/// Aggregate fingerprint cache: single blake3 hash of an entire file set.
///
/// Suited for operations where the decision is all-or-nothing (e.g., "run all
/// tests" or "rebuild everything"). Cheaper than `TwoLayerCache` when you only
/// need a single yes/no answer.
pub struct HashCache {
    cache_file: NormalizedPath,
}

impl HashCache {
    /// Create a new cache backed by the given file path.
    pub fn new(cache_file: impl Into<NormalizedPath>) -> Self {
        Self {
            cache_file: cache_file.into(),
        }
    }

    /// Check whether the operation needs to run.
    ///
    /// Computes an aggregate blake3 hash of all files (sorted by relative path)
    /// and compares against the cached hash.
    ///
    /// Uses an mtime fast-path: if the cache file is newer than all source files
    /// and the previous status was success with the same file count, returns
    /// `Skip` without reading any file contents.
    pub fn check(&self, files: &[ScannedFile]) -> Result<CacheDecision> {
        // Mtime fast-path: skip content hashing when cache is newer than all sources.
        if let Some(decision) = self.try_mtime_fast_path(files)? {
            return Ok(decision);
        }

        let current_hash = self.compute_hash(files)?;
        let file_count = files.len();
        let max_source_mtime = persist::max_mtime_ns(files).unwrap_or(0);

        let cached: Option<HashCacheData> = persist::read_json(&self.cache_file)?;

        let decision = match cached {
            None => CacheDecision::Run(RunReason::NoCacheFile),
            Some(data) if data.status == "failure" => {
                CacheDecision::Run(RunReason::PreviousFailure)
            }
            Some(data) if data.hash != current_hash => {
                CacheDecision::Run(RunReason::ContentChanged)
            }
            Some(_) => CacheDecision::Skip,
        };

        // Write pending with current state regardless of decision.
        let pending =
            HashCacheData::with_max_mtime(current_hash, "pending", file_count, max_source_mtime);
        persist::write_pending(&self.cache_file, &pending)?;

        Ok(decision)
    }

    /// Mark the operation as successful.
    pub fn mark_success(&self) -> Result<()> {
        self.promote_with_status("success")
    }

    /// Mark the operation as failed.
    pub fn mark_failure(&self) -> Result<()> {
        self.promote_with_status("failure")
    }

    /// Attempt to skip without walking the filesystem.
    ///
    /// Always returns `None` — the directory-level mtime optimization was
    /// removed because directory mtimes on NTFS and ext4 only change on
    /// file create/delete, not on in-place content edits. This caused
    /// false cache hits (see BUGS.md). The per-file `try_mtime_fast_path()`
    /// inside `check()` is the correct fast-path.
    pub fn try_skip_fast(&self, _root: &Path) -> Result<Option<CacheDecision>> {
        Ok(None)
    }

    /// Delete cache and pending files.
    pub fn invalidate(&self) -> Result<()> {
        persist::remove_cache(&self.cache_file);
        Ok(())
    }

    fn promote_with_status(&self, status: &str) -> Result<()> {
        let mut data =
            persist::read_pending::<HashCacheData>(&self.cache_file)?.ok_or_else(|| {
                super::error::FingerprintError::NoPendingData {
                    path: self.cache_file.clone(),
                }
            })?;
        data.status = status.to_string();
        data.timestamp_ns = persist::now_ns();
        persist::write_atomic(&self.cache_file, &data)?;
        let pending = self.cache_file.with_extension("pending");
        let _ = std::fs::remove_file(pending);
        Ok(())
    }

    fn try_mtime_fast_path(&self, files: &[ScannedFile]) -> Result<Option<CacheDecision>> {
        // Get cache file mtime. If cache doesn't exist, no fast-path.
        let cache_mtime = match persist::mtime_ns(&self.cache_file) {
            Ok(mt) => mt,
            Err(_) => return Ok(None),
        };

        // Get max source file mtime.
        let max_source_mtime = match persist::max_mtime_ns(files) {
            Ok(mt) => mt,
            Err(_) => return Ok(None),
        };

        // Cache must be strictly newer than all source files.
        if cache_mtime <= max_source_mtime {
            return Ok(None);
        }

        // Read the cache to check status and file count.
        let cached: Option<HashCacheData> = persist::read_json(&self.cache_file)?;
        match cached {
            Some(data) if data.status == "success" && data.file_count == files.len() => {
                tracing::debug!("mtime fast-path: cache is newer than all sources, skipping");
                Ok(Some(CacheDecision::Skip))
            }
            _ => Ok(None),
        }
    }

    fn compute_hash(&self, files: &[ScannedFile]) -> Result<String> {
        compute_aggregate_hash(files)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::scan;
    use std::fs;
    use tempfile::TempDir;

    fn setup() -> (TempDir, TempDir) {
        let src = TempDir::new().unwrap();
        let cache_dir = TempDir::new().unwrap();
        (src, cache_dir)
    }

    fn create_file(dir: &std::path::Path, rel: &str, content: &str) {
        let path = dir.join(rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(&path, content).unwrap();
    }

    fn scan_dir(dir: &std::path::Path) -> Vec<ScannedFile> {
        scan::walk_files(dir, &[], &[]).unwrap()
    }

    #[test]
    fn first_run_returns_no_cache_file() {
        let (src, cache_dir) = setup();
        create_file(src.path(), "a.rs", "fn main() {}");

        let cache = HashCache::new(cache_dir.path().join("fp.json"));
        let decision = cache.check(&scan_dir(src.path())).unwrap();
        assert_eq!(decision, CacheDecision::Run(RunReason::NoCacheFile));
    }

    #[test]
    fn no_changes_returns_skip() {
        let (src, cache_dir) = setup();
        create_file(src.path(), "a.rs", "fn main() {}");

        let cache = HashCache::new(cache_dir.path().join("fp.json"));
        cache.check(&scan_dir(src.path())).unwrap();
        cache.mark_success().unwrap();

        let decision = cache.check(&scan_dir(src.path())).unwrap();
        assert_eq!(decision, CacheDecision::Skip);
    }

    #[test]
    fn file_edit_returns_run() {
        let (src, cache_dir) = setup();
        create_file(src.path(), "a.rs", "v1");

        let cache = HashCache::new(cache_dir.path().join("fp.json"));
        cache.check(&scan_dir(src.path())).unwrap();
        cache.mark_success().unwrap();

        create_file(src.path(), "a.rs", "v2");
        let decision = cache.check(&scan_dir(src.path())).unwrap();
        assert_eq!(decision, CacheDecision::Run(RunReason::ContentChanged));
    }

    #[test]
    fn file_added_returns_run() {
        let (src, cache_dir) = setup();
        create_file(src.path(), "a.rs", "a");

        let cache = HashCache::new(cache_dir.path().join("fp.json"));
        cache.check(&scan_dir(src.path())).unwrap();
        cache.mark_success().unwrap();

        create_file(src.path(), "b.rs", "b");
        let decision = cache.check(&scan_dir(src.path())).unwrap();
        assert_eq!(decision, CacheDecision::Run(RunReason::ContentChanged));
    }

    #[test]
    fn file_removed_returns_run() {
        let (src, cache_dir) = setup();
        create_file(src.path(), "a.rs", "a");
        create_file(src.path(), "b.rs", "b");

        let cache = HashCache::new(cache_dir.path().join("fp.json"));
        cache.check(&scan_dir(src.path())).unwrap();
        cache.mark_success().unwrap();

        fs::remove_file(src.path().join("b.rs")).unwrap();
        let decision = cache.check(&scan_dir(src.path())).unwrap();
        assert_eq!(decision, CacheDecision::Run(RunReason::ContentChanged));
    }

    #[test]
    fn hash_is_deterministic() {
        let (src, cache_dir) = setup();
        create_file(src.path(), "b.rs", "b");
        create_file(src.path(), "a.rs", "a");

        let cache = HashCache::new(cache_dir.path().join("fp.json"));

        // First scan (files discovered in any order, but sorted).
        cache.check(&scan_dir(src.path())).unwrap();
        cache.mark_success().unwrap();

        // Second scan should match.
        let decision = cache.check(&scan_dir(src.path())).unwrap();
        assert_eq!(decision, CacheDecision::Skip);
    }

    #[test]
    fn previous_failure_returns_run() {
        let (src, cache_dir) = setup();
        create_file(src.path(), "a.rs", "a");

        let cache = HashCache::new(cache_dir.path().join("fp.json"));
        cache.check(&scan_dir(src.path())).unwrap();
        cache.mark_failure().unwrap();

        let decision = cache.check(&scan_dir(src.path())).unwrap();
        assert_eq!(decision, CacheDecision::Run(RunReason::PreviousFailure));
    }

    #[test]
    fn invalidate_clears_cache() {
        let (src, cache_dir) = setup();
        create_file(src.path(), "a.rs", "a");

        let cache = HashCache::new(cache_dir.path().join("fp.json"));
        cache.check(&scan_dir(src.path())).unwrap();
        cache.mark_success().unwrap();

        cache.invalidate().unwrap();

        let decision = cache.check(&scan_dir(src.path())).unwrap();
        assert_eq!(decision, CacheDecision::Run(RunReason::NoCacheFile));
    }

    #[test]
    fn corrupt_cache_returns_no_cache_file() {
        let (src, cache_dir) = setup();
        create_file(src.path(), "a.rs", "a");

        let cache_path = cache_dir.path().join("fp.json");
        fs::write(&cache_path, "garbage").unwrap();

        let cache = HashCache::new(cache_path);
        let decision = cache.check(&scan_dir(src.path())).unwrap();
        assert_eq!(decision, CacheDecision::Run(RunReason::NoCacheFile));
    }

    // ── Adversarial tests ─────────────────────────────────────────

    #[test]
    fn empty_file_set() {
        let (_src, cache_dir) = setup();
        let empty_dir = TempDir::new().unwrap();

        let cache = HashCache::new(cache_dir.path().join("fp.json"));
        let files = scan_dir(empty_dir.path());

        cache.check(&files).unwrap();
        cache.mark_success().unwrap();

        // Stable: empty set → empty set should skip.
        let decision = cache.check(&scan_dir(empty_dir.path())).unwrap();
        assert_eq!(decision, CacheDecision::Skip);
    }

    #[test]
    fn empty_to_nonempty_detected() {
        let (src, cache_dir) = setup();
        let cache = HashCache::new(cache_dir.path().join("fp.json"));

        // Start empty.
        let empty = scan_dir(src.path());
        cache.check(&empty).unwrap();
        cache.mark_success().unwrap();

        // Add a file.
        create_file(src.path(), "new.rs", "new");
        let decision = cache.check(&scan_dir(src.path())).unwrap();
        assert_eq!(decision, CacheDecision::Run(RunReason::ContentChanged));
    }

    #[test]
    fn nonempty_to_empty_detected() {
        let (src, cache_dir) = setup();
        create_file(src.path(), "a.rs", "a");

        let cache = HashCache::new(cache_dir.path().join("fp.json"));
        cache.check(&scan_dir(src.path())).unwrap();
        cache.mark_success().unwrap();

        // Remove the file.
        fs::remove_file(src.path().join("a.rs")).unwrap();
        let decision = cache.check(&scan_dir(src.path())).unwrap();
        assert_eq!(decision, CacheDecision::Run(RunReason::ContentChanged));
    }

    #[test]
    fn same_content_different_paths_different_hash() {
        // Two files with same content but different paths must produce
        // different aggregate hashes. Path is included in hash input.
        let (src, cache_dir) = setup();
        create_file(src.path(), "a.rs", "same");

        let cache = HashCache::new(cache_dir.path().join("fp.json"));
        cache.check(&scan_dir(src.path())).unwrap();
        cache.mark_success().unwrap();

        // Rename file (delete + create with different name, same content).
        fs::remove_file(src.path().join("a.rs")).unwrap();
        create_file(src.path(), "b.rs", "same");

        let decision = cache.check(&scan_dir(src.path())).unwrap();
        assert_eq!(decision, CacheDecision::Run(RunReason::ContentChanged));
    }

    #[test]
    fn check_without_mark_then_check_again() {
        let (src, cache_dir) = setup();
        create_file(src.path(), "a.rs", "v1");

        let cache = HashCache::new(cache_dir.path().join("fp.json"));

        // Full cycle.
        cache.check(&scan_dir(src.path())).unwrap();
        cache.mark_success().unwrap();

        // Check without mark (simulates crash).
        cache.check(&scan_dir(src.path())).unwrap();

        // Should still skip (reads main cache from successful cycle).
        let decision = cache.check(&scan_dir(src.path())).unwrap();
        assert_eq!(decision, CacheDecision::Skip);
    }

    #[test]
    fn mark_success_without_prior_check_errors() {
        let (_src, cache_dir) = setup();
        let cache = HashCache::new(cache_dir.path().join("fp.json"));
        let err = cache.mark_success().unwrap_err();
        assert!(
            matches!(err, super::super::error::FingerprintError::NoPendingData { .. }),
            "expected NoPendingData, got: {err}"
        );
    }

    #[test]
    fn mark_success_writes_success_status() {
        let (src, cache_dir) = setup();
        create_file(src.path(), "a.rs", "fn main() {}");

        let cache_path = cache_dir.path().join("fp.json");
        let cache = HashCache::new(cache_path.clone());
        cache.check(&scan_dir(src.path())).unwrap();
        cache.mark_success().unwrap();

        let data: persist::HashCacheData = persist::read_json(&cache_path).unwrap().unwrap();
        assert_eq!(data.status, "success");
    }

    #[test]
    fn mark_failure_writes_failure_status() {
        let (src, cache_dir) = setup();
        create_file(src.path(), "a.rs", "fn main() {}");

        let cache_path = cache_dir.path().join("fp.json");
        let cache = HashCache::new(cache_path.clone());
        cache.check(&scan_dir(src.path())).unwrap();
        cache.mark_failure().unwrap();

        let data: persist::HashCacheData = persist::read_json(&cache_path).unwrap().unwrap();
        assert_eq!(data.status, "failure");
    }

    #[test]
    fn failure_then_success_allows_skip() {
        let (src, cache_dir) = setup();
        create_file(src.path(), "a.rs", "a");

        let cache = HashCache::new(cache_dir.path().join("fp.json"));

        // Fail.
        cache.check(&scan_dir(src.path())).unwrap();
        cache.mark_failure().unwrap();

        // Re-run → PreviousFailure.
        let decision = cache.check(&scan_dir(src.path())).unwrap();
        assert_eq!(decision, CacheDecision::Run(RunReason::PreviousFailure));
        cache.mark_success().unwrap();

        // Now should skip.
        let decision = cache.check(&scan_dir(src.path())).unwrap();
        assert_eq!(decision, CacheDecision::Skip);
    }

    #[test]
    fn binary_content_handled() {
        let (src, cache_dir) = setup();
        let path = src.path().join("data.bin");
        fs::write(&path, [0u8, 1, 2, 255, 0, 128, 0, 0]).unwrap();

        let cache = HashCache::new(cache_dir.path().join("fp.json"));
        cache.check(&scan_dir(src.path())).unwrap();
        cache.mark_success().unwrap();

        let decision = cache.check(&scan_dir(src.path())).unwrap();
        assert_eq!(decision, CacheDecision::Skip);
    }

    #[test]
    fn content_with_null_bytes_no_delimiter_collision() {
        // File content containing "\0" (the same byte used as delimiter
        // between path and content in the hash) should not collide with
        // a different file layout.
        let (src, cache_dir) = setup();
        // File "a\0b" is not a valid filename, but content can have \0.
        create_file(src.path(), "a.txt", "hello\0world");

        let cache = HashCache::new(cache_dir.path().join("fp.json"));
        cache.check(&scan_dir(src.path())).unwrap();
        cache.mark_success().unwrap();

        // Change content after the null byte.
        create_file(src.path(), "a.txt", "hello\0changed");
        let decision = cache.check(&scan_dir(src.path())).unwrap();
        assert_eq!(decision, CacheDecision::Run(RunReason::ContentChanged));
    }

    #[test]
    fn many_files_all_unchanged() {
        let (src, cache_dir) = setup();
        for i in 0..100 {
            create_file(
                src.path(),
                &format!("mod_{i:03}.rs"),
                &format!("content {i}"),
            );
        }

        let cache = HashCache::new(cache_dir.path().join("fp.json"));
        cache.check(&scan_dir(src.path())).unwrap();
        cache.mark_success().unwrap();

        let decision = cache.check(&scan_dir(src.path())).unwrap();
        assert_eq!(decision, CacheDecision::Skip);
    }

    #[test]
    fn cache_in_deep_directory_auto_created() {
        let (src, cache_dir) = setup();
        create_file(src.path(), "a.rs", "a");

        let deep = cache_dir.path().join("x/y/z/fp.json");
        let cache = HashCache::new(deep);
        cache.check(&scan_dir(src.path())).unwrap();
        cache.mark_success().unwrap();

        let decision = cache.check(&scan_dir(src.path())).unwrap();
        assert_eq!(decision, CacheDecision::Skip);
    }

    #[test]
    fn double_invalidate_safe() {
        let (src, cache_dir) = setup();
        create_file(src.path(), "a.rs", "a");

        let cache = HashCache::new(cache_dir.path().join("fp.json"));
        cache.check(&scan_dir(src.path())).unwrap();
        cache.mark_success().unwrap();

        cache.invalidate().unwrap();
        cache.invalidate().unwrap(); // Should not panic.
    }

    #[test]
    fn swap_two_files_detected() {
        // Swapping content between two files should change the hash,
        // because path is included in the hash input.
        let (src, cache_dir) = setup();
        create_file(src.path(), "a.rs", "alpha");
        create_file(src.path(), "b.rs", "beta");

        let cache = HashCache::new(cache_dir.path().join("fp.json"));
        cache.check(&scan_dir(src.path())).unwrap();
        cache.mark_success().unwrap();

        // Swap contents.
        create_file(src.path(), "a.rs", "beta");
        create_file(src.path(), "b.rs", "alpha");

        let decision = cache.check(&scan_dir(src.path())).unwrap();
        assert_eq!(decision, CacheDecision::Run(RunReason::ContentChanged));
    }

    #[test]
    fn two_caches_same_files_independent() {
        // Two HashCache instances pointing at different cache files but
        // scanning the same source files should be independent.
        let (src, cache_dir) = setup();
        create_file(src.path(), "a.rs", "a");

        let cache1 = HashCache::new(cache_dir.path().join("c1.json"));
        let cache2 = HashCache::new(cache_dir.path().join("c2.json"));

        cache1.check(&scan_dir(src.path())).unwrap();
        cache1.mark_success().unwrap();

        // cache2 hasn't been initialized yet.
        let decision = cache2.check(&scan_dir(src.path())).unwrap();
        assert_eq!(decision, CacheDecision::Run(RunReason::NoCacheFile));
        cache2.mark_success().unwrap();

        // Both should now skip.
        assert_eq!(
            cache1.check(&scan_dir(src.path())).unwrap(),
            CacheDecision::Skip
        );
        assert_eq!(
            cache2.check(&scan_dir(src.path())).unwrap(),
            CacheDecision::Skip
        );
    }

    #[test]
    fn file_content_same_as_path_no_confusion() {
        // A file whose content is the same string as another file's path
        // should not cause hash collisions due to the \0 separator.
        let (src, cache_dir) = setup();
        create_file(src.path(), "a.txt", "b.txt");
        create_file(src.path(), "b.txt", "a.txt");

        let cache = HashCache::new(cache_dir.path().join("fp.json"));
        cache.check(&scan_dir(src.path())).unwrap();
        cache.mark_success().unwrap();

        // Swap the contents.
        create_file(src.path(), "a.txt", "a.txt");
        create_file(src.path(), "b.txt", "b.txt");

        let decision = cache.check(&scan_dir(src.path())).unwrap();
        assert_eq!(decision, CacheDecision::Run(RunReason::ContentChanged));
    }

    #[test]
    fn parallel_aggregate_hash_deterministic() {
        // Hash the same file set multiple times; parallel reads must produce
        // the same aggregate hash every time.
        let (src, _) = setup();
        for i in 0..100 {
            create_file(src.path(), &format!("f{i:03}.rs"), &format!("content {i}"));
        }
        let files = scan_dir(src.path());

        let h1 = compute_aggregate_hash(&files).unwrap();
        let h2 = compute_aggregate_hash(&files).unwrap();
        let h3 = compute_aggregate_hash(&files).unwrap();
        assert_eq!(h1, h2);
        assert_eq!(h2, h3);
    }

    // ── Dir fast-path tests ──────────────────────────────────────

    #[test]
    fn try_skip_fast_always_returns_none() {
        let (src, cache_dir) = setup();
        create_file(src.path(), "a.rs", "content");

        let cache_file = cache_dir.path().join("fp.json");
        let cache = HashCache::new(cache_file.clone());
        cache.check(&scan_dir(src.path())).unwrap();
        cache.mark_success().unwrap();

        // try_skip_fast is disabled — always falls through to per-file check.
        let cache = HashCache::new(cache_file);
        let decision = cache.try_skip_fast(src.path()).unwrap();
        assert_eq!(decision, None);
    }

    #[test]
    fn in_place_edit_detected_after_mark_success() {
        // Regression test for BUGS.md: in-place edits must NOT be missed.
        let (src, cache_dir) = setup();
        create_file(src.path(), "a.rs", "original");

        let cache_file = cache_dir.path().join("fp.json");
        let cache = HashCache::new(cache_file.clone());
        cache.check(&scan_dir(src.path())).unwrap();
        cache.mark_success().unwrap();

        // In-place edit.
        std::thread::sleep(std::time::Duration::from_millis(1100));
        create_file(src.path(), "a.rs", "modified");

        // try_skip_fast must not skip.
        let cache = HashCache::new(cache_file.clone());
        let decision = cache.try_skip_fast(src.path()).unwrap();
        assert_eq!(decision, None);

        // Full check must detect the change.
        let cache = HashCache::new(cache_file);
        let decision = cache.check(&scan_dir(src.path())).unwrap();
        assert_eq!(decision, CacheDecision::Run(RunReason::ContentChanged));
    }
}
