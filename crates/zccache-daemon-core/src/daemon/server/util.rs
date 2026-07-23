//! Small server helpers without a more natural home.

use super::*;

/// Enter an artifact-publication critical section unless shutdown has closed
/// the daemon to new publishers.
///
/// The shutdown flag must be checked *after* acquiring the fair Tokio lock:
/// a request may have started before shutdown but queued behind shutdown's
/// writer. In that case it must not publish after the writer drains and the
/// index task has stopped.
pub(super) async fn begin_artifact_publication(
    state: &SharedState,
) -> Option<tokio::sync::OwnedRwLockReadGuard<()>> {
    let guard = Arc::clone(&state.artifact_publication).read_owned().await;
    if state.shutdown_requested.load(Ordering::Acquire) {
        None
    } else {
        Some(guard)
    }
}

/// Blocking-worker counterpart to [`begin_artifact_publication`].
pub(super) fn begin_artifact_publication_blocking(
    state: &SharedState,
) -> Option<tokio::sync::RwLockReadGuard<'_, ()>> {
    let guard = state.artifact_publication.blocking_read();
    if state.shutdown_requested.load(Ordering::Acquire) {
        None
    } else {
        Some(guard)
    }
}

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

/// Hash a normalized path without repeating path normalization in the cache.
pub(super) fn hash_normalized_file_via_cache(
    state: &SharedState,
    path: &NormalizedPath,
) -> Option<ContentHash> {
    if let Ok(hash) = state.cache_system.metadata().lookup_normalized(path) {
        return Some(hash);
    }
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
    let lookup_path = path_for_cache_lookup(path);
    cache_system
        .lookup_since(&NormalizedPath::new(lookup_path.as_ref()), clock)
        .map(|r| r.hash)
        .map_err(|e| format!("{}: {e}", path.display()))
}

fn path_for_cache_lookup(path: &Path) -> std::borrow::Cow<'_, Path> {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::{OsStrExt, OsStringExt};

        if !path.as_os_str().as_encoded_bytes().starts_with(br"\\?\") {
            return std::borrow::Cow::Borrowed(path);
        }
        let encoded: Vec<u16> = path.as_os_str().encode_wide().collect();

        let ascii_eq = |value: u16, uppercase: u8| {
            value == u16::from(uppercase) || value == u16::from(uppercase.to_ascii_lowercase())
        };
        let is_unc = encoded.len() >= 8
            && ascii_eq(encoded[4], b'U')
            && ascii_eq(encoded[5], b'N')
            && ascii_eq(encoded[6], b'C')
            && encoded[7] == b'\\' as u16;
        let normalized = if is_unc {
            let mut result = vec![b'\\' as u16, b'\\' as u16];
            result.extend_from_slice(&encoded[8..]);
            Some(result)
        } else if encoded.len() >= 6
            && encoded[4] <= 0x7f
            && (encoded[4] as u8).is_ascii_alphabetic()
            && encoded[5] == b':' as u16
        {
            Some(encoded[4..].to_vec())
        } else {
            None
        };
        if let Some(normalized) = normalized {
            return std::borrow::Cow::Owned(PathBuf::from(std::ffi::OsString::from_wide(
                &normalized,
            )));
        }
    }
    std::borrow::Cow::Borrowed(path)
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
/// # Guard lifetime
///
/// The returned value owns an allocation-free artifact clone and a read lease
/// on the artifact-publication barrier. The DashMap guard therefore exists
/// only long enough to clone one `Arc`; filesystem payload discovery and
/// access checkpointing never hold a shard lock. Disk maintenance and Clear
/// take the barrier's write side, so they cannot delete files or order an
/// index removal ahead of an already-started hit.
pub(super) struct CachedArtifactLookup {
    cached: CachedArtifact,
    _publication_guard: tokio::sync::OwnedRwLockReadGuard<()>,
}

impl CachedArtifactLookup {
    fn new(
        cached: CachedArtifact,
        publication_guard: tokio::sync::OwnedRwLockReadGuard<()>,
    ) -> Self {
        Self {
            cached,
            _publication_guard: publication_guard,
        }
    }
}

impl std::ops::Deref for CachedArtifactLookup {
    type Target = CachedArtifact;

    fn deref(&self) -> &Self::Target {
        &self.cached
    }
}

pub(super) fn lookup_artifact_with_disk_fallback(
    state: &SharedState,
    key_hex: &str,
) -> Option<CachedArtifactLookup> {
    // Maintenance/Clear already owns or is queued for the write side. Treat
    // this as a cache miss instead of making a request wait behind destructive
    // work. Tokio's fair RwLock also rejects new readers once a writer queues.
    let publication_guard = Arc::clone(&state.artifact_publication)
        .try_read_owned()
        .ok()?;
    if let Some(entry) = state.artifacts.get(key_hex) {
        return Some(CachedArtifactLookup::new(
            entry.value().clone(),
            publication_guard,
        ));
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
    let cached = CachedArtifact::from_index(meta);
    let cached = state
        .artifacts
        .entry(key_hex.to_string())
        .or_insert(cached)
        .clone();
    Some(CachedArtifactLookup::new(cached, publication_guard))
}

/// Refresh an artifact's maintenance-visible access time at most once per
/// hour. The in-memory timestamp protects hot entries immediately; the
/// throttled index update preserves that access across daemon restarts without
/// adding a clock read or WAL write to every cache hit (issue #1148).
pub(super) fn record_artifact_access(
    state: &SharedState,
    key_hex: &str,
    artifact: &CachedArtifact,
    now: Instant,
) {
    if let Some(meta) = artifact.record_access(now) {
        let _ = state
            .index_writer_tx
            .send(IndexWriterCommand::Insert(key_hex.to_string(), meta));
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use std::os::windows::ffi::{OsStrExt, OsStringExt};

    #[test]
    fn cache_lookup_path_strips_windows_verbatim_prefix() {
        let input = Path::new(r"\\?\C:\workspace\clippy.toml");
        assert_eq!(
            path_for_cache_lookup(input).as_ref(),
            Path::new(r"C:\workspace\clippy.toml")
        );

        let unc = Path::new(r"\\?\UNC\server\share\clippy.toml");
        assert_eq!(
            path_for_cache_lookup(unc).as_ref(),
            Path::new(r"\\server\share\clippy.toml")
        );

        let encoded = [
            b'\\' as u16,
            b'\\' as u16,
            b'?' as u16,
            b'\\' as u16,
            b'C' as u16,
            b':' as u16,
            b'\\' as u16,
            0xd800,
        ];
        let non_unicode = PathBuf::from(std::ffi::OsString::from_wide(&encoded));
        assert_eq!(
            path_for_cache_lookup(&non_unicode)
                .as_os_str()
                .encode_wide()
                .collect::<Vec<_>>(),
            encoded[4..]
        );
    }
}
