//! Unified cache system: metadata cache + change journal.
//!
//! This is the primary API that the daemon uses. It composes
//! `MetadataCache` (per-file metadata + hashing) with `ChangeJournal`
//! (monotonic clock + batched change tracking) to provide clock-aware
//! lookups that eliminate redundant stat calls across concurrent clients.

use super::clock::{ChangeJournal, Clock};
use super::metadata::MetadataCache;
use zccache_monocrate::core::NormalizedPath;
use zccache_monocrate::core::Result;
use zccache_monocrate::hash::ContentHash;

/// Result of a clock-aware lookup.
#[derive(Debug, Clone)]
pub struct ClockLookup {
    /// The content hash of the file.
    pub hash: ContentHash,
    /// The clock at the time of lookup.
    pub clock: Clock,
}

/// Unified cache system: metadata cache + change journal.
///
/// During `make -j16`, 16 concurrent compilation requests all ask about
/// the same headers. Without the clock, each one would stat-verify
/// independently. With the clock, the daemon verifies once per batch
/// and all concurrent clients share that answer.
#[derive(Debug)]
pub struct CacheSystem {
    metadata: MetadataCache,
    journal: ChangeJournal,
}

impl CacheSystem {
    /// Create a new cache system with default settings.
    #[must_use]
    pub fn new() -> Self {
        Self {
            metadata: MetadataCache::new(),
            journal: ChangeJournal::new(),
        }
    }

    /// Create a new cache system with a pre-populated [`MetadataCache`].
    ///
    /// The journal is fresh (`Clock::ZERO`) because [`Clock`] is a
    /// process-local monotonic counter and cannot be persisted across
    /// daemon restarts. Use this on startup after restoring the metadata
    /// snapshot from disk; subsequent `lookup_since` calls with the
    /// freshly-restored entries still take the stat-verify safety-net
    /// path in `MetadataCache::get_cached_hash_if_stat_valid`, so a
    /// stale snapshot remains correctness-safe.
    #[must_use]
    pub fn with_metadata(metadata: MetadataCache) -> Self {
        Self {
            metadata,
            journal: ChangeJournal::new(),
        }
    }

    /// Returns the current clock value.
    #[must_use]
    pub fn current_clock(&self) -> Clock {
        self.journal.current_clock()
    }

    /// Access the underlying metadata cache.
    #[must_use]
    pub fn metadata(&self) -> &MetadataCache {
        &self.metadata
    }

    /// Access the underlying change journal.
    #[must_use]
    pub fn journal(&self) -> &ChangeJournal {
        &self.journal
    }

    /// Clock-aware lookup. The critical fast path for concurrent clients.
    ///
    /// When the journal says the file hasn't changed since `since_clock` AND
    /// the cached `(mtime, size)` still match the filesystem, returns the
    /// cached hash — **one stat syscall, zero hashing**.
    ///
    /// Otherwise falls through to the full stat-verify + hash path.
    ///
    /// # Errors
    ///
    /// Returns an error if the file cannot be read.
    pub fn lookup_since(&self, path: &NormalizedPath, since_clock: Clock) -> Result<ClockLookup> {
        let clock = self.journal.current_clock();

        // Fast path: journal says no changes AND stat confirms mtime+size match.
        // The stat guards against watcher latency: even if the watcher hasn't
        // delivered the event yet, a changed mtime/size falls through to re-hash.
        if !self.journal.changed_since(path, since_clock) {
            if let Some(hash) = self.metadata.get_cached_hash_if_stat_valid(path) {
                return Ok(ClockLookup { hash, clock });
            }
        }

        // Slow path: stat-verify and hash via MetadataCache.
        let hash = self.metadata.lookup(path.as_path())?;
        Ok(ClockLookup {
            hash,
            clock: self.journal.current_clock(),
        })
    }

    /// Apply a settled batch of file changes from the watcher.
    ///
    /// Advances the journal clock and downgrades MetadataCache confidence
    /// for each affected path. Called by the settle buffer after coalescing.
    ///
    /// Returns the new clock value.
    pub fn apply_changes(&self, changed_paths: Vec<NormalizedPath>) -> Clock {
        // Downgrade confidence for each changed path.
        for path in &changed_paths {
            self.metadata.downgrade(path);
        }

        // Record in journal and advance the clock.
        self.journal.advance(changed_paths)
    }

    /// Apply a batch where some files were removed.
    ///
    /// Removed files are evicted from the metadata cache entirely.
    /// Modified files are downgraded. Advances the clock.
    pub fn apply_changes_with_removals(
        &self,
        changed: Vec<NormalizedPath>,
        removed: Vec<NormalizedPath>,
    ) -> Clock {
        for path in &changed {
            self.metadata.downgrade(path);
        }
        for path in &removed {
            self.metadata.remove(path);
        }

        let mut all_paths = changed;
        all_paths.extend(removed);
        self.journal.advance(all_paths)
    }

    /// Handle a watcher overflow event.
    ///
    /// Downgrades ALL metadata cache entries to Low confidence and marks
    /// the overflow in the journal so all subsequent clock queries return
    /// "changed" for clocks before the overflow.
    pub fn apply_overflow(&self) -> Clock {
        self.metadata.downgrade_all();
        self.journal.mark_overflow()
    }

    /// Re-verify all cached entries after an overflow.
    ///
    /// Stats each Low-confidence entry and promotes it back to High
    /// if the filesystem metadata still matches. Does NOT re-hash.
    ///
    /// Returns the number of entries promoted.
    pub fn rescan_entries(&self) -> usize {
        self.metadata.rescan_all()
    }

    /// Clear all cached metadata and journal state.
    ///
    /// Returns the new overflow clock (all pre-clear clocks are invalidated).
    pub fn clear(&self) -> Clock {
        self.metadata.clear();
        self.journal.clear()
    }

    /// Trim metadata entries older than `max_age`, then remove orphaned
    /// journal `last_change` entries. Returns `(metadata_removed, journal_removed)`.
    pub fn trim(&self, max_age: std::time::Duration) -> (usize, usize) {
        let meta_removed = self.metadata.trim(max_age);
        let journal_removed = self.cleanup_journal();
        (meta_removed, journal_removed)
    }

    /// Evict the `count` oldest metadata entries, then remove orphaned
    /// journal entries. Returns `(metadata_removed, journal_removed)`.
    pub fn evict_oldest(&self, count: usize) -> (usize, usize) {
        let meta_removed = self.metadata.evict_oldest(count);
        let journal_removed = self.cleanup_journal();
        (meta_removed, journal_removed)
    }

    /// Remove journal `last_change` entries not in the metadata cache.
    fn cleanup_journal(&self) -> usize {
        let live: std::collections::HashSet<NormalizedPath> =
            self.metadata.paths().into_iter().collect();
        self.journal.retain_paths(&live)
    }

    /// Register files as tracked by the journal without downgrading their
    /// confidence.
    ///
    /// This enables the zero-syscall fast path in `lookup_since` for files
    /// that the watcher hasn't reported yet. Call after scanning includes
    /// on a cache miss so headers get the fast path on subsequent hits.
    pub fn register_tracked(&self, paths: &[NormalizedPath]) {
        for path in paths {
            self.journal.register(path.clone());
        }
    }
}

impl Default for CacheSystem {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::metadata::Confidence;
    use std::fs;
    use std::thread;
    use std::time::Duration;
    use tempfile::TempDir;

    fn create_file(dir: &TempDir, name: &str, content: &str) -> NormalizedPath {
        let path = dir.path().join(name);
        fs::write(&path, content).expect("failed to create test file");
        path.into()
    }

    fn sleep_for_mtime() {
        thread::sleep(Duration::from_millis(1100));
    }

    #[test]
    fn new_cache_is_at_clock_zero() {
        let cache = CacheSystem::new();
        assert_eq!(cache.current_clock(), Clock::ZERO);
    }

    #[test]
    fn default_creates_new_cache() {
        let cache = CacheSystem::default();
        assert_eq!(cache.current_clock(), Clock::ZERO);
        assert!(cache.metadata().is_empty());
    }

    #[test]
    fn lookup_since_nonexistent_file_returns_error() {
        let cache = CacheSystem::new();
        let result = cache.lookup_since(&NormalizedPath::from("/no/such/file.c"), Clock::ZERO);
        assert!(result.is_err());
    }

    #[test]
    fn lookup_since_journal_no_change_but_no_cached_hash() {
        // Journal says "no change" but cache entry has no hash → slow path.
        let dir = TempDir::new().unwrap();
        let path = create_file(&dir, "nohash.h", "data");

        let cache = CacheSystem::new();

        // Manually put the path in the journal without populating the metadata cache hash.
        let c1 = cache.apply_changes(vec![path.clone()]);

        // lookup_since with c1 → journal says no change, but no cached hash → slow path.
        let result = cache.lookup_since(&path, c1).unwrap();
        let expected = zccache_monocrate::hash::hash_bytes(b"data");
        assert_eq!(result.hash, expected);
    }

    #[test]
    fn clock_advances_on_apply() {
        let cache = CacheSystem::new();
        assert_eq!(cache.current_clock(), Clock::ZERO);

        let c1 = cache.apply_changes(vec![]);
        assert_eq!(c1.tick(), 1);

        let c2 = cache.apply_changes(vec![]);
        assert_eq!(c2.tick(), 2);
    }

    #[test]
    fn apply_overflow_advances_clock() {
        let cache = CacheSystem::new();
        let c1 = cache.apply_changes(vec![]);
        let overflow = cache.apply_overflow();
        assert!(overflow > c1);
    }

    #[test]
    fn lookup_since_returns_current_clock() {
        let dir = TempDir::new().unwrap();
        let path = create_file(&dir, "clocked.h", "tick");

        let cache = CacheSystem::new();
        let result = cache.lookup_since(&path, Clock::ZERO).unwrap();
        // Clock should be at or after the current clock at time of lookup.
        assert!(result.clock >= Clock::ZERO);
    }

    #[test]
    fn lookup_since_zero_always_stats() {
        let dir = TempDir::new().unwrap();
        let path = create_file(&dir, "a.c", "hello");

        let cache = CacheSystem::new();
        let result = cache.lookup_since(&path, Clock::ZERO).unwrap();

        // Should have computed the hash.
        let expected = zccache_monocrate::hash::hash_bytes(b"hello");
        assert_eq!(result.hash, expected);
    }

    #[test]
    fn lookup_since_skips_stat_when_no_changes() {
        let dir = TempDir::new().unwrap();
        let path = create_file(&dir, "stable.h", "#pragma once");

        let cache = CacheSystem::new();

        // First lookup populates the cache.
        let r1 = cache.lookup_since(&path, Clock::ZERO).unwrap();

        // Record the file in the journal so it's "tracked" (not untracked → conservative true).
        let c1 = cache.apply_changes(vec![path.clone()]);

        // File hasn't changed since c1. Second lookup should use fast path.
        let r2 = cache.lookup_since(&path, c1).unwrap();
        assert_eq!(r1.hash, r2.hash);
    }

    #[test]
    fn lookup_since_stats_when_changed() {
        let dir = TempDir::new().unwrap();
        let path = create_file(&dir, "evolving.c", "v1");

        let cache = CacheSystem::new();
        let r1 = cache.lookup_since(&path, Clock::ZERO).unwrap();

        // Simulate file change: edit + watcher event.
        sleep_for_mtime();
        fs::write(&path, "v2").unwrap();
        let c2 = cache.apply_changes(vec![path.clone()]);

        // Lookup with old clock → journal says "changed" → full stat path.
        let r2 = cache.lookup_since(&path, Clock::ZERO).unwrap();
        assert_ne!(r1.hash, r2.hash);
        assert_eq!(r2.hash, zccache_monocrate::hash::hash_bytes(b"v2"));

        // Lookup with new clock → journal says "not changed" → fast path.
        let r3 = cache.lookup_since(&path, c2).unwrap();
        assert_eq!(r2.hash, r3.hash);
    }

    #[test]
    fn apply_changes_downgrades_confidence() {
        let dir = TempDir::new().unwrap();
        let path = create_file(&dir, "watched.h", "content");

        let cache = CacheSystem::new();
        cache.lookup_since(&path, Clock::ZERO).unwrap();

        // Entry should be High.
        let entry = cache.metadata().get(&path).unwrap();
        assert_eq!(entry.confidence, Confidence::High);

        // Apply changes → confidence should drop.
        cache.apply_changes(vec![path.clone()]);
        let entry = cache.metadata().get(&path).unwrap();
        assert_eq!(entry.confidence, Confidence::Low);
    }

    #[test]
    fn apply_overflow_downgrades_all() {
        let dir = TempDir::new().unwrap();
        let path_a = create_file(&dir, "a.h", "aaa");
        let path_b = create_file(&dir, "b.h", "bbb");

        let cache = CacheSystem::new();
        cache.lookup_since(&path_a, Clock::ZERO).unwrap();
        cache.lookup_since(&path_b, Clock::ZERO).unwrap();

        let c_before = cache.current_clock();
        cache.apply_overflow();

        // All entries should be Low.
        let a = cache.metadata().get(&path_a).unwrap();
        let b = cache.metadata().get(&path_b).unwrap();
        assert_eq!(a.confidence, Confidence::Low);
        assert_eq!(b.confidence, Confidence::Low);

        // Journal: queries before overflow return "changed".
        assert!(cache.journal().changed_since(&path_a, c_before));
    }

    #[test]
    fn register_tracked_enables_fast_path() {
        let dir = TempDir::new().unwrap();
        let path = create_file(&dir, "header.h", "content");

        let cache = CacheSystem::new();

        // First lookup populates metadata cache at High confidence.
        let r1 = cache.lookup_since(&path, Clock::ZERO).unwrap();

        // Register the file so journal tracks it.
        cache.register_tracked(std::slice::from_ref(&path));

        let clock = cache.current_clock();

        // Now lookup_since should use fast path (journal says no change, hash cached).
        let r2 = cache.lookup_since(&path, clock).unwrap();
        assert_eq!(r1.hash, r2.hash);
        assert_eq!(r2.hash, zccache_monocrate::hash::hash_bytes(b"content"));
    }

    #[test]
    fn concurrent_lookups_shared_cache() {
        use std::sync::Arc;

        let dir = TempDir::new().unwrap();
        let mut paths = Vec::new();
        for i in 0..10 {
            paths.push(create_file(
                &dir,
                &format!("header_{i}.h"),
                &format!("content {i}"),
            ));
        }

        let cache = Arc::new(CacheSystem::new());

        // Populate cache.
        for path in &paths {
            cache.lookup_since(path, Clock::ZERO).unwrap();
        }
        let c1 = cache.apply_changes(paths.clone());

        // Simulate make -j8: 8 threads all looking up the same 10 headers.
        let mut handles = Vec::new();
        for _ in 0..8 {
            let cache = Arc::clone(&cache);
            let paths = paths.clone();
            handles.push(std::thread::spawn(move || {
                for path in &paths {
                    let result = cache.lookup_since(path, c1).unwrap();
                    assert_eq!(result.hash, zccache_monocrate::hash::hash_file(path).unwrap(),);
                }
            }));
        }

        for h in handles {
            h.join().unwrap();
        }
    }

    #[test]
    fn rescan_entries_after_overflow() {
        let dir = TempDir::new().unwrap();
        let path_a = create_file(&dir, "a.h", "aaa");
        let path_b = create_file(&dir, "b.h", "bbb");

        let cache = CacheSystem::new();
        cache.lookup_since(&path_a, Clock::ZERO).unwrap();
        cache.lookup_since(&path_b, Clock::ZERO).unwrap();

        cache.apply_overflow();
        assert_eq!(
            cache.metadata().get(&path_a).unwrap().confidence,
            Confidence::Low
        );

        let promoted = cache.rescan_entries();
        assert_eq!(promoted, 2);
        assert_eq!(
            cache.metadata().get(&path_a).unwrap().confidence,
            Confidence::High
        );
        assert_eq!(
            cache.metadata().get(&path_b).unwrap().confidence,
            Confidence::High
        );
    }

    #[test]
    fn clear_empties_metadata_and_invalidates_clocks() {
        let dir = TempDir::new().unwrap();
        let path_a = create_file(&dir, "a.h", "aaa");
        let path_b = create_file(&dir, "b.h", "bbb");

        let cache = CacheSystem::new();
        cache.lookup_since(&path_a, Clock::ZERO).unwrap();
        cache.lookup_since(&path_b, Clock::ZERO).unwrap();
        assert_eq!(cache.metadata().len(), 2);

        let overflow_clock = cache.clear();
        assert!(cache.metadata().is_empty());
        assert!(overflow_clock > Clock::ZERO);
    }

    #[test]
    fn trim_cascades_to_journal() {
        let cache = CacheSystem::new();

        // Insert a file and track it in the journal.
        let dir = TempDir::new().unwrap();
        let path = create_file(&dir, "old.c", "content");
        cache.lookup_since(&path, Clock::ZERO).unwrap();
        cache.apply_changes(vec![path.clone()]);

        // Journal should track this file.
        assert_eq!(cache.journal().last_change_len(), 1);

        // Trim with zero max_age removes everything.
        let (meta_removed, journal_removed) = cache.trim(Duration::ZERO);
        assert!(meta_removed >= 1);
        assert_eq!(journal_removed, 1);
        assert!(cache.metadata().is_empty());
        assert_eq!(cache.journal().last_change_len(), 0);
    }

    #[test]
    fn evict_oldest_cascades() {
        let cache = CacheSystem::new();
        let dir = TempDir::new().unwrap();
        let path = create_file(&dir, "evict.c", "data");
        cache.lookup_since(&path, Clock::ZERO).unwrap();
        cache.apply_changes(vec![path.clone()]);

        let (meta_removed, journal_removed) = cache.evict_oldest(10);
        assert_eq!(meta_removed, 1);
        assert_eq!(journal_removed, 1);
        assert!(cache.metadata().is_empty());
    }

    #[test]
    fn apply_changes_with_removals() {
        let dir = TempDir::new().unwrap();
        let path_mod = create_file(&dir, "mod.c", "modified");
        let path_del = create_file(&dir, "del.c", "deleted");

        let cache = CacheSystem::new();
        cache.lookup_since(&path_mod, Clock::ZERO).unwrap();
        cache.lookup_since(&path_del, Clock::ZERO).unwrap();
        assert_eq!(cache.metadata().len(), 2);

        fs::remove_file(&path_del).unwrap();
        cache.apply_changes_with_removals(vec![path_mod], vec![path_del.clone()]);

        // Deleted file should be gone from metadata cache.
        assert!(cache.metadata().get(&path_del).is_none());
        assert_eq!(cache.metadata().len(), 1);
    }
}
