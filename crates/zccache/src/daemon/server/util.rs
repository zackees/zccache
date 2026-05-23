//! Small server helpers without a more natural home.

use super::*;

/// How many artifact-persist tasks may be in flight concurrently.
///
/// The daemon's persist path writes each cached artifact to disk via
/// `std::fs::write` inside `tokio::task::spawn_blocking`. On Windows with
/// Defender real-time protection, every write blocks until Defender finishes
/// scanning the file. The hardcoded default of 8 was retained because raising
/// it without other changes regressed wall-clock on this machine
/// (see `tests/persist_pool_bench.rs`). The env var gives operators a lever
/// when their workload differs — e.g. cache on a network mount, or a slow
/// AV setup that benefits from more in-flight writes.
///
/// Override with `ZCCACHE_STORE_WORKERS=<N>` (must be ≥ 1, clamped to 1024).
pub(super) fn persist_workers_default() -> usize {
    if let Ok(v) = std::env::var("ZCCACHE_STORE_WORKERS") {
        if let Ok(n) = v.parse::<usize>() {
            if n >= 1 {
                return n.min(1024);
            }
        }
    }
    8
}

/// Hash a file using the metadata cache (with watcher-assisted confidence).
pub(super) fn hash_file_via_cache(state: &SharedState, path: &Path) -> Option<ContentHash> {
    // Try metadata cache first (stat-verified hash)
    if let Ok(hash) = state.cache_system.metadata().lookup(path) {
        return Some(hash);
    }
    // Fall back to direct hash
    zccache::hash::hash_file(path).ok()
}

/// Hash a file using the CacheSystem's metadata cache.
///
/// This stat-verifies the file, hashes if needed (with TOCTOU protection),
/// and caches the result. The file watcher proactively downgrades confidence
/// on changes, ensuring stale hashes are re-computed.
///
/// `clock` should be snapped once at the start of each compile request so all
/// files in a single compilation see a consistent journal clock.
pub(super) fn hash_file(
    cache_system: &CacheSystem,
    path: &Path,
    clock: Clock,
) -> Result<ContentHash, String> {
    debug_assert!(
        !path.to_string_lossy().starts_with(r"\\?\"),
        "path must not have \\\\?\\ prefix: {}",
        path.display()
    );
    cache_system
        .lookup_since(&NormalizedPath::new(path), clock)
        .map(|r| r.hash)
        .map_err(|e| format!("{}: {e}", path.display()))
}

/// Check if all files in a context's dependency list are unchanged since
/// the given clock. Uses per-file journal tracking instead of global clock
/// comparison, so output file changes (like .o writes) don't invalidate
/// fast-hit entries for unrelated source contexts.
pub(super) fn context_files_fresh(
    state: &SharedState,
    context_key: &ContextKey,
    source_path: &Path,
    since: Clock,
) -> bool {
    let journal = state.cache_system.journal();
    if journal.changed_since(&source_path.into(), since) {
        return false;
    }
    if let Some(includes) = state.dep_graph.get_includes(context_key) {
        for header in &includes {
            if journal.changed_since(header, since) {
                return false;
            }
        }
    }
    true
}

/// Look up an artifact by key, falling through to the on-disk
/// [`ArtifactStore`] when the in-memory [`SharedState::artifacts`] DashMap
/// has not yet been hydrated.
///
/// # Why the fallthrough is required
///
/// Daemon startup spawns a background task that copies every entry from
/// `state.artifact_store` (loaded synchronously by `ArtifactStore::open`)
/// into `state.artifacts`. The daemon begins accepting IPC requests
/// immediately, before that background task finishes. Without this
/// helper, the warm-after-restore window (`soldr load` → first compile)
/// reports MISS on every lookup until the DashMap catches up — measured
/// at 0/115 hits on the medium fixture's `cold-tar-untar-warm`
/// scenario (perf-cluster run 26255457227).
///
/// The DashMap is a cache *of* the on-disk store; the on-disk store is
/// the source of truth for artifact existence. Lookups now:
/// 1. Hit the in-memory DashMap (fast path; populated by stores +
///    background load).
/// 2. On miss, consult the in-memory hashmap that backs
///    [`ArtifactStore::open`] (also fast — already hydrated from
///    `index.bin` at daemon bind time).
/// 3. On disk-store hit, hydrate the DashMap so subsequent lookups
///    skip the fallback entirely.
///
/// # Why two `get_mut` calls
///
/// DashMap forbids holding a shard lock (`get_mut` returns a guard
/// holding it) across an `insert` on the same map — that would
/// deadlock. We release the first guard's `None` arm, do the
/// disk-store lookup + insert, then take a fresh `get_mut` to hand
/// back. The `insert` + re-`get_mut` is on the cold path (DashMap
/// miss + disk-store hit), so the extra hash is dwarfed by the
/// hardlink/write work that follows.
pub(super) fn lookup_artifact_with_disk_fallback<'a>(
    state: &'a SharedState,
    key_hex: &str,
) -> Option<dashmap::mapref::one::RefMut<'a, String, CachedArtifact>> {
    if let Some(entry) = state.artifacts.get_mut(key_hex) {
        return Some(entry);
    }
    let meta = state.artifact_store.get(key_hex)?;
    state
        .artifacts
        .insert(key_hex.to_string(), CachedArtifact::from_index(meta));
    state.artifacts.get_mut(key_hex)
}
