use std::collections::BTreeMap;
use std::path::Path;
use zccache_monocrate::core::NormalizedPath;

use rayon::prelude::*;

use super::decision::{CacheDecision, RunReason};
use super::error::Result;
use super::persist::{self, FileEntry, TwoLayerData};
use super::scan::ScannedFile;

/// Per-file fingerprint cache with mtime fast-path and blake3 verification.
///
/// Layer 1: If mtime + size match the cached entry, skip hashing entirely.
/// Layer 2: If mtime differs, compute blake3. If hash matches, the file was
/// merely touched (e.g., `git checkout`) — update cached mtime silently.
pub struct TwoLayerCache {
    cache_file: NormalizedPath,
}

impl TwoLayerCache {
    /// Create a new cache backed by the given file path.
    pub fn new(cache_file: impl Into<NormalizedPath>) -> Self {
        Self {
            cache_file: cache_file.into(),
        }
    }

    /// Check whether the operation needs to run.
    ///
    /// Compares each file against cached state. Writes a `.pending` file with
    /// the current snapshot so that `mark_success()` can promote it atomically.
    ///
    /// Uses an mtime fast-path: if the cache file is newer than all source files
    /// and the previous status was success with the same file count, returns
    /// `Skip` without per-file stat+hash checks.
    pub fn check(&self, files: &[ScannedFile]) -> Result<CacheDecision> {
        // Mtime fast-path: skip per-file checks when cache is newer than all sources.
        if let Some(decision) = self.try_mtime_fast_path(files)? {
            return Ok(decision);
        }

        let cached: Option<TwoLayerData> = persist::read_json(&self.cache_file)?;

        let (cached_files, prev_status) = match cached {
            Some(data) => (data.files, data.status),
            None => {
                // No cache — compute everything and write pending.
                let entries = self.compute_all(files)?;
                let pending = TwoLayerData::new("pending", entries);
                persist::write_pending(&self.cache_file, &pending)?;
                return Ok(CacheDecision::Run(RunReason::NoCacheFile));
            }
        };

        if prev_status == "failure" {
            let entries = self.compute_all(files)?;
            let pending = TwoLayerData::new("pending", entries);
            persist::write_pending(&self.cache_file, &pending)?;
            return Ok(CacheDecision::Run(RunReason::PreviousFailure));
        }

        // Parallel stat + conditional hash for each file.
        let results: Result<Vec<_>> = files
            .par_iter()
            .map(|file| {
                let mtime = persist::mtime_ns(&file.absolute)?;
                let size = persist::file_size(&file.absolute)?;

                if let Some(cached_entry) = cached_files.get(&file.relative) {
                    if cached_entry.mtime_ns == mtime && cached_entry.size == size {
                        // Layer 1: mtime + size match → reuse cached hash, no I/O.
                        Ok((file.relative.clone(), cached_entry.clone(), false))
                    } else {
                        // Layer 2: mtime or size differ → hash the file.
                        let hash = zccache_monocrate::hash::hash_file(&file.absolute)?;
                        let hash_hex = hash.to_hex();
                        let content_changed = hash_hex != cached_entry.hash;
                        Ok((
                            file.relative.clone(),
                            FileEntry {
                                mtime_ns: mtime,
                                size,
                                hash: hash_hex,
                            },
                            content_changed,
                        ))
                    }
                } else {
                    // New file not in cache.
                    let hash = zccache_monocrate::hash::hash_file(&file.absolute)?;
                    Ok((
                        file.relative.clone(),
                        FileEntry {
                            mtime_ns: mtime,
                            size,
                            hash: hash.to_hex(),
                        },
                        true,
                    ))
                }
            })
            .collect();
        let results = results?;

        let mut changed = results.iter().any(|(_, _, c)| *c);
        let mut entries = BTreeMap::new();
        for (rel, entry, _) in results {
            entries.insert(rel, entry);
        }

        // Check for removed files.
        if cached_files.keys().any(|k| !entries.contains_key(k)) {
            changed = true;
        }

        let pending = TwoLayerData::new("pending", entries);
        persist::write_pending(&self.cache_file, &pending)?;

        if changed {
            Ok(CacheDecision::Run(RunReason::ContentChanged))
        } else {
            Ok(CacheDecision::Skip)
        }
    }

    /// Mark the operation as successful. Promotes the pending snapshot to the
    /// cache file with status `"success"`.
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

    fn try_mtime_fast_path(&self, files: &[ScannedFile]) -> Result<Option<CacheDecision>> {
        let cache_mtime = match persist::mtime_ns(&self.cache_file) {
            Ok(mt) => mt,
            Err(_) => return Ok(None),
        };

        let max_source_mtime = match persist::max_mtime_ns(files) {
            Ok(mt) => mt,
            Err(_) => return Ok(None),
        };

        if cache_mtime <= max_source_mtime {
            return Ok(None);
        }

        let cached: Option<TwoLayerData> = persist::read_json(&self.cache_file)?;
        match cached {
            Some(data) if data.status == "success" && data.files.len() == files.len() => {
                tracing::debug!("mtime fast-path: cache is newer than all sources, skipping");
                Ok(Some(CacheDecision::Skip))
            }
            _ => Ok(None),
        }
    }

    fn promote_with_status(&self, status: &str) -> Result<()> {
        let mut data =
            persist::read_pending::<TwoLayerData>(&self.cache_file)?.ok_or_else(|| {
                super::error::FingerprintError::NoPendingData {
                    path: self.cache_file.clone(),
                }
            })?;
        data.status = status.to_string();
        data.timestamp_ns = persist::now_ns();
        persist::write_atomic(&self.cache_file, &data)?;
        let pending = NormalizedPath::from(self.cache_file.with_extension("pending"));
        let _ = std::fs::remove_file(pending);
        Ok(())
    }

    fn compute_all(&self, files: &[ScannedFile]) -> Result<BTreeMap<String, FileEntry>> {
        let results: Result<Vec<_>> = files
            .par_iter()
            .map(|file| {
                let mtime = persist::mtime_ns(&file.absolute)?;
                let size = persist::file_size(&file.absolute)?;
                let hash = zccache_monocrate::hash::hash_file(&file.absolute)?;
                Ok((
                    file.relative.clone(),
                    FileEntry {
                        mtime_ns: mtime,
                        size,
                        hash: hash.to_hex(),
                    },
                ))
            })
            .collect();
        Ok(results?.into_iter().collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::{persist, scan};
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

    fn scan(dir: &std::path::Path) -> Vec<ScannedFile> {
        scan::walk_files(dir, &[], &[]).unwrap()
    }

    #[test]
    fn first_run_returns_no_cache_file() {
        let (src, cache_dir) = setup();
        create_file(src.path(), "a.rs", "fn main() {}");

        let cache = TwoLayerCache::new(cache_dir.path().join("fp.json"));
        let files = scan(src.path());
        let decision = cache.check(&files).unwrap();
        assert_eq!(decision, CacheDecision::Run(RunReason::NoCacheFile));
    }

    #[test]
    fn no_changes_returns_skip() {
        let (src, cache_dir) = setup();
        create_file(src.path(), "a.rs", "fn main() {}");

        let cache = TwoLayerCache::new(cache_dir.path().join("fp.json"));
        let files = scan(src.path());

        let decision = cache.check(&files).unwrap();
        assert!(decision.should_run());
        cache.mark_success().unwrap();

        // Second check with no changes.
        let files = scan(src.path());
        let decision = cache.check(&files).unwrap();
        assert_eq!(decision, CacheDecision::Skip);
    }

    #[test]
    fn content_change_returns_run() {
        let (src, cache_dir) = setup();
        create_file(src.path(), "a.rs", "version 1");

        let cache = TwoLayerCache::new(cache_dir.path().join("fp.json"));
        cache.check(&scan(src.path())).unwrap();
        cache.mark_success().unwrap();

        // Ensure mtime changes (filesystem granularity).
        std::thread::sleep(std::time::Duration::from_millis(1100));
        create_file(src.path(), "a.rs", "version 2");

        let decision = cache.check(&scan(src.path())).unwrap();
        assert_eq!(decision, CacheDecision::Run(RunReason::ContentChanged));
    }

    #[test]
    fn touch_same_content_returns_skip() {
        let (src, cache_dir) = setup();
        create_file(src.path(), "a.rs", "unchanged");

        let cache = TwoLayerCache::new(cache_dir.path().join("fp.json"));
        cache.check(&scan(src.path())).unwrap();
        cache.mark_success().unwrap();

        // Touch the file (same content, new mtime).
        std::thread::sleep(std::time::Duration::from_millis(1100));
        create_file(src.path(), "a.rs", "unchanged");

        let decision = cache.check(&scan(src.path())).unwrap();
        assert_eq!(decision, CacheDecision::Skip);
    }

    #[test]
    fn file_added_returns_run() {
        let (src, cache_dir) = setup();
        create_file(src.path(), "a.rs", "a");

        let cache = TwoLayerCache::new(cache_dir.path().join("fp.json"));
        cache.check(&scan(src.path())).unwrap();
        cache.mark_success().unwrap();

        create_file(src.path(), "b.rs", "b");
        let decision = cache.check(&scan(src.path())).unwrap();
        assert_eq!(decision, CacheDecision::Run(RunReason::ContentChanged));
    }

    #[test]
    fn file_removed_returns_run() {
        let (src, cache_dir) = setup();
        create_file(src.path(), "a.rs", "a");
        create_file(src.path(), "b.rs", "b");

        let cache = TwoLayerCache::new(cache_dir.path().join("fp.json"));
        cache.check(&scan(src.path())).unwrap();
        cache.mark_success().unwrap();

        fs::remove_file(src.path().join("b.rs")).unwrap();
        let decision = cache.check(&scan(src.path())).unwrap();
        assert_eq!(decision, CacheDecision::Run(RunReason::ContentChanged));
    }

    #[test]
    fn previous_failure_returns_run() {
        let (src, cache_dir) = setup();
        create_file(src.path(), "a.rs", "a");

        let cache = TwoLayerCache::new(cache_dir.path().join("fp.json"));
        cache.check(&scan(src.path())).unwrap();
        cache.mark_failure().unwrap();

        let decision = cache.check(&scan(src.path())).unwrap();
        assert_eq!(decision, CacheDecision::Run(RunReason::PreviousFailure));
    }

    #[test]
    fn corrupt_cache_returns_no_cache_file() {
        let (src, cache_dir) = setup();
        create_file(src.path(), "a.rs", "a");

        let cache_path = cache_dir.path().join("fp.json");
        fs::write(&cache_path, "not valid json!!!").unwrap();

        let cache = TwoLayerCache::new(cache_path);
        let decision = cache.check(&scan(src.path())).unwrap();
        // Corrupt cache is treated as missing (fail-open).
        assert_eq!(decision, CacheDecision::Run(RunReason::NoCacheFile));
    }

    #[test]
    fn invalidate_clears_cache() {
        let (src, cache_dir) = setup();
        create_file(src.path(), "a.rs", "a");

        let cache = TwoLayerCache::new(cache_dir.path().join("fp.json"));
        cache.check(&scan(src.path())).unwrap();
        cache.mark_success().unwrap();

        cache.invalidate().unwrap();

        let decision = cache.check(&scan(src.path())).unwrap();
        assert_eq!(decision, CacheDecision::Run(RunReason::NoCacheFile));
    }

    // ── Adversarial tests ─────────────────────────────────────────

    #[test]
    fn empty_file_set() {
        let (_src, cache_dir) = setup();
        let empty_dir = TempDir::new().unwrap();

        let cache = TwoLayerCache::new(cache_dir.path().join("fp.json"));
        let files = scan(empty_dir.path());
        assert!(files.is_empty());

        // First check with empty set → NoCacheFile.
        let decision = cache.check(&files).unwrap();
        assert_eq!(decision, CacheDecision::Run(RunReason::NoCacheFile));
        cache.mark_success().unwrap();

        // Second check with empty set → Skip (nothing changed).
        let decision = cache.check(&files).unwrap();
        assert_eq!(decision, CacheDecision::Skip);
    }

    #[test]
    fn same_size_different_content_detected() {
        // Two strings of identical length but different content.
        let (src, cache_dir) = setup();
        create_file(src.path(), "a.rs", "aaaa");

        let cache = TwoLayerCache::new(cache_dir.path().join("fp.json"));
        cache.check(&scan(src.path())).unwrap();
        cache.mark_success().unwrap();

        // Same length, different content. Must wait for mtime to change.
        std::thread::sleep(std::time::Duration::from_millis(1100));
        create_file(src.path(), "a.rs", "bbbb");

        let decision = cache.check(&scan(src.path())).unwrap();
        assert_eq!(decision, CacheDecision::Run(RunReason::ContentChanged));
    }

    #[test]
    fn check_without_mark_then_check_again() {
        // If check() is called but mark_success() is never called (crash),
        // the next check() should still work against the old cache state.
        let (src, cache_dir) = setup();
        create_file(src.path(), "a.rs", "v1");

        let cache = TwoLayerCache::new(cache_dir.path().join("fp.json"));

        // First full cycle.
        cache.check(&scan(src.path())).unwrap();
        cache.mark_success().unwrap();

        // Second check without mark (simulates crash).
        cache.check(&scan(src.path())).unwrap();
        // Don't call mark_success()!

        // Third check should still see the cache from step 1.
        let decision = cache.check(&scan(src.path())).unwrap();
        assert_eq!(decision, CacheDecision::Skip);
    }

    #[test]
    fn mark_success_without_prior_check_errors() {
        let (_src, cache_dir) = setup();
        let cache = TwoLayerCache::new(cache_dir.path().join("fp.json"));
        let err = cache.mark_success().unwrap_err();
        assert!(
            matches!(err, super::error::FingerprintError::NoPendingData { .. }),
            "expected NoPendingData, got: {err}"
        );
    }

    #[test]
    fn mark_failure_without_prior_check_errors() {
        let (_src, cache_dir) = setup();
        let cache = TwoLayerCache::new(cache_dir.path().join("fp.json"));
        let err = cache.mark_failure().unwrap_err();
        assert!(
            matches!(err, super::error::FingerprintError::NoPendingData { .. }),
            "expected NoPendingData, got: {err}"
        );
    }

    #[test]
    fn mark_success_writes_success_status() {
        let (src, cache_dir) = setup();
        create_file(src.path(), "a.rs", "fn main() {}");

        let cache_path = cache_dir.path().join("fp.json");
        let cache = TwoLayerCache::new(cache_path.clone());
        cache.check(&scan(src.path())).unwrap();
        cache.mark_success().unwrap();

        let data: persist::TwoLayerData = persist::read_json(&cache_path).unwrap().unwrap();
        assert_eq!(data.status, "success");
    }

    #[test]
    fn mark_failure_writes_failure_status() {
        let (src, cache_dir) = setup();
        create_file(src.path(), "a.rs", "fn main() {}");

        let cache_path = cache_dir.path().join("fp.json");
        let cache = TwoLayerCache::new(cache_path.clone());
        cache.check(&scan(src.path())).unwrap();
        cache.mark_failure().unwrap();

        let data: persist::TwoLayerData = persist::read_json(&cache_path).unwrap().unwrap();
        assert_eq!(data.status, "failure");
    }

    #[test]
    fn failure_then_success_allows_skip() {
        let (src, cache_dir) = setup();
        create_file(src.path(), "a.rs", "a");

        let cache = TwoLayerCache::new(cache_dir.path().join("fp.json"));

        // Fail first.
        cache.check(&scan(src.path())).unwrap();
        cache.mark_failure().unwrap();

        // Now succeed.
        let decision = cache.check(&scan(src.path())).unwrap();
        assert_eq!(decision, CacheDecision::Run(RunReason::PreviousFailure));
        cache.mark_success().unwrap();

        // Should skip now.
        let decision = cache.check(&scan(src.path())).unwrap();
        assert_eq!(decision, CacheDecision::Skip);
    }

    #[test]
    fn binary_content_handled() {
        let (src, cache_dir) = setup();
        let binary = src.path().join("data.bin");
        fs::write(&binary, [0u8, 1, 2, 255, 0, 128]).unwrap();

        let cache = TwoLayerCache::new(cache_dir.path().join("fp.json"));
        let files = scan(src.path());
        cache.check(&files).unwrap();
        cache.mark_success().unwrap();

        let decision = cache.check(&scan(src.path())).unwrap();
        assert_eq!(decision, CacheDecision::Skip);
    }

    #[test]
    fn many_files_performance() {
        let (src, cache_dir) = setup();
        for i in 0..100 {
            create_file(
                src.path(),
                &format!("src/mod_{i:03}.rs"),
                &format!("mod {i}"),
            );
        }

        let cache = TwoLayerCache::new(cache_dir.path().join("fp.json"));
        cache.check(&scan(src.path())).unwrap();
        cache.mark_success().unwrap();

        // All files unchanged → Skip.
        let decision = cache.check(&scan(src.path())).unwrap();
        assert_eq!(decision, CacheDecision::Skip);

        // Change one file.
        std::thread::sleep(std::time::Duration::from_millis(1100));
        create_file(src.path(), "src/mod_050.rs", "CHANGED");

        let decision = cache.check(&scan(src.path())).unwrap();
        assert_eq!(decision, CacheDecision::Run(RunReason::ContentChanged));
    }

    #[test]
    fn cache_in_nonexistent_directory_auto_created() {
        let (src, cache_dir) = setup();
        create_file(src.path(), "a.rs", "a");

        let deep_cache = cache_dir.path().join("a/b/c/fp.json");
        let cache = TwoLayerCache::new(deep_cache);
        cache.check(&scan(src.path())).unwrap();
        cache.mark_success().unwrap();

        let decision = cache.check(&scan(src.path())).unwrap();
        assert_eq!(decision, CacheDecision::Skip);
    }

    #[test]
    fn double_invalidate_is_safe() {
        let (src, cache_dir) = setup();
        create_file(src.path(), "a.rs", "a");

        let cache = TwoLayerCache::new(cache_dir.path().join("fp.json"));
        cache.check(&scan(src.path())).unwrap();
        cache.mark_success().unwrap();

        cache.invalidate().unwrap();
        cache.invalidate().unwrap(); // Should not panic.
    }

    #[test]
    fn empty_file_content_change_detected() {
        let (src, cache_dir) = setup();
        create_file(src.path(), "a.rs", "");

        let cache = TwoLayerCache::new(cache_dir.path().join("fp.json"));
        cache.check(&scan(src.path())).unwrap();
        cache.mark_success().unwrap();

        // Write content to previously-empty file.
        std::thread::sleep(std::time::Duration::from_millis(1100));
        create_file(src.path(), "a.rs", "now has content");

        let decision = cache.check(&scan(src.path())).unwrap();
        assert_eq!(decision, CacheDecision::Run(RunReason::ContentChanged));
    }

    #[test]
    fn smart_touch_updates_cached_mtime() {
        // After a smart-touch (same content, new mtime), the NEXT check
        // should skip without needing to hash again (mtime now matches).
        let (src, cache_dir) = setup();
        create_file(src.path(), "a.rs", "stable");

        let cache = TwoLayerCache::new(cache_dir.path().join("fp.json"));
        cache.check(&scan(src.path())).unwrap();
        cache.mark_success().unwrap();

        // Touch file (same content).
        std::thread::sleep(std::time::Duration::from_millis(1100));
        create_file(src.path(), "a.rs", "stable");

        // First re-check: triggers Layer 2 (hash), but hash matches → Skip.
        let decision = cache.check(&scan(src.path())).unwrap();
        assert_eq!(decision, CacheDecision::Skip);
        cache.mark_success().unwrap();

        // Second re-check WITHOUT touching: should use Layer 1 (mtime match)
        // because mark_success saved the updated mtime.
        let decision = cache.check(&scan(src.path())).unwrap();
        assert_eq!(decision, CacheDecision::Skip);
    }

    #[test]
    fn file_replaced_with_subdirectory_file() {
        // Replace a flat file with a file in a subdirectory.
        let (src, cache_dir) = setup();
        create_file(src.path(), "a.rs", "flat");

        let cache = TwoLayerCache::new(cache_dir.path().join("fp.json"));
        cache.check(&scan(src.path())).unwrap();
        cache.mark_success().unwrap();

        fs::remove_file(src.path().join("a.rs")).unwrap();
        create_file(src.path(), "src/a.rs", "nested");

        let decision = cache.check(&scan(src.path())).unwrap();
        assert_eq!(decision, CacheDecision::Run(RunReason::ContentChanged));
    }

    #[test]
    fn parallel_compute_all_correctness() {
        // Verify parallel compute_all produces correct entries by checking
        // that a miss+mark_success followed by a hit cycle works correctly.
        let (src, cache_dir) = setup();
        for i in 0..50 {
            create_file(src.path(), &format!("f{i:02}.rs"), &format!("data {i}"));
        }

        let cache = TwoLayerCache::new(cache_dir.path().join("fp.json"));
        let files = scan(src.path());
        let decision = cache.check(&files).unwrap();
        assert!(decision.should_run());
        cache.mark_success().unwrap();

        // Second check must skip (proves parallel entries are identical to what
        // the sequential check would have produced).
        let decision = cache.check(&scan(src.path())).unwrap();
        assert_eq!(decision, CacheDecision::Skip);
    }

    // ── Dir fast-path tests ──────────────────────────────────────
    // try_skip_fast() was disabled (always returns None) because directory-level
    // mtimes on NTFS/ext4 don't reflect in-place content edits (BUGS.md).

    #[test]
    fn try_skip_fast_always_returns_none() {
        let (src, cache_dir) = setup();
        create_file(src.path(), "a.rs", "content");

        let cache_file = cache_dir.path().join("fp.json");
        let cache = TwoLayerCache::new(cache_file.clone());
        cache.check(&scan(src.path())).unwrap();
        cache.mark_success().unwrap();

        // try_skip_fast is disabled — always falls through to per-file check.
        let cache = TwoLayerCache::new(cache_file);
        let decision = cache.try_skip_fast(src.path()).unwrap();
        assert_eq!(decision, None);
    }

    #[test]
    fn in_place_edit_detected_after_mark_success() {
        // Regression test for BUGS.md: in-place edits must NOT be missed.
        let (src, cache_dir) = setup();
        create_file(src.path(), "a.rs", "original");

        let cache_file = cache_dir.path().join("fp.json");
        let cache = TwoLayerCache::new(cache_file.clone());
        cache.check(&scan(src.path())).unwrap();
        cache.mark_success().unwrap();

        // In-place edit (directory mtime does NOT change, but file mtime does).
        std::thread::sleep(std::time::Duration::from_millis(1100));
        create_file(src.path(), "a.rs", "modified");

        // try_skip_fast must not skip.
        let cache = TwoLayerCache::new(cache_file);
        let decision = cache.try_skip_fast(src.path()).unwrap();
        assert_eq!(decision, None);

        // Full check must detect the change.
        let decision = cache.check(&scan(src.path())).unwrap();
        assert_eq!(decision, CacheDecision::Run(RunReason::ContentChanged));
    }
}
