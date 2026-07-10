//! Small server helpers without a more natural home.

use super::*;

/// How many artifact-persist tasks may be in flight concurrently.
///
/// The daemon's persist path writes each cached artifact to disk via
/// `std::fs::write` inside `tokio::task::spawn_blocking`.
///
/// **Platform split (ISSUE-501, Linux Docker 2026-06-25):**
///
/// - **Windows:** Defender real-time protection serializes file writes —
///   every `std::fs::write` blocks until Defender finishes scanning the
///   file. The hardcoded default of 8 was retained because raising it
///   without other changes regressed wall-clock on this machine (see
///   `tests/persist_pool_bench.rs`).
/// - **Non-Windows (Linux/macOS):** No AV serialization. The previous
///   `8` cap throttled cold-miss waves on Linux to Windows-Defender
///   width — a 50-file fan-out only got 8 concurrent persists.
///   PR #919 sized the non-Windows default at `max(16, parallelism)`.
///   The follow-up bump (this revision) takes it to
///   `max(32, parallelism * 2)` so the persist pool stays *non-critical*
///   relative to the compile path: cold-miss persists never queue
///   behind a 50-wide cargo wave on any host with ≥ 8 cores, and
///   small hosts still get a hard floor of 32 (4× the Windows
///   Defender floor) before file-descriptor pressure becomes a concern.
///
/// The env var gives operators a lever when their workload differs —
/// e.g. cache on a network mount, or a slow AV setup that benefits
/// from more (or fewer) in-flight writes.
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
    #[cfg(windows)]
    {
        // Windows Defender serializes write-scan; raising this above 8
        // regressed wall-clock in `tests/persist_pool_bench.rs`.
        8
    }
    #[cfg(not(windows))]
    {
        // Follow-up to ISSUE-501 (Linux Docker 2026-06-25): on non-AV
        // hosts persist is non-critical work; size it generously above
        // the cold-miss wave width so it never queues. `parallelism * 2`
        // covers a 50-wide cargo cold burst on hosts with ≥ 25 cores;
        // the `max(32, …)` floor covers the same burst on smaller hosts
        // (32 simultaneous hardlinks is well under the per-process fd
        // ceiling even on default-configured Linux).
        let parallelism = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(8);
        parallelism.saturating_mul(2).max(32)
    }
}

/// Hash a file using the metadata cache (with watcher-assisted confidence).
pub(super) fn hash_file_via_cache(state: &SharedState, path: &Path) -> Option<ContentHash> {
    // Try metadata cache first (stat-verified hash)
    if let Ok(hash) = state.cache_system.metadata().lookup(path) {
        return Some(hash);
    }
    // Fall back to direct hash
    crate::hash::hash_file(path).ok()
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
    if let Some(includes) = state.dep_graph.load().get_includes(context_key) {
        for header in &includes {
            if journal.changed_since(header, since) {
                return false;
            }
        }
    }
    if let Some(externs) = state.dep_graph.load().get_rustc_externs(context_key) {
        for (_, path) in &externs {
            if journal.changed_since(path, since) {
                return false;
            }
        }
    }
    true
}

/// [`context_files_fresh`] plus the rustc env-dep gate (zccache#1021):
/// the zero-hash fast paths must also decline when a recorded
/// `env!()`/`option_env!()` value differs from the current request env —
/// env values have no mtime, so the journal can't see them change.
pub(super) fn context_files_and_env_fresh(
    state: &SharedState,
    context_key: &ContextKey,
    source_path: &Path,
    since: Clock,
    client_env: Option<&[(String, String)]>,
) -> bool {
    if !context_env_deps_fresh(state, context_key, client_env) {
        return false;
    }
    context_files_fresh(state, context_key, source_path, since)
}

/// True when every recorded env-dep value for the context matches the
/// current request env (or the context has none recorded — the common
/// case).
pub(super) fn context_env_deps_fresh(
    state: &SharedState,
    context_key: &ContextKey,
    client_env: Option<&[(String, String)]>,
) -> bool {
    let Some(deps) = state.dep_graph.load().get_rustc_env_deps(context_key) else {
        return true;
    };
    deps.iter().all(|(name, recorded_hash)| {
        let current = client_env
            .and_then(|env| env.iter().find(|(k, _)| k == name))
            .map(|(_, v)| v.as_str());
        crate::depgraph::hash_env_dep_value(current) == *recorded_hash
    })
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
    // Issue #784 phase 2d: the artifact-index blob is no longer read at
    // bind time — `bind_with_cache_dir` constructs an empty store and a
    // background `spawn_blocking` calls `load_from_disk`. If a lookup
    // races ahead of that load, fall through to the disk read on the
    // spot so this helper's contract ("DashMap miss → on-disk fallback
    // hit") still holds. Idempotent: the background loader and this
    // synchronous path can both insert the same entries; DashMap
    // inserts are last-writer-wins of equivalent values, so the live
    // store converges. We swap `artifact_store_loaded` to `true`
    // afterwards so subsequent misses (including from other request
    // handlers) skip the disk read.
    if !state
        .artifact_store_loaded
        .load(std::sync::atomic::Ordering::Acquire)
    {
        let _ = state.artifact_store.load_from_disk();
        state
            .artifact_store_loaded
            .store(true, std::sync::atomic::Ordering::Release);
    }
    let meta = state.artifact_store.get(key_hex)?;
    state
        .artifacts
        .insert(key_hex.to_string(), CachedArtifact::from_index(meta));
    state.artifacts.get_mut(key_hex)
}
