//! Memory-bounded eviction for long-running daemon.
//!
//! Estimates in-memory cache size using per-entry constants and evicts
//! entries in priority order when the total exceeds the configured budget.
//!
//! Disk artifact eviction (`evict_disk_artifacts`) is separate: it enforces
//! `max_cache_size` by removing the oldest `.meta` + data files from the
//! artifact directory.

use crate::core::NormalizedPath;
use crate::depgraph::{ContextKey, DepGraph};
use crate::fscache::CacheSystem;
use dashmap::DashMap;
use std::collections::HashMap;
use std::path::Path;
use std::time::{Duration, Instant};

use super::server::{remove_cow_blob, CachedArtifact, FastHitEntry};

/// Estimated bytes per metadata cache entry.
const METADATA_ENTRY_BYTES: usize = 400;
/// Estimated bytes per journal `last_change` entry.
const JOURNAL_ENTRY_BYTES: usize = 280;
/// Estimated bytes per depgraph file entry.
const DEPGRAPH_FILE_BYTES: usize = 600;
/// Estimated bytes per depgraph context entry.
const DEPGRAPH_CONTEXT_BYTES: usize = 2048;
/// Estimated bytes per fast-hit cache entry.
const FAST_HIT_ENTRY_BYTES: usize = 200;
/// Estimated fixed overhead per cached artifact entry (excludes payload).
const ARTIFACT_OVERHEAD_BYTES: usize = 200;
/// Max age preserved by the fast-hit cache when a budget-pressure trim
/// fires. Set small (5 s) so the trim still reclaims the bulk of the
/// cache, but recently-cached entries — including those being filled
/// in-flight when the trim races — survive. (#454)
const FAST_HIT_BUDGET_TRIM_MAX_AGE: Duration = Duration::from_secs(5);
const DISK_EVICTION_SCAN_YIELD_EVERY: usize = 512;
const DISK_EVICTION_DELETE_BATCH_SIZE: usize = 64;

/// Snapshot of estimated memory usage across all in-memory caches.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub(crate) struct MemorySnapshot {
    pub(crate) metadata_entries: usize,
    pub(crate) journal_entries: usize,
    pub(crate) depgraph_files: usize,
    pub(crate) depgraph_contexts: usize,
    pub(crate) fast_hit_entries: usize,
    pub(crate) artifact_entries: usize,
    /// Total artifact payload bytes (actual, not estimated).
    pub(crate) artifact_payload_bytes: usize,
    /// Bytes in spawn_blocking persistence tasks, not yet visible to eviction.
    pub(crate) in_flight_bytes: usize,
    /// Total estimated bytes across all subsystems.
    pub(crate) total_bytes: usize,
}

/// Compute a memory usage snapshot.
pub(crate) fn memory_snapshot(
    cache_system: &CacheSystem,
    dep_graph: &DepGraph,
    fast_hit_cache: &DashMap<ContextKey, FastHitEntry>,
    artifacts: &DashMap<String, CachedArtifact>,
    in_flight_bytes: usize,
) -> MemorySnapshot {
    let metadata_entries = cache_system.metadata().len();
    let journal_entries = cache_system.journal().last_change_len();
    let dg_stats = dep_graph.stats();
    let depgraph_files = dg_stats.file_count;
    let depgraph_contexts = dg_stats.context_count;
    let fast_hit_entries = fast_hit_cache.len();
    let artifact_entries = artifacts.len();

    let artifact_payload_bytes: usize = artifacts
        .iter()
        .map(|entry| entry.value().meta.total_size as usize)
        .sum();

    // NOTE: artifact_payload_bytes is intentionally excluded from the memory
    // budget.  Artifact payloads are persisted on disk and governed by
    // `max_cache_size` (disk GC).  Including them here caused the memory
    // eviction loop to wipe the dep graph every 30 s when artifact payload
    // exceeded the 1 GB budget — the root cause of Bug 2 (0% hit rate).
    let total_bytes = metadata_entries * METADATA_ENTRY_BYTES
        + journal_entries * JOURNAL_ENTRY_BYTES
        + depgraph_files * DEPGRAPH_FILE_BYTES
        + depgraph_contexts * DEPGRAPH_CONTEXT_BYTES
        + fast_hit_entries * FAST_HIT_ENTRY_BYTES
        + artifact_entries * ARTIFACT_OVERHEAD_BYTES
        + in_flight_bytes;

    MemorySnapshot {
        metadata_entries,
        journal_entries,
        depgraph_files,
        depgraph_contexts,
        fast_hit_entries,
        artifact_entries,
        artifact_payload_bytes,
        in_flight_bytes,
        total_bytes,
    }
}

/// Evict entries until estimated memory is at or below `budget_bytes`.
///
/// Eviction priority (cheapest to regenerate first):
/// 1. Fast-hit cache (ephemeral, regenerated on next compile hit)
/// 2. Metadata cache + journal (re-populated on next stat)
/// 3. Depgraph contexts + orphaned files (rebuilt on next compile)
///
/// Artifacts are **not** evicted here — disk GC (`evict_disk_artifacts`)
/// handles artifact lifecycle based on `max_cache_size`.
///
/// Evicts to 90% of budget to avoid thrashing.
///
/// Returns `(estimated_freed_bytes, items_removed)`.
pub(crate) fn evict_to_budget(
    budget_bytes: u64,
    cache_system: &CacheSystem,
    dep_graph: &DepGraph,
    fast_hit_cache: &DashMap<ContextKey, FastHitEntry>,
    artifacts: &DashMap<String, CachedArtifact>,
    in_flight_bytes: usize,
) -> (u64, usize) {
    let snap = memory_snapshot(
        cache_system,
        dep_graph,
        fast_hit_cache,
        artifacts,
        in_flight_bytes,
    );

    if (snap.total_bytes as u64) <= budget_bytes {
        return (0, 0);
    }

    // Target 90% of budget to avoid evicting a tiny bit every cycle.
    let target = (budget_bytes as f64 * 0.9) as u64;
    let mut to_free = snap.total_bytes as u64 - target;
    let mut total_freed: u64 = 0;
    let mut total_items: usize = 0;

    // Priority 1: fast-hit cache (cheapest to lose).
    //
    // #454: pass a small TTL rather than `Duration::ZERO`. ZERO trims any
    // entry older than "now", which during async in-flight populate can
    // wipe entries that were JUST cached — a request being filled races
    // against its own result when a budget-pressure check fires at the
    // wrong instant. Keeping the most-recent few seconds of entries costs
    // at most a handful of FAST_HIT_ENTRY_BYTES (well inside the 10 %
    // slack we already give the budget) and saves the corresponding
    // cold re-hits during build surges.
    if to_free > 0 && snap.fast_hit_entries > 0 {
        let removed = trim_fast_hit_cache_two_pass(fast_hit_cache, FAST_HIT_BUDGET_TRIM_MAX_AGE);
        let freed = (removed * FAST_HIT_ENTRY_BYTES) as u64;
        total_freed += freed;
        total_items += removed;
        to_free = to_free.saturating_sub(freed);
    }

    // Priority 2: metadata + journal.
    if to_free > 0 && !cache_system.metadata().is_empty() {
        let entries_to_evict = (to_free as usize / METADATA_ENTRY_BYTES)
            .max(1)
            .min(cache_system.metadata().len());
        let (meta_removed, journal_removed) = cache_system.evict_oldest(entries_to_evict);
        let freed =
            (meta_removed * METADATA_ENTRY_BYTES + journal_removed * JOURNAL_ENTRY_BYTES) as u64;
        total_freed += freed;
        total_items += meta_removed + journal_removed;
        to_free = to_free.saturating_sub(freed);
    }

    // Priority 3: depgraph contexts (trim all, then orphaned files cleaned up).
    if to_free > 0 {
        let dg_stats = dep_graph.stats();
        if dg_stats.context_count > 0 {
            let removed = dep_graph.trim(Duration::ZERO);
            let freed = (removed * DEPGRAPH_CONTEXT_BYTES) as u64;
            // File entries cleaned up by trim() internally.
            let files_after = dep_graph.stats().file_count;
            let files_freed = dg_stats.file_count.saturating_sub(files_after);
            let file_bytes = (files_freed * DEPGRAPH_FILE_BYTES) as u64;
            total_freed += freed + file_bytes;
            total_items += removed + files_freed;
        }
    }

    (total_freed, total_items)
}

fn trim_fast_hit_cache_two_pass(
    cache: &DashMap<ContextKey, FastHitEntry>,
    max_age: Duration,
) -> usize {
    let now = Instant::now();
    let expired: Vec<ContextKey> = cache
        .iter()
        .filter_map(|entry| {
            if now.saturating_duration_since(entry.value().cached_at) > max_age {
                Some(*entry.key())
            } else {
                None
            }
        })
        .collect();

    let mut removed = 0;
    for key in expired {
        if cache.remove(&key).is_some() {
            removed += 1;
        }
    }
    removed
}

// ── Disk artifact eviction ──────────────────────────────────────────────

/// Info about one artifact group on disk (`.meta` + data files).
struct DiskArtifact {
    key: String,
    total_size: u64,
    mtime: std::time::SystemTime,
    files: Vec<NormalizedPath>,
}

/// Evict on-disk artifacts when total disk usage exceeds `max_cache_size`.
///
/// Strategy: LRU by `.meta` file mtime (proxy for last use). Evicts oldest
/// artifacts until disk usage is at 90% of budget (same headroom strategy as
/// memory eviction).
///
/// Also removes the corresponding in-memory `DashMap` entries.
///
/// **Issue #680**: when `dep_graph` is `Some`, after the artifact eviction
/// pass completes, every depgraph context whose `artifact_key` matches an
/// evicted artifact gets its key cleared via
/// [`DepGraph::invalidate_artifact_keys`]. Without this, the depgraph keeps
/// pointing at the now-evicted artifact and the next compile reports
/// `verdict=Hit` followed by `artifact_not_found` and a wasted recompile.
/// Callers that don't have a depgraph reference (the legacy test fixtures
/// in this module) pass `None`.
///
/// Returns `(bytes_freed, artifacts_removed)`.
pub(crate) fn evict_disk_artifacts(
    artifact_dir: &Path,
    artifacts: &DashMap<String, CachedArtifact>,
    max_cache_size: u64,
    dep_graph: Option<&DepGraph>,
) -> (u64, usize) {
    let entries = match std::fs::read_dir(artifact_dir) {
        Ok(e) => e,
        Err(_) => return (0, 0),
    };

    // Group files by artifact key stem.
    let mut groups: HashMap<String, DiskArtifact> = HashMap::new();
    let mut total_disk: u64 = 0;

    for (idx, entry) in entries.flatten().enumerate() {
        if idx > 0 && idx % DISK_EVICTION_SCAN_YIELD_EVERY == 0 {
            std::thread::yield_now();
        }

        let path = entry.path();
        let Some(fname) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };

        // Derive the key stem: "abcd1234.meta" → "abcd1234", "abcd1234_0" → "abcd1234"
        let key = if let Some(stem) = fname.strip_suffix(".meta") {
            stem.to_string()
        } else if let Some(pos) = fname.rfind('_') {
            // Data file: key_hex_{index}
            fname[..pos].to_string()
        } else {
            continue;
        };

        let size = path.metadata().map(|m| m.len()).unwrap_or(0);
        total_disk += size;

        let group = groups.entry(key.clone()).or_insert_with(|| DiskArtifact {
            key,
            total_size: 0,
            mtime: std::time::SystemTime::UNIX_EPOCH,
            files: Vec::new(),
        });
        group.total_size += size;
        // Use data file mtime as LRU proxy. For legacy .meta files, also
        // use their mtime. The latest mtime across all files in the group
        // is the best estimate of last use.
        if let Ok(meta) = path.metadata() {
            let mtime = meta.modified().unwrap_or(std::time::SystemTime::UNIX_EPOCH);
            if mtime > group.mtime {
                group.mtime = mtime;
            }
        }
        group.files.push(path.into());
    }

    if total_disk <= max_cache_size {
        return (0, 0);
    }

    // Target 90% of budget to avoid evicting a tiny bit every cycle.
    let target = (max_cache_size as f64 * 0.9) as u64;
    let mut to_free = total_disk.saturating_sub(target);

    // Sort by mtime ascending (oldest first).
    let mut sorted: Vec<DiskArtifact> = groups.into_values().collect();
    sorted.sort_by_key(|a| a.mtime);

    let mut bytes_freed: u64 = 0;
    let mut artifacts_removed: usize = 0;

    // Collect artifacts that need eviction (sequential — must respect LRU order).
    let mut to_evict: Vec<DiskArtifact> = Vec::new();
    for artifact in sorted {
        if to_free == 0 {
            break;
        }
        bytes_freed += artifact.total_size;
        to_free = to_free.saturating_sub(artifact.total_size);
        artifacts_removed += 1;
        to_evict.push(artifact);
    }

    // Keep deletion pressure bounded; this runs inside one spawn_blocking
    // task, so unbounded parallel fan-out only competes with request-side I/O.
    let mut deleted_since_yield = 0;
    for artifact in &to_evict {
        for file in &artifact.files {
            let _ = remove_cow_blob(file);
            deleted_since_yield += 1;
            if deleted_since_yield >= DISK_EVICTION_DELETE_BATCH_SIZE {
                std::thread::yield_now();
                deleted_since_yield = 0;
            }
        }
    }

    // Remove from in-memory DashMap.
    for artifact in &to_evict {
        artifacts.remove(&artifact.key);
    }

    // Issue #680: invalidate any depgraph contexts pointing at the
    // artifacts we just evicted. Without this, the next compile through
    // those contexts reports Hit, the artifact_store lookup misses, and
    // the unit recompiles for no caching benefit. Build the set once and
    // pass it; the depgraph walks its DashMap inside.
    if let Some(dg) = dep_graph {
        if !to_evict.is_empty() {
            let evicted_keys: std::collections::HashSet<String> =
                to_evict.iter().map(|a| a.key.clone()).collect();
            let cleared = dg.invalidate_artifact_keys(&evicted_keys);
            if cleared > 0 {
                tracing::info!(
                    contexts_cleared = cleared,
                    artifacts_evicted = artifacts_removed,
                    "invalidated depgraph contexts pointing at disk-evicted artifacts (issue #680)"
                );
            }
        }
    }

    (bytes_freed, artifacts_removed)
}

#[cfg(test)]
mod tests {
    use super::super::server::CachedPayload;
    use super::*;
    use crate::depgraph::CompileContext;
    use crate::fscache::{Confidence, FileMetadata};
    use std::time::{Instant, SystemTime};

    fn empty_caches() -> (
        CacheSystem,
        DepGraph,
        DashMap<ContextKey, FastHitEntry>,
        DashMap<String, CachedArtifact>,
    ) {
        (
            CacheSystem::new(),
            DepGraph::new(),
            DashMap::new(),
            DashMap::new(),
        )
    }

    fn make_ctx(source: &str) -> CompileContext {
        CompileContext {
            source_file: source.into(),
            include_search: crate::depgraph::IncludeSearchPaths::default(),
            defines: Vec::new(),
            flags: Vec::new(),
            force_includes: Vec::new(),
            unknown_flags: Vec::new(),
        }
    }

    fn make_context_key(source: &str) -> ContextKey {
        make_ctx(source).context_key()
    }

    #[test]
    fn snapshot_empty() {
        let (cs, dg, fh, art) = empty_caches();
        let snap = memory_snapshot(&cs, &dg, &fh, &art, 0);
        assert_eq!(snap.total_bytes, 0);
        assert_eq!(snap.metadata_entries, 0);
        assert_eq!(snap.fast_hit_entries, 0);
        assert_eq!(snap.artifact_entries, 0);
    }

    #[test]
    fn snapshot_with_entries() {
        let (cs, dg, fh, _art) = empty_caches();

        // Add a fast-hit entry.
        fh.insert(
            make_context_key("/tmp/snap.c"),
            FastHitEntry {
                clock: crate::fscache::Clock::ZERO,
                artifact_key_hex: String::new(),
                cached_at: Instant::now(),
            },
        );

        let snap = memory_snapshot(&cs, &dg, &fh, &DashMap::new(), 0);
        assert_eq!(snap.fast_hit_entries, 1);
        assert!(snap.total_bytes >= FAST_HIT_ENTRY_BYTES);
    }

    #[test]
    fn evict_noop_under_budget() {
        let (cs, dg, fh, art) = empty_caches();
        let (freed, items) = evict_to_budget(1_073_741_824, &cs, &dg, &fh, &art, 0);
        assert_eq!(freed, 0);
        assert_eq!(items, 0);
    }

    #[test]
    fn evict_fast_hit_first() {
        let (cs, dg, fh, art) = empty_caches();

        // Add enough fast-hit entries to exceed a tiny budget. Aged past
        // FAST_HIT_BUDGET_TRIM_MAX_AGE (5 s) so the budget-pressure trim
        // can wipe them — recent entries are intentionally preserved by
        // #454; see `evict_fast_hit_preserves_recent_entries` below.
        let aged = Instant::now() - Duration::from_secs(60);
        for i in 0..100 {
            fh.insert(
                make_context_key(&format!("/tmp/fh{i}.c")),
                FastHitEntry {
                    clock: crate::fscache::Clock::ZERO,
                    artifact_key_hex: String::new(),
                    cached_at: aged,
                },
            );
        }

        let budget = 1000; // Very small budget.
        let (freed, items) = evict_to_budget(budget, &cs, &dg, &fh, &art, 0);
        assert!(freed > 0);
        assert_eq!(items, 100); // All fast-hit entries evicted.
        assert!(fh.is_empty());
    }

    #[test]
    fn evict_fast_hit_preserves_recent_entries() {
        // #454: the budget-pressure trim should keep entries cached within
        // the last FAST_HIT_BUDGET_TRIM_MAX_AGE (5 s) so a populate that
        // races with the trim isn't wiped immediately after it landed.
        let (cs, dg, fh, art) = empty_caches();

        let aged = Instant::now() - Duration::from_secs(60);
        let recent = Instant::now();
        // 50 old entries (should be wiped) + 50 fresh entries (should survive).
        for i in 0..50 {
            fh.insert(
                make_context_key(&format!("/tmp/old{i}.c")),
                FastHitEntry {
                    clock: crate::fscache::Clock::ZERO,
                    artifact_key_hex: String::new(),
                    cached_at: aged,
                },
            );
        }
        for i in 0..50 {
            fh.insert(
                make_context_key(&format!("/tmp/new{i}.c")),
                FastHitEntry {
                    clock: crate::fscache::Clock::ZERO,
                    artifact_key_hex: String::new(),
                    cached_at: recent,
                },
            );
        }

        let budget = 1000; // Tiny budget — guarantees the fast-hit trim fires.
        let (_freed, items) = evict_to_budget(budget, &cs, &dg, &fh, &art, 0);

        assert_eq!(items, 50, "only the 50 aged entries should evict");
        assert_eq!(fh.len(), 50, "the 50 recent entries must survive");
        // Confirm the survivors are the recent ones.
        for i in 0..50 {
            assert!(
                fh.contains_key(&make_context_key(&format!("/tmp/new{i}.c"))),
                "recent entry /tmp/new{i}.c should survive the budget-pressure trim",
            );
        }
    }

    #[test]
    fn evict_cascades_to_metadata() {
        let (cs, dg, fh, art) = empty_caches();

        // Add metadata entries.
        for i in 0..50 {
            cs.metadata().insert(
                NormalizedPath::from(format!("/tmp/meta{i}.c")),
                FileMetadata {
                    mtime: SystemTime::now(),
                    size: 100,
                    confidence: Confidence::High,
                    last_verified: Instant::now(),
                    content_hash: None,
                },
            );
        }
        // Track in journal.
        let paths: Vec<NormalizedPath> = (0..50)
            .map(|i| NormalizedPath::from(format!("/tmp/meta{i}.c")))
            .collect();
        cs.apply_changes(paths);

        let budget = 1000; // Very small budget.
        let (freed, items) = evict_to_budget(budget, &cs, &dg, &fh, &art, 0);
        assert!(freed > 0);
        assert!(items > 0);
        // Metadata should be reduced.
        assert!(cs.metadata().len() < 50);
    }

    #[test]
    fn evict_cascades_to_depgraph() {
        let (cs, dg, fh, art) = empty_caches();

        // Add depgraph contexts.
        for i in 0..20 {
            dg.register(make_ctx(&format!("/tmp/src{i}.c")));
        }

        // Also add metadata so metadata eviction happens first.
        for i in 0..10 {
            cs.metadata().insert(
                NormalizedPath::from(format!("/tmp/m{i}.c")),
                FileMetadata {
                    mtime: SystemTime::now(),
                    size: 100,
                    confidence: Confidence::High,
                    last_verified: Instant::now(),
                    content_hash: None,
                },
            );
        }

        let budget = 1000; // Very small budget.
        let (freed, items) = evict_to_budget(budget, &cs, &dg, &fh, &art, 0);
        assert!(freed > 0);
        assert!(items > 0);
        // Depgraph contexts should be cleared (trim(ZERO) removes all).
        assert_eq!(dg.stats().context_count, 0);
    }

    #[test]
    fn snapshot_includes_in_flight_bytes() {
        let (cs, dg, fh, art) = empty_caches();
        let snap = memory_snapshot(&cs, &dg, &fh, &art, 500_000);
        assert_eq!(snap.in_flight_bytes, 500_000);
        assert_eq!(snap.total_bytes, 500_000);
    }

    #[test]
    fn in_flight_bytes_push_over_budget_triggers_eviction() {
        let (cs, dg, fh, art) = empty_caches();

        // Add fast-hit entries worth 100 * 200 = 20_000 bytes estimated.
        // Aged past FAST_HIT_BUDGET_TRIM_MAX_AGE so the budget-pressure
        // trim is allowed to wipe them — recent entries are intentionally
        // preserved by #454.
        let aged = Instant::now() - Duration::from_secs(60);
        for i in 0..100 {
            fh.insert(
                make_context_key(&format!("/tmp/inflight{i}.c")),
                FastHitEntry {
                    clock: crate::fscache::Clock::ZERO,
                    artifact_key_hex: String::new(),
                    cached_at: aged,
                },
            );
        }

        // Budget of 100_000 — fast-hit alone (20_000) fits fine.
        let (freed, items) = evict_to_budget(100_000, &cs, &dg, &fh, &art, 0);
        assert_eq!(freed, 0);
        assert_eq!(items, 0);

        // Now add 90_000 in-flight bytes → total = 110_000 > 100_000 budget.
        let (freed, items) = evict_to_budget(100_000, &cs, &dg, &fh, &art, 90_000);
        assert!(freed > 0);
        assert!(items > 0);
        // Fast-hit entries should have been evicted to bring total under budget.
        assert!(fh.is_empty());
    }

    #[test]
    fn in_flight_bytes_alone_over_budget_evicts_nothing_when_caches_empty() {
        let (cs, dg, fh, art) = empty_caches();
        // In-flight bytes exceed budget but there's nothing to evict.
        let (freed, items) = evict_to_budget(1000, &cs, &dg, &fh, &art, 50_000);
        assert_eq!(freed, 0);
        assert_eq!(items, 0);
    }

    fn make_artifact(payload_size: usize) -> CachedArtifact {
        use crate::artifact::ArtifactIndex;
        CachedArtifact {
            meta: ArtifactIndex::new(
                vec!["test.o".to_string()],
                vec![payload_size as u64],
                Vec::new(),
                Vec::new(),
                0,
            ),
            stdout: std::sync::Arc::new(Vec::new()),
            stderr: std::sync::Arc::new(Vec::new()),
            payloads: Some(std::sync::Arc::from(vec![CachedPayload::Bytes(
                std::sync::Arc::new(vec![0u8; payload_size]),
            )])),
            last_used: Instant::now(),
        }
    }

    #[test]
    fn snapshot_excludes_artifact_payload() {
        let (cs, dg, fh, art) = empty_caches();
        // Insert an artifact with 10 MB of payload.
        art.insert("big_artifact".to_string(), make_artifact(10_000_000));
        let snap = memory_snapshot(&cs, &dg, &fh, &art, 0);
        assert_eq!(snap.artifact_entries, 1);
        assert_eq!(snap.artifact_payload_bytes, 10_000_000);
        // total_bytes should only include the per-entry overhead, NOT the payload.
        assert_eq!(snap.total_bytes, ARTIFACT_OVERHEAD_BYTES);
    }

    #[test]
    fn evict_no_longer_wipes_depgraph_with_large_artifact_payload() {
        let (cs, dg, fh, art) = empty_caches();

        // Register dep graph contexts.
        for i in 0..10 {
            dg.register(make_ctx(&format!("/tmp/depgraph{i}.c")));
        }
        assert_eq!(dg.stats().context_count, 10);

        // Insert artifacts with huge payload (simulating Bug 5: 51 GB loaded).
        for i in 0..5 {
            art.insert(format!("art_{i}"), make_artifact(500_000));
        }

        // Budget is 1 GB — artifact payload (2.5 MB) is excluded from the
        // memory budget, so total_bytes is small (overhead only) and fits
        // within the 1 GB budget.  Dep graph must NOT be wiped.
        let budget = 1_073_741_824u64; // 1 GB
        let (freed, items) = evict_to_budget(budget, &cs, &dg, &fh, &art, 0);
        assert_eq!(freed, 0);
        assert_eq!(items, 0);
        // Dep graph is intact.
        assert_eq!(dg.stats().context_count, 10);
    }

    // ── Disk eviction tests ──────────────────────────────────────────

    /// Write a fake artifact group to `dir`: `{key}.meta` + `{key}_0`.
    fn write_fake_artifact(dir: &Path, key: &str, size: usize) {
        let meta_path = dir.join(format!("{key}.meta"));
        let data_path = dir.join(format!("{key}_0"));
        std::fs::write(&meta_path, vec![0u8; 64]).unwrap();
        std::fs::write(&data_path, vec![0u8; size]).unwrap();
    }

    #[test]
    fn disk_eviction_noop_under_budget() {
        let dir = tempfile::tempdir().unwrap();
        let artifacts: DashMap<String, CachedArtifact> = DashMap::new();
        write_fake_artifact(dir.path(), "aaa", 100);
        let (freed, removed) = evict_disk_artifacts(dir.path(), &artifacts, 1_000_000, None);
        assert_eq!(freed, 0);
        assert_eq!(removed, 0);
    }

    #[test]
    fn disk_eviction_removes_oldest_first() {
        let dir = tempfile::tempdir().unwrap();
        let artifacts: DashMap<String, CachedArtifact> = DashMap::new();

        // Create 3 artifacts with staggered mtimes.
        write_fake_artifact(dir.path(), "old", 5000);
        // Sleep briefly to ensure different mtimes.
        std::thread::sleep(Duration::from_millis(50));
        write_fake_artifact(dir.path(), "mid", 5000);
        std::thread::sleep(Duration::from_millis(50));
        write_fake_artifact(dir.path(), "new", 5000);

        // Total: 3 * (5000 + 64) = 15192 bytes.
        // Budget: 10000 bytes → need to evict oldest.
        let (freed, removed) = evict_disk_artifacts(dir.path(), &artifacts, 10_000, None);
        assert!(freed > 0);
        assert!(removed >= 1);
        // "new" should still exist.
        assert!(dir.path().join("new.meta").exists());
        // "old" should be gone.
        assert!(!dir.path().join("old.meta").exists());
    }

    #[test]
    fn disk_eviction_removes_dashmap_entries() {
        let dir = tempfile::tempdir().unwrap();
        let artifacts: DashMap<String, CachedArtifact> = DashMap::new();

        write_fake_artifact(dir.path(), "key1", 5000);
        artifacts.insert("key1".to_string(), make_artifact(5000));

        // Budget: 0 → must evict everything.
        let (freed, removed) = evict_disk_artifacts(dir.path(), &artifacts, 0, None);
        assert!(freed > 0);
        assert_eq!(removed, 1);
        assert!(artifacts.is_empty());
    }

    #[cfg(windows)]
    #[test]
    fn disk_eviction_removes_readonly_blob() {
        let dir = tempfile::tempdir().unwrap();
        let artifacts: DashMap<String, CachedArtifact> = DashMap::new();
        write_fake_artifact(dir.path(), "readonly", 5000);
        let data = dir.path().join("readonly_0");
        let mut permissions = std::fs::metadata(&data).unwrap().permissions();
        permissions.set_readonly(true);
        std::fs::set_permissions(&data, permissions).unwrap();

        let (_, removed) = evict_disk_artifacts(dir.path(), &artifacts, 0, None);

        assert_eq!(removed, 1);
        assert!(!data.exists());
    }

    /// Regression test for <https://github.com/zackees/zccache/issues/680>.
    ///
    /// Pre-fix: `evict_disk_artifacts` removed the on-disk artifact and the
    /// in-memory `DashMap` entry, but did NOT touch the depgraph contexts
    /// that pointed at the now-evicted artifact key. The next compile
    /// through those contexts surfaced `verdict=Hit` followed by
    /// `artifact_not_found` and a wasted recompile (~15% real hit rate on
    /// the dogfood reproducer instead of ~99%).
    ///
    /// Post-fix: passing `Some(&dep_graph)` invalidates the matching
    /// context `artifact_key`s. Full integration coverage (registering a
    /// depgraph context with a known artifact_key and asserting it gets
    /// cleared) lives next to the `invalidate_artifact_keys` method in
    /// `depgraph::graph::tests` — that's where the depgraph's internal
    /// test seams already exist. This site-local test asserts only the
    /// signature wiring: passing `None` is a no-op (back-compat with the
    /// fixture callers above), passing `Some` reaches the depgraph.
    #[test]
    fn disk_eviction_signature_accepts_optional_depgraph() {
        let dir = tempfile::tempdir().unwrap();
        let artifacts: DashMap<String, CachedArtifact> = DashMap::new();
        let dg = DepGraph::new();

        write_fake_artifact(dir.path(), "k", 5000);

        // Some(&dg) compiles and is a no-op when the depgraph has no
        // matching contexts — the bridge is wired, the production
        // invalidation path is covered by the depgraph-level unit test
        // (`invalidate_artifact_keys_clears_only_matching` in
        // `depgraph::graph::tests`).
        let (freed, removed) = evict_disk_artifacts(dir.path(), &artifacts, 0, Some(&dg));
        assert!(freed > 0);
        assert_eq!(removed, 1);
        // Empty depgraph → no contexts to invalidate, no panic.
        assert_eq!(dg.stats().context_count, 0);
    }

    #[test]
    fn disk_eviction_targets_90_percent() {
        let dir = tempfile::tempdir().unwrap();
        let artifacts: DashMap<String, CachedArtifact> = DashMap::new();

        // Create 10 artifacts of ~1000 bytes each = ~10640 total.
        for i in 0..10 {
            write_fake_artifact(dir.path(), &format!("k{i:02}"), 1000);
            std::thread::sleep(Duration::from_millis(20));
        }

        // Budget: 10000. 90% target = 9000. Need to free ~1640 bytes.
        // That's ~2 artifacts (each ~1064 bytes).
        let (freed, removed) = evict_disk_artifacts(dir.path(), &artifacts, 10_000, None);
        assert!(freed > 0);
        // Should remove just enough, not all.
        assert!(removed >= 1);
        assert!(removed <= 5);
    }
}
