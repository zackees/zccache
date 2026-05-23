//! Daemon server — accepts IPC connections and handles requests.

use dashmap::DashMap;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{Mutex, Notify};
use zccache_artifact::{ArtifactIndex, ArtifactStore};
use zccache_core::NormalizedPath;
use zccache_depgraph::{
    CompileContext, ContextKey, DepGraph, DepfileStrategy, SessionId, SessionManager,
    SystemIncludeCache, UserDepFlags,
};
use zccache_fscache::{CacheSystem, Clock};
use zccache_hash::ContentHash;
use zccache_ipc::{IpcConnection, IpcListener};
use zccache_protocol::{ArtifactData, ArtifactOutput, ArtifactPayload, Request, Response};
use zccache_watcher::{NotifyWatcher, SettleBuffer, SettledEvent};

use crate::compile_journal::{extract_outcome, CompileJournal, JournalContext, JournalEntry};
use crate::fingerprint::FingerprintManager;
use crate::process::CompilePriority;
use crate::stats::{HitPhases, MissPhases, PhaseProfiler, StatsCollector};

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
fn persist_workers_default() -> usize {
    if let Ok(v) = std::env::var("ZCCACHE_STORE_WORKERS") {
        if let Ok(n) = v.parse::<usize>() {
            if n >= 1 {
                return n.min(1024);
            }
        }
    }
    8
}

/// RAII guard that decrements `in_flight_bytes` on drop, even during panic unwind.
/// Prevents permanent counter inflation if a `spawn_blocking` task panics.
struct InFlightGuard {
    state: Arc<SharedState>,
    size: usize,
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        self.state
            .in_flight_bytes
            .fetch_sub(self.size, Ordering::Relaxed);
    }
}

/// Cached result of a verified cache hit, enabling zero-hash fast path.
///
/// When the journal clock hasn't advanced since the last verified hit for a
/// context, we can skip all stat/hash/depgraph work and jump straight to
/// artifact lookup.
pub(crate) struct FastHitEntry {
    pub(crate) clock: Clock,
    pub(crate) artifact_key_hex: String,
    pub(crate) cached_at: std::time::Instant,
}

/// Maximum age for fast-hit cache entries. Matches the High→Medium confidence
/// decay in the metadata cache. Without watcher events, entries expire and
/// fall through to the stat-verify slow path. Set to 60s because the watcher
/// + journal provide real invalidation — this timer is just a safety net.
const FAST_HIT_MAX_AGE: std::time::Duration = std::time::Duration::from_secs(60);
const EPHEMERAL_CACHE_MAX_AGE: std::time::Duration = std::time::Duration::from_secs(300);
const REQUEST_CACHE_MAX_ENTRIES: usize = 4096;
const RSP_CACHE_MAX_ENTRIES: usize = 1024;
const RUST_MISS_PROFILE_ENV: &str = "ZCCACHE_PROFILE_RUST_MISS";
const WORKTREE_ROOT_ENV: &str = "ZCCACHE_WORKTREE_ROOT";
const PATH_REMAP_ENV: &str = "ZCCACHE_PATH_REMAP";
const REQUEST_ROOT_MARKER: &str = "$ZCCACHE_WORKTREE_ROOT";
const LINK_PATH_REMAP_AUTO_KEY_FLAG: &str = "zccache:path-remap=auto";
const LINK_PATH_REMAP_ROOT_SPECIFIC_FLAG: &str = "zccache:path-remap=root-specific";

#[derive(Clone)]
struct RspDependency {
    path: NormalizedPath,
    hash: ContentHash,
}

#[derive(Clone)]
struct RspCacheEntry {
    expanded: Vec<String>,
    dependencies: Vec<RspDependency>,
    cached_at: std::time::Instant,
}

#[derive(Clone)]
struct CompilerHashEntry {
    mtime: std::time::SystemTime,
    size: u64,
    hash: ContentHash,
}

#[derive(Default)]
struct CompilerHashCache {
    entries: DashMap<NormalizedPath, CompilerHashEntry>,
}

impl CompilerHashCache {
    fn new() -> Self {
        Self {
            entries: DashMap::new(),
        }
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.entries.len()
    }

    fn get_or_hash(&self, path: &Path) -> Option<ContentHash> {
        self.get_or_hash_with(path, |path| zccache_hash::hash_file(path).ok())
    }

    fn get_or_hash_with<F>(&self, path: &Path, hasher: F) -> Option<ContentHash>
    where
        F: FnOnce(&Path) -> Option<ContentHash>,
    {
        let metadata = std::fs::metadata(path).ok()?;
        let mtime = metadata.modified().ok()?;
        let size = metadata.len();
        let key = NormalizedPath::new(path);

        if let Some(entry) = self.entries.get(&key) {
            if entry.mtime == mtime && entry.size == size {
                return Some(entry.hash);
            }
        }

        let hash = hasher(path)?;
        let post_metadata = std::fs::metadata(path).ok()?;
        let post_mtime = post_metadata.modified().ok()?;
        let post_size = post_metadata.len();
        if post_mtime != mtime || post_size != size {
            return Some(hash);
        }

        self.entries
            .insert(key, CompilerHashEntry { mtime, size, hash });
        Some(hash)
    }
}

#[derive(Clone)]
pub(crate) enum CachedPayload {
    /// Payload bytes already resident in memory.
    Bytes(Arc<Vec<u8>>),
    /// Payload bytes are available in a cache file.
    File(NormalizedPath),
}

#[derive(Clone)]
/// Cached compilation artifact with lazy payload loading.
///
/// Metadata (output names, sizes, stdout, stderr, exit code) is always in
/// memory after startup. Output payloads are either already in memory or are
/// represented by cache files so hits can hardlink without eager reads.
pub(crate) struct CachedArtifact {
    pub(crate) meta: ArtifactIndex,
    /// Arc-wrapped stdout/stderr for cheap IPC response clones.
    pub(crate) stdout: Arc<Vec<u8>>,
    pub(crate) stderr: Arc<Vec<u8>>,
    /// Lazily-resolved output payloads. `None` = not yet checked on disk.
    /// Arc-wrapped so cache-hit clones are O(1) refcount bumps.
    pub(crate) payloads: Option<Arc<[CachedPayload]>>,
    /// When this artifact was last used (inserted or returned as a hit).
    pub(crate) last_used: std::time::Instant,
}

impl CachedArtifact {
    /// Create from a freshly compiled `ArtifactData`. Payload mapping is
    /// 1:1 between the protocol `ArtifactPayload` enum and the internal
    /// `CachedPayload` enum.
    fn from_artifact_data(artifact: &ArtifactData) -> Self {
        let meta = ArtifactIndex::new(
            artifact.outputs.iter().map(|o| o.name.clone()).collect(),
            artifact
                .outputs
                .iter()
                .map(|o| o.payload.size_bytes())
                .collect(),
            Arc::clone(&artifact.stdout),
            Arc::clone(&artifact.stderr),
            artifact.exit_code,
        );
        Self {
            meta,
            stdout: Arc::clone(&artifact.stdout),
            stderr: Arc::clone(&artifact.stderr),
            payloads: Some(Arc::from(
                artifact
                    .outputs
                    .iter()
                    .map(|o| match &o.payload {
                        ArtifactPayload::Bytes(b) => CachedPayload::Bytes(Arc::clone(b)),
                        ArtifactPayload::Path(p) => CachedPayload::File(p.clone()),
                    })
                    .collect::<Vec<_>>(),
            )),
            last_used: std::time::Instant::now(),
        }
    }

    /// Create from index metadata and already-created payload files.
    fn from_file_payloads(meta: ArtifactIndex, payloads: Vec<NormalizedPath>) -> Self {
        let stdout = Arc::clone(&meta.stdout);
        let stderr = Arc::clone(&meta.stderr);
        Self {
            meta,
            stdout,
            stderr,
            payloads: Some(Arc::from(
                payloads
                    .into_iter()
                    .map(CachedPayload::File)
                    .collect::<Vec<_>>(),
            )),
            last_used: std::time::Instant::now(),
        }
    }

    /// Create from index metadata (lazy payloads not loaded yet).
    fn from_index(meta: ArtifactIndex) -> Self {
        let stdout = Arc::clone(&meta.stdout);
        let stderr = Arc::clone(&meta.stderr);
        Self {
            meta,
            stdout,
            stderr,
            payloads: None,
            last_used: std::time::Instant::now(),
        }
    }
}

/// Load output payloads from `{key}_0`, `{key}_1`, ... files on disk.
///
/// Returns the payload slice, or `None` if any data file is missing
/// (indicating corruption or eviction — caller should treat as cache miss).
fn ensure_payloads<'a>(
    cached: &'a mut CachedArtifact,
    artifact_dir: &Path,
    key_hex: &str,
) -> Option<&'a [CachedPayload]> {
    if cached.payloads.is_none() {
        let mut payloads = Vec::with_capacity(cached.meta.output_names.len());
        for i in 0..cached.meta.output_names.len() {
            let path = artifact_dir.join(format!("{key_hex}_{i}"));
            if let Ok(meta) = std::fs::metadata(&path) {
                if meta.is_file()
                    && cached
                        .meta
                        .output_sizes
                        .get(i)
                        .is_none_or(|expected| *expected == meta.len())
                {
                    payloads.push(CachedPayload::File(path.into()));
                    continue;
                }
            }
            // Fallback: artifact may be stored in a `.pack` file (pack mode).
            let bytes = try_load_packed_payload(artifact_dir, key_hex, i)?;
            if let Some(expected) = cached.meta.output_sizes.get(i) {
                if *expected != bytes.len() as u64 {
                    return None;
                }
            }
            payloads.push(CachedPayload::Bytes(Arc::new(bytes)));
        }
        cached.payloads = Some(Arc::from(payloads));
    }
    cached.payloads.as_deref()
}

/// Migrate legacy `.meta` files to the in-memory artifact index.
/// Called once on first startup after upgrade.
fn migrate_meta_files(
    artifact_dir: &Path,
    artifacts: &DashMap<String, CachedArtifact>,
    store: &ArtifactStore,
) -> usize {
    use rayon::prelude::*;

    // Collect .meta file paths first.
    let meta_paths: Vec<NormalizedPath> = match std::fs::read_dir(artifact_dir) {
        Ok(entries) => entries
            .flatten()
            .map(|e| e.path().into())
            .filter(|p: &NormalizedPath| p.extension().and_then(|e| e.to_str()) == Some("meta"))
            .collect(),
        Err(_) => return 0,
    };

    if meta_paths.is_empty() {
        return 0;
    }

    // Parallel phase: read, deserialize, and write data files.
    // Each .meta file is fully independent for I/O.
    let migrated: Vec<(String, CachedArtifact, NormalizedPath)> = meta_paths
        .par_iter()
        .filter_map(|path| {
            let data = std::fs::read(path).ok()?;
            let artifact = bincode::deserialize::<ArtifactData>(&data).ok()?;
            let stem: String = path
                .file_stem()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned();

            // Write {key}_0, {key}_1, ... data files if missing.
            // Legacy `.meta` files only ever stored inline bytes, so we
            // only handle the `Bytes` variant here. Any `Path` variant
            // would be a forward-compat artefact that legacy migration
            // can safely skip — caller treats failures as non-cacheable.
            for (i, out) in artifact.outputs.iter().enumerate() {
                let data_path = artifact_dir.join(format!("{stem}_{i}"));
                if !data_path.exists() {
                    if let Some(bytes) = out.payload.as_bytes() {
                        std::fs::write(&data_path, bytes.as_slice()).ok();
                    }
                }
            }

            let cached = CachedArtifact::from_artifact_data(&artifact);
            Some((stem, cached, path.clone()))
        })
        .collect();

    // Sequential phase: insert into the in-memory store and DashMap,
    // then delete the legacy .meta files.
    let count = migrated.len();
    for (stem, cached, meta_path) in migrated {
        store.insert(&stem, &cached.meta);
        artifacts.insert(stem, cached);
        std::fs::remove_file(&meta_path).ok();
    }

    if count > 0 {
        tracing::info!(count, "migrated legacy .meta files to artifact index");
    }
    count
}

/// Shared state accessible by all connection handlers.
struct SharedState {
    sessions: SessionManager,
    system_includes: Mutex<SystemIncludeCache>,
    /// Dependency graph: tracks include relationships and cache verdicts.
    dep_graph: DepGraph,
    /// In-memory artifact cache: artifact_key_hex → artifact data.
    artifacts: DashMap<String, CachedArtifact>,
    /// Metadata cache + change journal. The watcher feeds file-change events
    /// into this, which downgrades confidence so `lookup()` re-hashes on
    /// next access. Without the watcher, stat-verify on every `lookup()` is
    /// the fallback (correct but slower).
    cache_system: CacheSystem,
    /// File watcher for proactive metadata invalidation.
    watcher: Mutex<Option<NotifyWatcher>>,
    /// Directories currently being watched (avoid duplicate watches).
    watched_dirs: Mutex<HashSet<NormalizedPath>>,
    /// Shutdown signal — shared so request handlers can trigger shutdown.
    shutdown: Arc<Notify>,
    /// Epoch seconds of last client activity (for idle timeout).
    last_activity: AtomicU64,
    /// Daemon start time (epoch seconds).
    start_time: u64,
    /// Global stats collector.
    stats: StatsCollector,
    /// Phase-level profiler for hot-path breakdown.
    profiler: PhaseProfiler,
    /// On-disk artifact cache for hardlink optimization on cache hits.
    artifact_dir: NormalizedPath,
    /// On-disk path for the persisted [`MetadataCache`] snapshot.
    ///
    /// Written on flush (`Clear`) and shutdown (`Shutdown`); read at
    /// daemon startup so warm-side daemons spawned after `soldr load`
    /// start with their fast path already populated instead of an
    /// empty `DashMap`. See `zccache_fscache::persistence`.
    metadata_path: NormalizedPath,
    /// Temporary directory for injected depfiles.
    depfile_tmpdir: NormalizedPath,
    /// Ultra-fast hit cache: context_key → (clock, artifact_key_hex, timestamp).
    /// When the journal clock hasn't advanced since the last verified hit,
    /// we skip all stat/hash/depgraph work and jump straight to artifact lookup.
    fast_hit_cache: DashMap<ContextKey, FastHitEntry>,
    /// Whether the file watcher is active. Fast-hit cache is only used when
    /// the watcher is running, since we rely on it for change detection.
    watcher_active: AtomicBool,
    /// Response file expansion cache keyed by canonical root path.
    /// Each entry carries the transitive response-file hashes required to
    /// validate freshness before reusing the cached expansion.
    rsp_cache: DashMap<NormalizedPath, RspCacheEntry>,
    /// Request-level fast path cache: hash(compiler, args, cwd) → pre-computed context.
    /// When the same compile request is seen again and the fast-hit cache still
    /// holds a valid entry, this allows skipping ALL heavy work: system include
    /// discovery, watch_directories, response file expansion, arg parsing,
    /// context building, and dep_graph registration.
    request_cache: DashMap<ContentHash, RequestCacheEntry>,
    /// Session-level worktree-root cache resolved once at SessionStart.
    session_worktree_roots: DashMap<SessionId, SessionWorktreeRoot>,
    /// Cross-root request-cache validation: (request fingerprint, root) -> last
    /// verified artifact and journal clock. This lets repeated sibling hits
    /// validate with journal checks instead of re-hashing every input.
    request_validation_cache: DashMap<RequestValidationKey, RequestValidationEntry>,
    /// Compiler executable hash cache keyed by compiler path.
    compiler_hash_cache: CompilerHashCache,
    /// Pre-filter for watch_directories: raw (non-canonicalized) paths we've
    /// already processed. Avoids expensive canonicalize() syscalls (~1-5ms each
    /// on Windows) for directories that are already being watched.
    watched_raw_dirs: DashMap<NormalizedPath, ()>,
    /// PCH source registry: pch_output_path → source_header_path.
    /// When a PCH generation succeeds, we record the mapping so that
    /// consuming compilations can hash the source header instead of the
    /// non-deterministic PCH binary.
    pch_source_map: DashMap<NormalizedPath, NormalizedPath>,
    /// JSONL compile journal for build replay.
    journal: CompileJournal,
    /// Bytes currently in spawn_blocking persistence tasks, invisible to eviction.
    in_flight_bytes: AtomicUsize,
    /// Limits concurrent disk persistence tasks to prevent memory pileup
    /// when disk I/O is slow and compilation requests are fast.
    persist_semaphore: Arc<tokio::sync::Semaphore>,
    /// In-memory artifact index (bincode blob-backed) for fast startup and
    /// persistence. Hot-path reads and writes go through `state.artifacts`;
    /// this store holds the same data and snapshots it to disk periodically.
    ///
    /// Arc-wrapped so the background index-writer task (see `index_writer_tx`)
    /// can hold its own clone for batched `insert` calls without contending
    /// with the request-handler path.
    artifact_store: Arc<ArtifactStore>,
    /// Sender to the background index-writer task. Persist call-sites push
    /// `(key_hex, ArtifactIndex)` pairs here and return immediately; the
    /// writer task drains the channel and flushes to the on-disk blob in
    /// batches.
    ///
    /// Decouples the artifact-persist semaphore (which gates concurrent disk
    /// writes) from the periodic index snapshot, so a slow flush no longer
    /// holds a persist permit while other artifacts wait. See
    /// `tests/persist_pool_bench.rs` for the data motivating this split.
    index_writer_tx: tokio::sync::mpsc::UnboundedSender<(String, ArtifactIndex)>,
    /// Notify the index-writer to drain its WAL and exit on graceful shutdown.
    /// Without this, the writer would only see the channel close after every
    /// `Arc<SharedState>` ref (including those held by spawned persist tasks)
    /// drops — which can race with runtime abort and lose unflushed entries.
    index_writer_shutdown: Arc<Notify>,
    /// Whether the background artifact loading has completed.
    artifacts_loaded: AtomicBool,
    /// Fingerprint manager: tracks per-watch dirty state for `zccache fp` commands.
    fingerprint: FingerprintManager,
    /// Whether the in-memory dep graph is backed by a persisted snapshot.
    ///
    /// Set to `true` when the graph is loaded from disk on startup (via
    /// `set_dep_graph`) or when a periodic/shutdown save completes
    /// successfully. Surfaced in `DaemonStatus.dep_graph_persisted` so the
    /// CLI can distinguish "persisted graph" from "first-run, not yet flushed"
    /// without inferring it from the on-disk file size.
    dep_graph_persisted: AtomicBool,
    /// Optional load-time warning to mirror into every session log.
    ///
    /// Populated by `set_depgraph_load_warning` when the daemon's startup load
    /// of the persisted depgraph fell back to a cold session (version
    /// mismatch, corrupt header, or unexpected I/O error). The string is
    /// emitted once per session into the per-session log (`last-session.log`)
    /// at `SessionStart` time so the cold fallback is never silent. Issue #320.
    depgraph_load_warning: Mutex<Option<String>>,
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
fn lookup_artifact_with_disk_fallback<'a>(
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

/// Pre-computed compile request data for the request-level fast path.
struct RequestCacheEntry {
    context_key: ContextKey,
    root: Option<NormalizedPath>,
    source_path: CachedRequestPath,
    output_path: CachedRequestPath,
    input_paths: Vec<CachedRequestPath>,
    cross_root_shareable: bool,
    cached_at: std::time::Instant,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct RequestValidationKey {
    request_fp: ContentHash,
    root: NormalizedPath,
}

struct RequestValidationEntry {
    artifact_key_hex: String,
    clock: Clock,
    cached_at: std::time::Instant,
}

struct SessionWorktreeRoot {
    working_dir: NormalizedPath,
    root: Option<NormalizedPath>,
}

#[derive(Clone)]
enum CachedRequestPath {
    RootRelative(NormalizedPath),
    Absolute(NormalizedPath),
}

impl CachedRequestPath {
    fn capture(path: &Path, key_root: Option<&Path>) -> Self {
        if let Some(root) = key_root {
            if let Ok(relative) = path.strip_prefix(root) {
                return Self::RootRelative(NormalizedPath::new(relative));
            }
        }
        Self::Absolute(NormalizedPath::new(path))
    }

    fn resolve(&self, key_root: Option<&Path>) -> NormalizedPath {
        match (self, key_root) {
            (Self::RootRelative(relative), Some(root)) => root.join(relative).into(),
            (Self::RootRelative(relative), None) | (Self::Absolute(relative), _) => {
                relative.clone()
            }
        }
    }

    fn is_root_relative(&self) -> bool {
        matches!(self, Self::RootRelative(_))
    }
}

/// The daemon server that listens for IPC connections.
pub struct DaemonServer {
    listener: IpcListener,
    shutdown: Arc<Notify>,
    state: Arc<SharedState>,
    /// Receiver for the background index-writer task. Taken in `run()`.
    index_writer_rx: Option<tokio::sync::mpsc::UnboundedReceiver<(String, ArtifactIndex)>>,
}

/// Remove fast-hit cache entries older than `max_age`. Returns entries removed.
pub(crate) fn trim_fast_hit_cache(
    cache: &DashMap<ContextKey, FastHitEntry>,
    max_age: Duration,
) -> usize {
    trim_fast_hit_cache_at(cache, max_age, Instant::now())
}

fn cache_age_at(now: Instant, cached_at: Instant) -> Duration {
    now.saturating_duration_since(cached_at)
}

fn cache_entry_expired_at(now: Instant, cached_at: Instant, max_age: Duration) -> bool {
    cache_age_at(now, cached_at) > max_age
}

fn cache_entry_fresh_at(now: Instant, cached_at: Instant, max_age: Duration) -> bool {
    cache_age_at(now, cached_at) < max_age
}

fn trim_fast_hit_cache_at(
    cache: &DashMap<ContextKey, FastHitEntry>,
    max_age: Duration,
    now: Instant,
) -> usize {
    let mut removed = 0;
    cache.retain(|_, entry| {
        if cache_entry_expired_at(now, entry.cached_at, max_age) {
            removed += 1;
            false
        } else {
            true
        }
    });
    removed
}

fn trim_request_cache(cache: &DashMap<ContentHash, RequestCacheEntry>, max_age: Duration) -> usize {
    trim_request_cache_at(cache, max_age, Instant::now())
}

fn trim_request_validation_cache(
    cache: &DashMap<RequestValidationKey, RequestValidationEntry>,
    max_age: Duration,
) -> usize {
    trim_request_validation_cache_at(cache, max_age, Instant::now())
}

fn trim_request_cache_at(
    cache: &DashMap<ContentHash, RequestCacheEntry>,
    max_age: Duration,
    now: Instant,
) -> usize {
    let mut removed = 0;
    cache.retain(|_, entry| {
        if cache_entry_expired_at(now, entry.cached_at, max_age) {
            removed += 1;
            false
        } else {
            true
        }
    });
    if cache.len() > REQUEST_CACHE_MAX_ENTRIES {
        let remaining = cache.len();
        cache.clear();
        removed += remaining;
    }
    removed
}

fn trim_request_validation_cache_at(
    cache: &DashMap<RequestValidationKey, RequestValidationEntry>,
    max_age: Duration,
    now: Instant,
) -> usize {
    let mut removed = 0;
    cache.retain(|_, entry| {
        if cache_entry_expired_at(now, entry.cached_at, max_age) {
            removed += 1;
            false
        } else {
            true
        }
    });
    if cache.len() > REQUEST_CACHE_MAX_ENTRIES {
        let remaining = cache.len();
        cache.clear();
        removed += remaining;
    }
    removed
}

fn trim_rsp_cache(cache: &DashMap<NormalizedPath, RspCacheEntry>, max_age: Duration) -> usize {
    trim_rsp_cache_at(cache, max_age, Instant::now())
}

fn trim_rsp_cache_at(
    cache: &DashMap<NormalizedPath, RspCacheEntry>,
    max_age: Duration,
    now: Instant,
) -> usize {
    let mut removed = 0;
    cache.retain(|_, entry| {
        if cache_entry_expired_at(now, entry.cached_at, max_age) {
            removed += 1;
            false
        } else {
            true
        }
    });
    if cache.len() > RSP_CACHE_MAX_ENTRIES {
        let remaining = cache.len();
        cache.clear();
        removed += remaining;
    }
    removed
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Monotonic counter ensuring each `DaemonServer` instance gets unique
/// artifact and depfile directories, even within the same process.
static SERVER_INSTANCE: AtomicU64 = AtomicU64::new(0);
static ARTIFACT_PERSIST_TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

impl DaemonServer {
    /// Create a new daemon server bound to the given endpoint, using the
    /// configured cache directory (resolved via [`zccache_core::config::default_cache_dir`]).
    ///
    /// Production callers should use this. Tests that need to isolate their
    /// cache directory must use [`Self::bind_with_cache_dir`] instead — this
    /// reads `ZCCACHE_CACHE_DIR` from a process-global env, which races when
    /// multiple tests run in parallel.
    pub fn bind(endpoint: &str) -> Result<Self, zccache_ipc::IpcError> {
        Self::bind_with_cache_dir(endpoint, &zccache_core::config::default_cache_dir())
    }

    /// Create a new daemon server bound to the given endpoint, rooted at an
    /// explicit cache directory. Bypasses the `ZCCACHE_CACHE_DIR` env var so
    /// parallel tests can each operate in isolation.
    pub fn bind_with_cache_dir(
        endpoint: &str,
        cache_dir: &zccache_core::NormalizedPath,
    ) -> Result<Self, zccache_ipc::IpcError> {
        let listener = IpcListener::bind(endpoint)?;
        let shutdown = Arc::new(Notify::new());
        let now = now_secs();
        let instance = SERVER_INSTANCE.fetch_add(1, Ordering::Relaxed);
        let artifact_dir = zccache_core::config::artifacts_dir_from_cache_dir(cache_dir);
        std::fs::create_dir_all(&artifact_dir).ok();

        // Artifact loading is deferred to a background task in run() so the
        // daemon starts accepting connections immediately (Bug 6 fix).
        let artifacts: DashMap<String, CachedArtifact> = DashMap::new();

        // Open the bincode-backed artifact index for fast startup + persistence.
        let index_path = zccache_core::config::index_path_from_cache_dir(cache_dir);
        let artifact_store = ArtifactStore::open(&index_path).map_err(|e| {
            zccache_ipc::IpcError::Io(std::io::Error::other(format!(
                "failed to open artifact index at {}: {e}",
                index_path.display()
            )))
        })?;
        let artifact_store = Arc::new(artifact_store);

        let (index_writer_tx, index_writer_rx) =
            tokio::sync::mpsc::unbounded_channel::<(String, ArtifactIndex)>();
        let index_writer_shutdown = Arc::new(Notify::new());

        // Try to restore the metadata cache from disk. A wrong-version /
        // corrupt snapshot falls back to an empty cache (the
        // `MetadataCache::lookup` stat-verify safety net still guards
        // correctness on every subsequent lookup).
        let metadata_path = zccache_core::config::metadata_path_from_cache_dir(cache_dir);
        let cache_system =
            match zccache_fscache::MetadataCache::load_from_disk(metadata_path.as_path()) {
                Ok(metadata) => {
                    let loaded = metadata.len();
                    if loaded > 0 {
                        tracing::info!(
                            loaded,
                            path = %metadata_path.display(),
                            "metadata cache restored from disk"
                        );
                    }
                    CacheSystem::with_metadata(metadata)
                }
                Err(e) => {
                    tracing::warn!(
                        path = %metadata_path.display(),
                        "failed to load metadata cache, starting empty: {e}"
                    );
                    CacheSystem::new()
                }
            };

        Ok(Self {
            listener,
            shutdown: Arc::clone(&shutdown),
            index_writer_rx: Some(index_writer_rx),
            state: Arc::new(SharedState {
                sessions: SessionManager::new(std::time::Duration::from_secs(300)),
                system_includes: Mutex::new(SystemIncludeCache::new()),
                dep_graph: DepGraph::new(),
                artifacts,
                cache_system,
                watcher: Mutex::new(None),
                watched_dirs: Mutex::new(HashSet::new()),
                shutdown,
                last_activity: AtomicU64::new(now),
                start_time: now,
                stats: StatsCollector::new(),
                profiler: PhaseProfiler::new(),
                artifact_dir,
                metadata_path,
                depfile_tmpdir: {
                    let dir = zccache_core::config::depfile_dir_from_cache_dir(cache_dir)
                        .join(format!("{}-{instance}", std::process::id()));
                    std::fs::create_dir_all(&dir).ok();
                    dir
                },
                fast_hit_cache: DashMap::new(),
                watcher_active: AtomicBool::new(false),
                rsp_cache: DashMap::new(),
                request_cache: DashMap::new(),
                session_worktree_roots: DashMap::new(),
                request_validation_cache: DashMap::new(),
                compiler_hash_cache: CompilerHashCache::new(),
                watched_raw_dirs: DashMap::new(),
                pch_source_map: DashMap::new(),
                journal: CompileJournal::new(zccache_core::config::log_dir_from_cache_dir(
                    cache_dir,
                )),
                in_flight_bytes: AtomicUsize::new(0),
                persist_semaphore: Arc::new(tokio::sync::Semaphore::new(persist_workers_default())),
                artifact_store,
                index_writer_tx,
                index_writer_shutdown,
                artifacts_loaded: AtomicBool::new(false),
                fingerprint: FingerprintManager::new(),
                dep_graph_persisted: AtomicBool::new(false),
                depgraph_load_warning: Mutex::new(None),
            }),
        })
    }

    /// Get a handle to signal shutdown.
    #[must_use]
    pub fn shutdown_handle(&self) -> Arc<Notify> {
        Arc::clone(&self.shutdown)
    }

    /// Replace the dependency graph with a pre-loaded one.
    ///
    /// Must be called before `run()` (while this is the only Arc holder).
    /// Marks the graph as persisted because it was restored from disk.
    pub fn set_dep_graph(&mut self, graph: zccache_depgraph::DepGraph) {
        let state =
            Arc::get_mut(&mut self.state).expect("set_dep_graph must be called before run()");
        state.dep_graph = graph;
        state.dep_graph_persisted.store(true, Ordering::Release);
    }

    /// Record a load-time depgraph warning to mirror into per-session logs.
    ///
    /// Called by the daemon's startup path after [`zccache_depgraph::classify_load`]
    /// returns a non-`Loaded` outcome that warrants surfacing (version
    /// mismatch, corruption, I/O error). The warning is appended to each
    /// session's log file at `SessionStart` so a cold fallback caused by a
    /// stale or corrupt `depgraph.bin` is visible to operators. Issue #320.
    ///
    /// Must be called before `run()` (while this is the only `Arc` holder).
    pub fn set_depgraph_load_warning(&mut self, warning: String) {
        let state = Arc::get_mut(&mut self.state)
            .expect("set_depgraph_load_warning must be called before run()");
        *state.depgraph_load_warning.get_mut() = Some(warning);
    }

    /// Get a snapshot of the phase profiler (for benchmarks).
    #[must_use]
    pub fn profile_snapshot(&self) -> crate::stats::ProfileSnapshot {
        self.state.profiler.snapshot()
    }

    /// Test-only seam: exercise the DashMap → on-disk-`ArtifactStore`
    /// fallback used by every artifact-lookup site.
    ///
    /// Returns `true` if the key is found either in the in-memory
    /// `artifacts` DashMap or in the on-disk artifact store (in which
    /// case the DashMap is hydrated as a side-effect). Lets perf tests
    /// assert that warm-after-restore lookups hit the on-disk store
    /// without spinning up an IPC server + real compile.
    #[doc(hidden)]
    #[must_use]
    pub fn test_lookup_artifact(&self, key_hex: &str) -> bool {
        lookup_artifact_with_disk_fallback(&self.state, key_hex).is_some()
    }

    /// Test-only seam: report whether the background artifact-load
    /// task has finished hydrating `state.artifacts`. Used by
    /// `perf_artifact_fallback_test.rs` to assert that the fallback
    /// path is the one being exercised (not the post-load fast path).
    #[doc(hidden)]
    #[must_use]
    pub fn test_artifacts_loaded(&self) -> bool {
        self.state.artifacts_loaded.load(Ordering::Acquire)
    }

    /// Test-only seam: report the number of entries currently in the
    /// in-memory `state.artifacts` DashMap. Lets the perf test assert
    /// that a fresh bind (before `run()`) starts with an empty
    /// DashMap, proving that any subsequent hit comes from the
    /// on-disk fallback path.
    #[doc(hidden)]
    #[must_use]
    pub fn test_artifacts_len(&self) -> usize {
        self.state.artifacts.len()
    }

    /// Run the server, accepting connections until shutdown is signaled.
    ///
    /// `idle_timeout_secs`: if non-zero, the daemon shuts down after this many
    /// seconds with no client activity. Pass 0 to disable.
    pub async fn run(&mut self, idle_timeout_secs: u64) -> Result<(), zccache_ipc::IpcError> {
        tracing::info!(
            persist_workers = self.state.persist_semaphore.available_permits(),
            "daemon server running"
        );

        // Background index-writer task: in-memory WAL with timer-driven
        // flushing. See `run_index_writer` for the design rationale.
        let mut index_writer_handle: Option<tokio::task::JoinHandle<()>> = None;
        if let Some(rx) = self.index_writer_rx.take() {
            let store = Arc::clone(&self.state.artifact_store);
            let shutdown = Arc::clone(&self.state.index_writer_shutdown);
            index_writer_handle = Some(tokio::spawn(run_index_writer(rx, store, shutdown)));
        }

        let cache_dir = zccache_core::config::default_cache_dir();
        let temp_root = std::env::temp_dir();

        // Clean up legacy log backup directory (Bug 7).
        {
            let legacy_logs = cache_dir.join("logs.bak");
            if legacy_logs.is_dir() {
                match std::fs::remove_dir_all(&legacy_logs) {
                    Ok(()) => tracing::info!("removed legacy logs.bak directory"),
                    Err(e) => tracing::warn!(
                        path = %legacy_logs.display(),
                        "failed to remove legacy logs.bak: {e}"
                    ),
                }
            }
            // Also remove stale daemon.lock.bak if present.
            let legacy_lock = cache_dir.join("daemon.lock.bak");
            let _ = std::fs::remove_file(&legacy_lock);
        }

        // Remove legacy temp-root state from older builds before starting the daemon.
        {
            let cleaned = zccache_core::config::cleanup_legacy_temp_root_state(
                &temp_root,
                &cache_dir,
                zccache_ipc::is_process_alive,
            );
            if cleaned > 0 {
                tracing::info!(cleaned, "cleaned legacy temp-root zccache state");
            }
        }

        // Clean up stale depfile directories from dead daemon instances.
        {
            let cleaned =
                zccache_core::config::cleanup_stale_depfile_dirs(zccache_ipc::is_process_alive);
            if cleaned > 0 {
                tracing::info!(cleaned, "cleaned stale depfile directories");
            }
        }

        self.start_watcher_pipeline().await;

        // Start idle watchdog if timeout is configured.
        if idle_timeout_secs > 0 {
            let state = Arc::clone(&self.state);
            let timeout = idle_timeout_secs;
            tokio::spawn(async move {
                loop {
                    tokio::time::sleep(std::time::Duration::from_secs(60)).await;
                    let last = state.last_activity.load(Ordering::Relaxed);
                    let idle = now_secs().saturating_sub(last);
                    if idle >= timeout {
                        tracing::info!(idle_secs = idle, "idle timeout — shutting down");
                        // Persist a "died-idle" lifecycle event so operators
                        // can see why the daemon exited. Pair this with the
                        // "spawn" entry to reconstruct daemon lifetime from
                        // the lifecycle log alone — tracing stderr is NUL'd.
                        crate::lifecycle::write_event(
                            crate::lifecycle::EVENT_DIED_IDLE,
                            serde_json::json!({
                                "reason": crate::lifecycle::REASON_IDLE_TIMEOUT,
                                "idle_secs": idle,
                                "idle_timeout_secs": timeout,
                            }),
                        );
                        state.shutdown.notify_one();
                        break;
                    }
                }
            });
        }

        // Start background artifact loading (non-blocking so daemon responds
        // immediately — Bug 6 fix).
        {
            let state = Arc::clone(&self.state);
            let state2 = Arc::clone(&self.state);
            tokio::spawn(async move {
                let artifact_dir = state.artifact_dir.clone();
                let artifacts = state.artifacts.clone();
                let state_ref = Arc::clone(&state);
                let loaded = tokio::task::spawn_blocking(move || {
                    // Load the in-memory index that `ArtifactStore::open` already
                    // hydrated from the on-disk blob.
                    let entries = state_ref.artifact_store.load_all();
                    if !entries.is_empty() {
                        let count = entries.len();
                        for (key, meta) in entries {
                            artifacts.insert(key, CachedArtifact::from_index(meta));
                        }
                        count
                    } else {
                        // Migration: legacy `.meta` files predate the redb index
                        // and the current bincode blob; populate the live store
                        // from them so the first session after upgrade still has
                        // its warm cache.
                        migrate_meta_files(&artifact_dir, &artifacts, &state_ref.artifact_store)
                    }
                })
                .await
                .unwrap_or(0);
                if loaded > 0 {
                    tracing::info!(loaded, "background artifact loading complete");
                }
                state2.artifacts_loaded.store(true, Ordering::Release);
            });
        }

        // Start memory eviction background task.
        {
            let state = Arc::clone(&self.state);
            let budget = zccache_core::config::Config::default().max_memory_bytes;
            let interval_secs = zccache_core::config::Config::default().eviction_interval_secs;
            tokio::spawn(async move {
                loop {
                    tokio::time::sleep(std::time::Duration::from_secs(interval_secs)).await;
                    let req_removed =
                        trim_request_cache(&state.request_cache, EPHEMERAL_CACHE_MAX_AGE);
                    let req_validation_removed = trim_request_validation_cache(
                        &state.request_validation_cache,
                        EPHEMERAL_CACHE_MAX_AGE,
                    );
                    let rsp_removed = trim_rsp_cache(&state.rsp_cache, EPHEMERAL_CACHE_MAX_AGE);
                    if req_removed > 0 || req_validation_removed > 0 || rsp_removed > 0 {
                        tracing::debug!(
                            request_cache_removed = req_removed,
                            request_validation_cache_removed = req_validation_removed,
                            rsp_cache_removed = rsp_removed,
                            "trimmed ephemeral daemon caches"
                        );
                    }
                    let (freed, items) = crate::eviction::evict_to_budget(
                        budget,
                        &state.cache_system,
                        &state.dep_graph,
                        &state.fast_hit_cache,
                        &state.artifacts,
                        state.in_flight_bytes.load(Ordering::Relaxed),
                    );
                    if items > 0 {
                        tracing::info!(
                            freed_bytes = freed,
                            items_removed = items,
                            "memory eviction"
                        );
                    }
                }
            });
        }

        // Start disk artifact GC background task.
        {
            let state = Arc::clone(&self.state);
            let max_cache_size = zccache_core::config::Config::default().max_cache_size;
            let interval_secs = zccache_core::config::Config::default().disk_gc_interval_secs;
            tokio::spawn(async move {
                // Run once immediately at startup to reclaim excess disk from Bug 5.
                {
                    let dir = state.artifact_dir.clone();
                    let artifacts = state.artifacts.clone();
                    let result = tokio::task::spawn_blocking(move || {
                        crate::eviction::evict_disk_artifacts(&dir, &artifacts, max_cache_size)
                    })
                    .await;
                    if let Ok((freed, removed)) = result {
                        if removed > 0 {
                            tracing::info!(
                                freed_bytes = freed,
                                artifacts_removed = removed,
                                "initial disk GC"
                            );
                        }
                    }
                }
                loop {
                    tokio::time::sleep(std::time::Duration::from_secs(interval_secs)).await;
                    let dir = state.artifact_dir.clone();
                    let artifacts = state.artifacts.clone();
                    let result = tokio::task::spawn_blocking(move || {
                        crate::eviction::evict_disk_artifacts(&dir, &artifacts, max_cache_size)
                    })
                    .await;
                    if let Ok((freed, removed)) = result {
                        if removed > 0 {
                            tracing::info!(
                                freed_bytes = freed,
                                artifacts_removed = removed,
                                "disk GC"
                            );
                        }
                    }
                }
            });
        }

        // Start periodic depgraph save task (every 5 minutes).
        {
            let state = Arc::clone(&self.state);
            tokio::spawn(async move {
                loop {
                    tokio::time::sleep(std::time::Duration::from_secs(300)).await;
                    let path = zccache_depgraph::depgraph_file_path();
                    if let Some(parent) = path.parent() {
                        std::fs::create_dir_all(parent).ok();
                    }
                    match zccache_depgraph::save_to_file(&state.dep_graph, &path) {
                        Ok(()) => {
                            state.dep_graph_persisted.store(true, Ordering::Release);
                            tracing::debug!("periodic depgraph save");
                        }
                        Err(e) => tracing::warn!("periodic depgraph save failed: {e}"),
                    }
                }
            });
        }

        loop {
            tokio::select! {
                result = self.listener.accept() => {
                    let conn = match result {
                        Ok(c) => c,
                        Err(e) => {
                            tracing::error!("accept failed, continuing: {e}");
                            continue;
                        }
                    };
                    let state = Arc::clone(&self.state);
                    tokio::spawn(async move {
                        if let Err(e) = handle_connection(conn, state).await {
                            tracing::warn!("connection error: {e}");
                        }
                    });
                }
                () = self.shutdown.notified() => {
                    tracing::info!("daemon server shutting down");
                    // Drop the watcher to stop the OS thread and close channels.
                    // The settle buffer and consumer tasks will exit when their
                    // input channels close.
                    *self.state.watcher.lock().await = None;

                    // Signal the index-writer to drain its WAL to disk, then
                    // wait briefly for it. Without this, unflushed entries are
                    // lost if the runtime aborts before the next interval tick.
                    self.state.index_writer_shutdown.notify_waiters();
                    if let Some(h) = index_writer_handle.take() {
                        let _ = tokio::time::timeout(
                            std::time::Duration::from_secs(2),
                            h,
                        )
                        .await;
                    }

                    // Critical: the WAL drain above only persists entries that
                    // went through `index_writer_tx`. The compile-success path
                    // at server.rs:6122 (and friends) inserts DIRECTLY into
                    // `artifact_store` without sending to the WAL, and
                    // `flush_wal_to_disk` early-returns on an empty WAL —
                    // so those direct-inserts never reach disk on a
                    // WAL-only-empty shutdown. Reproduced locally: a fresh
                    // medium-fixture build wrote 271 MB of CAS payloads
                    // but no index.bin, leaving the warm-side daemon (and
                    // every other `soldr load` consumer) with an empty index
                    // even though all artifacts are on disk.
                    //
                    // Force a final `store.flush()` here so the in-memory
                    // DashMap snapshot lands on disk regardless of WAL state.
                    // spawn_blocking keeps the synchronous I/O off the
                    // runtime; the await is bounded by the same 2s pattern
                    // as the WAL drain above.
                    let store = Arc::clone(&self.state.artifact_store);
                    let entries = store.len();
                    let flush_start = std::time::Instant::now();
                    let res = tokio::task::spawn_blocking(move || store.flush()).await;
                    match res {
                        Ok(Ok(())) => tracing::info!(
                            entries,
                            elapsed_ms = flush_start.elapsed().as_millis() as u64,
                            "artifact store final flush complete"
                        ),
                        Ok(Err(e)) => tracing::warn!(
                            entries,
                            "artifact store final flush failed: {e}"
                        ),
                        Err(e) => tracing::warn!(
                            entries,
                            "artifact store final flush task join error: {e}"
                        ),
                    }

                    // Save depgraph to disk before exiting.
                    let start = std::time::Instant::now();
                    let path = zccache_depgraph::depgraph_file_path();
                    if let Some(parent) = path.parent() {
                        std::fs::create_dir_all(parent).ok();
                    }
                    let (cold_ctxs, warm_ctxs, stale_ctxs) =
                        self.state.dep_graph.state_breakdown();
                    let ctxs_with_key = self.state.dep_graph.contexts_with_artifact_key();
                    match zccache_depgraph::save_to_file(&self.state.dep_graph, &path) {
                        Ok(()) => {
                            self.state
                                .dep_graph_persisted
                                .store(true, Ordering::Release);
                            // State breakdown lets a future warm-side daemon
                            // explain its cold_skip miss rate: if cold_ctxs
                            // is high relative to warm_ctxs, the warm side
                            // will take the cold_skip branch for those keys
                            // and never consult the artifact_store.
                            tracing::info!(
                                elapsed_ms = start.elapsed().as_millis() as u64,
                                cold = cold_ctxs,
                                warm = warm_ctxs,
                                stale = stale_ctxs,
                                with_artifact_key = ctxs_with_key,
                                "depgraph saved"
                            );
                        }
                        Err(e) => tracing::warn!("depgraph save failed: {e}"),
                    }

                    // Persist the in-memory MetadataCache so the next
                    // daemon (in particular the warm side of soldr
                    // save/load) starts with its fast path populated.
                    // Failure here is a perf regression, not a
                    // correctness bug — log and move on so shutdown
                    // never hangs on disk I/O.
                    let meta_start = std::time::Instant::now();
                    let metadata_entries = self.state.cache_system.metadata().len();
                    match self
                        .state
                        .cache_system
                        .metadata()
                        .save_to_disk(self.state.metadata_path.as_path())
                    {
                        Ok(()) => {
                            if metadata_entries > 0 {
                                tracing::info!(
                                    entries = metadata_entries,
                                    elapsed_ms = meta_start.elapsed().as_millis() as u64,
                                    "metadata cache persisted"
                                );
                            }
                        }
                        Err(e) => tracing::warn!(
                            path = %self.state.metadata_path.display(),
                            "metadata cache save failed: {e}"
                        ),
                    }

                    // Clean up our own depfile temp directory.
                    let _ = std::fs::remove_dir_all(&self.state.depfile_tmpdir);

                    return Ok(());
                }
            }
        }
    }

    /// Initialize the file watcher pipeline:
    /// `NotifyWatcher (OS thread) → SettleBuffer (tokio task) → CacheSystem consumer (tokio task)`
    async fn start_watcher_pipeline(&self) {
        let ignore = Arc::new(zccache_watcher::IgnoreFilter::default());
        let (watcher, raw_rx) = match NotifyWatcher::new(ignore) {
            Ok(w) => w,
            Err(e) => {
                tracing::warn!("failed to start file watcher: {e} — running without watcher");
                return;
            }
        };

        *self.state.watcher.lock().await = Some(watcher);
        self.state.watcher_active.store(true, Ordering::Release);

        // Settle buffer: coalesces raw events into batches after a quiet period.
        let (settled_tx, mut settled_rx) = tokio::sync::mpsc::unbounded_channel();
        let settle = SettleBuffer::default_window();
        tokio::spawn(async move {
            settle.run(raw_rx, settled_tx).await;
        });

        // Consumer: feeds settled events into CacheSystem for metadata invalidation.
        let state = Arc::clone(&self.state);
        tokio::spawn(async move {
            while let Some(event) = settled_rx.recv().await {
                match event {
                    SettledEvent::Batch { changed, removed } => {
                        let count = changed.len() + removed.len();
                        if count > 0 {
                            tracing::debug!(
                                changed = changed.len(),
                                removed = removed.len(),
                                "watcher batch applied"
                            );
                            // On Windows, notify reports paths with \\?\
                            // extended-length prefix but the rest of the
                            // codebase uses plain paths. Strip the prefix
                            // so journal/metadata lookups match.
                            #[cfg(windows)]
                            let (changed, removed) = {
                                let strip = |paths: Vec<NormalizedPath>| -> Vec<NormalizedPath> {
                                    paths
                                        .into_iter()
                                        .map(|p| {
                                            let s = p.to_string_lossy();
                                            if let Some(stripped) = s.strip_prefix(r"\\?\") {
                                                stripped.into()
                                            } else {
                                                p
                                            }
                                        })
                                        .collect()
                                };
                                (strip(changed), strip(removed))
                            };
                            #[cfg(debug_assertions)]
                            for p in changed.iter().chain(removed.iter()) {
                                debug_assert!(
                                    !p.to_string_lossy().starts_with(r"\\?\"),
                                    "watcher path must not have \\\\?\\ prefix: {}",
                                    p.display()
                                );
                            }
                            state.fingerprint.on_batch(&changed, &removed);
                            state
                                .cache_system
                                .apply_changes_with_removals(changed, removed);
                        }
                    }
                    SettledEvent::Overflow => {
                        tracing::warn!("watcher overflow — downgrading all metadata");
                        state.cache_system.apply_overflow();
                    }
                }
            }
            tracing::debug!("watcher consumer task exiting");
        });

        tracing::info!("file watcher pipeline started");
    }
}

/// Watch a directory for file changes, if not already watched.
async fn watch_directory(state: &SharedState, dir: &Path) {
    watch_directories(state, &[dir.into()]).await;
}

/// Watch multiple directories in a single batch, acquiring locks once.
///
/// Canonicalizes all paths up front, deduplicates against already-watched set,
/// then registers all new watches in one lock acquisition.
async fn watch_directories(state: &SharedState, dirs: &[NormalizedPath]) {
    if dirs.is_empty() {
        return;
    }

    // Pre-filter: skip dirs we've already processed (by raw path).
    // This avoids expensive canonicalize() syscalls (~1-5ms each on Windows)
    // for directories that are already being watched.
    let new_raw: Vec<&NormalizedPath> = dirs
        .iter()
        .filter(|d| !state.watched_raw_dirs.contains_key(*d))
        .collect();
    if new_raw.is_empty() {
        return;
    }

    // Canonicalize only new paths (filesystem work, no lock needed).
    // On Windows, canonicalize() produces \\?\ extended-length paths which
    // don't match the paths reported by notify's ReadDirectoryChangesW.
    // Strip the prefix so watched paths match event paths.
    let canonical: Vec<NormalizedPath> = new_raw
        .iter()
        .filter_map(|dir| match dir.canonicalize() {
            Ok(p) => {
                #[cfg(windows)]
                {
                    let s = p.to_string_lossy();
                    if let Some(stripped) = s.strip_prefix(r"\\?\") {
                        Some(stripped.into())
                    } else {
                        Some(p.into())
                    }
                }
                #[cfg(not(windows))]
                {
                    Some(p.into())
                }
            }
            Err(e) => {
                tracing::debug!("cannot canonicalize {}: {e}", dir.display());
                None
            }
        })
        .collect();

    // Mark raw paths as processed (even if canonicalize failed) so we don't
    // retry them on every subsequent call.
    for d in &new_raw {
        state.watched_raw_dirs.insert((*d).clone(), ());
    }

    if canonical.is_empty() {
        return;
    }

    // Single lock acquisition: filter already-watched and register new ones.
    // Each directory here is the exact parent of a source/header file from
    // depfile scanning — no need to walk children or parents.
    let mut watched = state.watched_dirs.lock().await;
    let new_dirs: Vec<NormalizedPath> = canonical
        .into_iter()
        .filter(|p| !watched.contains(p))
        .collect();

    if new_dirs.is_empty() {
        return;
    }

    let mut watcher_guard = state.watcher.lock().await;
    if let Some(ref mut w) = *watcher_guard {
        for dir in new_dirs {
            if let Err(e) = w.watch(&dir) {
                tracing::warn!("failed to watch {}: {e}", dir.display());
                continue;
            }
            tracing::info!("watching directory: {}", dir.display());
            watched.insert(dir);
        }
    }
}

/// Handle a Clear request: wipe all caches and reset stats.
async fn handle_clear(state: &SharedState) -> Response {
    // Snapshot counts before clearing.
    let artifacts_removed = {
        let count = state.artifacts.len() as u64;
        state.artifacts.clear();
        count
    };
    let metadata_cleared = state.cache_system.metadata().len() as u64;
    let dep_graph_contexts_cleared = state.dep_graph.stats().context_count as u64;

    // Calculate on-disk artifact size before deleting.
    let on_disk_bytes_freed = match std::fs::read_dir(&state.artifact_dir) {
        Ok(entries) => entries
            .filter_map(|e| e.ok())
            .filter_map(|e| e.metadata().ok().map(|m| m.len()))
            .sum(),
        Err(_) => 0,
    };

    // Clear all subsystems.
    state.dep_graph.clear();
    state.cache_system.clear();
    state.fast_hit_cache.clear();
    state.request_cache.clear();
    state.request_validation_cache.clear();
    state.rsp_cache.clear();
    state.watched_raw_dirs.clear();
    state.system_includes.lock().await.clear();
    state.watched_dirs.lock().await.clear();

    // Reset stats and profiler.
    state.stats.reset();
    state.profiler.reset();

    // Delete on-disk artifact files in parallel.
    if let Ok(entries) = std::fs::read_dir(&state.artifact_dir) {
        use rayon::prelude::*;
        let paths: Vec<_> = entries.flatten().map(|e| e.path()).collect();
        paths.par_iter().for_each(|p| {
            let _ = std::fs::remove_file(p);
        });
    }

    // Clear the in-memory artifact index and persist the empty state.
    state.artifact_store.clear();
    let _ = state.artifact_store.flush();

    // Persist the (now empty) metadata cache so the prior on-disk
    // snapshot stays consistent with the live state. Empty snapshots
    // skip the write entirely, but if a previous snapshot exists we
    // also remove it — without that, a subsequent daemon would
    // restore stale entries that `Clear` was meant to wipe.
    if let Err(e) = state
        .cache_system
        .metadata()
        .save_to_disk(state.metadata_path.as_path())
    {
        tracing::warn!(
            path = %state.metadata_path.display(),
            "metadata cache save during Clear failed: {e}"
        );
    }
    let _ = std::fs::remove_file(state.metadata_path.as_path());

    // Delete on-disk depgraph snapshot.
    let _ = std::fs::remove_file(zccache_depgraph::depgraph_file_path());

    tracing::info!(
        artifacts_removed,
        metadata_cleared,
        dep_graph_contexts_cleared,
        on_disk_bytes_freed,
        "cache cleared"
    );

    Response::Cleared {
        artifacts_removed,
        metadata_cleared,
        dep_graph_contexts_cleared,
        on_disk_bytes_freed,
    }
}

/// Handle a single-roundtrip ephemeral compile: session start + compile + session end.
/// Avoids 3 IPC roundtrips for drop-in wrapper mode.
#[allow(clippy::too_many_arguments)] // Single dispatch hop; ergonomic refactor unblocked once we stop adding new client-side fields.
async fn handle_compile_ephemeral(
    state: &Arc<SharedState>,
    client_pid: u32,
    working_dir: &Path,
    compiler: &Path,
    args: &[String],
    cwd: &Path,
    env: Option<Vec<(String, String)>>,
    stdin: Vec<u8>,
) -> Response {
    // 1. Start ephemeral session (inline, no IPC roundtrip)
    state.stats.record_session();
    let session_resp =
        handle_session_start(state, client_pid, working_dir, None, false, None, false).await;
    let session_id = match session_resp {
        Response::SessionStarted { session_id, .. } => session_id,
        Response::Error { message } => return Response::Error { message },
        other => {
            return Response::Error {
                message: format!("unexpected session start response: {other:?}"),
            };
        }
    };

    // 2. Compile — pass the compiler from the ephemeral request
    let result = handle_compile(state, &session_id, args, cwd, compiler, env, stdin).await;

    // 3. End session (best-effort, no response needed)
    if let Ok(sid) = session_id.parse::<SessionId>() {
        state.session_worktree_roots.remove(&sid);
        state.sessions.end(&sid);
    }

    result
}

/// Hash a file using the metadata cache (with watcher-assisted confidence).
fn hash_file_via_cache(state: &SharedState, path: &Path) -> Option<ContentHash> {
    // Try metadata cache first (stat-verified hash)
    if let Ok(hash) = state.cache_system.metadata().lookup(path) {
        return Some(hash);
    }
    // Fall back to direct hash
    zccache_hash::hash_file(path).ok()
}

/// Handle a SessionStart request: create session, watch working directory.
async fn handle_session_start(
    state: &SharedState,
    client_pid: u32,
    working_dir: &Path,
    log_file: Option<NormalizedPath>,
    track_stats: bool,
    journal_path: Option<NormalizedPath>,
    profile: bool,
) -> Response {
    let session_config = zccache_depgraph::SessionConfig {
        client_pid,
        working_dir: working_dir.into(),
        log_file,
        track_stats,
        journal_path,
        profile,
    };

    let session_id = state.sessions.create(session_config);
    state.session_worktree_roots.insert(
        session_id,
        SessionWorktreeRoot {
            working_dir: working_dir.into(),
            root: resolve_worktree_root(working_dir, None),
        },
    );

    // Mirror any depgraph load-time warning into this session's log so
    // the cold fallback after a version-mismatch / corrupt depgraph.bin
    // is visible to operators reading `last-session.log`. Issue #320.
    {
        let warning_opt = {
            let guard = state.depgraph_load_warning.lock().await;
            guard.clone()
        };
        if let Some(warning) = warning_opt {
            write_session_log(&state.sessions, &session_id, &warning);
        }
    }

    // Watch the working directory for file changes.
    watch_directory(state, working_dir).await;

    let journal_path = state
        .sessions
        .get(&session_id)
        .and_then(|s| s.journal_path.clone());

    Response::SessionStarted {
        session_id: session_id.to_string(),
        journal_path,
    }
}

/// Apply a mutation to the session's stats tracker (if tracking is enabled).
fn record_session_stat(
    sessions: &SessionManager,
    session_id: &SessionId,
    f: impl FnOnce(&mut zccache_depgraph::SessionStatsTracker),
) {
    sessions.mutate(session_id, |session| {
        if let Some(ref mut tracker) = session.stats_tracker {
            f(tracker);
        }
    });
}

/// Write a log line to the session's log file (if configured).
fn write_session_log(sessions: &SessionManager, session_id: &SessionId, message: &str) {
    if let Some(session) = sessions.get(session_id) {
        if let Some(ref log_path) = session.log_file {
            use std::io::Write;
            if let Ok(mut f) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(log_path)
            {
                let _ = writeln!(f, "{message}");
            }
        }
    }
}

/// Hash a file using the CacheSystem's metadata cache.
///
/// This stat-verifies the file, hashes if needed (with TOCTOU protection),
/// and caches the result. The file watcher proactively downgrades confidence
/// on changes, ensuring stale hashes are re-computed.
///
/// `clock` should be snapped once at the start of each compile request so all
/// files in a single compilation see a consistent journal clock.
fn hash_file(cache_system: &CacheSystem, path: &Path, clock: Clock) -> Result<ContentHash, String> {
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

/// For a PCH binary (.pch/.gch), return the path to its source header.
///
/// Clang PCH output is non-deterministic (embeds timestamps), so hashing the
/// binary produces different keys even when headers haven't changed. Instead,
/// we hash the source header which IS deterministic.
///
/// `test_pch.h.pch` → `test_pch.h`
/// `.build/meson-quick/tests/test_pch.h.pch` → tries sibling `test_pch.h`,
/// then walks parent directories looking for `tests/test_pch.h`.
fn pch_source_header(path: &Path) -> Option<NormalizedPath> {
    let ext = path.extension()?.to_str()?;
    if ext != "pch" && ext != "gch" {
        return None;
    }
    // The stem of "test_pch.h.pch" is "test_pch.h"
    let header_name = path.file_stem()?;
    // Try sibling: same directory
    let sibling = path.with_file_name(header_name);
    if sibling.exists() {
        return Some(sibling.into());
    }
    // The PCH is typically in a build directory. Walk up looking for the
    // source header by matching the last path component(s).
    // e.g., .build/meson-quick/tests/test_pch.h.pch → look for tests/test_pch.h
    if let Some(parent) = path.parent() {
        // Get the directory name (e.g., "tests")
        if let Some(dir_name) = parent.file_name() {
            let relative = NormalizedPath::new(dir_name).join(header_name);
            // Walk up from the build dir looking for a matching path
            let mut search: NormalizedPath = parent.into();
            for _ in 0..10 {
                if let Some(up) = search.parent() {
                    let candidate = up.join(&relative);
                    if candidate.exists() {
                        return Some(candidate.into());
                    }
                    search = up.into();
                } else {
                    break;
                }
            }
        }
    }
    None
}

/// Resolve the source header for a PCH binary. First checks the in-memory
/// registry (populated when PCH generation succeeds), then falls back to the
/// filesystem heuristic. Returns `None` for non-PCH files.
fn resolve_pch_source(
    path: &Path,
    pch_map: &DashMap<NormalizedPath, NormalizedPath>,
) -> Option<NormalizedPath> {
    // Fast path: check registry (covers build-dir separation).
    if let Some(src) = pch_map.get(&NormalizedPath::new(path)) {
        return Some(src.clone());
    }
    // Fallback: filesystem heuristic.
    pch_source_header(path)
}

/// Expand response file references with caching.
///
/// For each `@file` argument, checks if the expansion is already cached.
/// If so, uses the cached result (no file I/O or canonicalize). Otherwise,
/// expands the reference and caches the result for future requests.
/// Non-`@file` arguments are passed through unchanged.
fn expand_args_cached(state: &SharedState, args: &[String], cwd: &Path) -> Vec<String> {
    // Quick check: skip expansion if no @file references exist
    if !args.iter().any(|a| a.len() > 1 && a.starts_with('@')) {
        return args.to_vec();
    }

    let mut result = Vec::with_capacity(args.len());
    for arg in args {
        if arg.len() > 1 && arg.starts_with('@') {
            let filename = &arg[1..];
            let resolved: NormalizedPath = if Path::new(filename).is_absolute() {
                filename.into()
            } else {
                cwd.join(filename).into()
            };

            match expand_rsp_arg_cached(state, &resolved) {
                Ok(expanded) => result.extend(expanded),
                Err(e) => {
                    tracing::debug!("response file expansion failed: {e}, passing raw arg");
                    result.push(arg.clone());
                }
            }
        } else {
            result.push(arg.clone());
        }
    }
    result
}

fn expand_rsp_arg_cached(state: &SharedState, resolved: &Path) -> Result<Vec<String>, String> {
    let canonical: NormalizedPath = resolved
        .canonicalize()
        .map_err(|e| format!("failed to read response file '{}': {e}", resolved.display()))?
        .into();

    if let Some(cached) = state.rsp_cache.get(&canonical) {
        let fresh = cached
            .dependencies
            .iter()
            .all(|dep| hash_file_via_cache(state, &dep.path) == Some(dep.hash));
        if fresh {
            return Ok(cached.expanded.clone());
        }
    }

    let mut seen = HashSet::new();
    let mut dependencies = Vec::new();
    let expanded = expand_rsp_recursive(state, &canonical, &mut seen, &mut dependencies, 0)
        .map_err(|e| e.to_string())?;
    state.rsp_cache.insert(
        canonical,
        RspCacheEntry {
            expanded: expanded.clone(),
            dependencies,
            cached_at: std::time::Instant::now(),
        },
    );
    Ok(expanded)
}

fn expand_rsp_recursive(
    state: &SharedState,
    path: &Path,
    seen: &mut HashSet<NormalizedPath>,
    dependencies: &mut Vec<RspDependency>,
    depth: usize,
) -> Result<Vec<String>, zccache_compiler::response_file::ResponseFileError> {
    use zccache_compiler::response_file::{parse_response_file_content, ResponseFileError};

    const MAX_RSP_DEPTH: usize = 10;

    if depth >= MAX_RSP_DEPTH {
        return Err(ResponseFileError::TooDeep { path: path.into() });
    }

    let canonical: NormalizedPath = path
        .canonicalize()
        .map_err(|e| ResponseFileError::ReadError {
            path: path.into(),
            source: e,
        })?
        .into();

    if !seen.insert(canonical.clone()) {
        return Err(ResponseFileError::CircularReference {
            path: canonical.clone(),
        });
    }

    let content_hash =
        hash_file_via_cache(state, &canonical).ok_or_else(|| ResponseFileError::ReadError {
            path: canonical.clone(),
            source: std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "failed to hash response file",
            ),
        })?;
    dependencies.push(RspDependency {
        path: canonical.clone(),
        hash: content_hash,
    });

    let content =
        std::fs::read_to_string(&canonical).map_err(|e| ResponseFileError::ReadError {
            path: canonical.clone(),
            source: e,
        })?;
    let base_dir = canonical.parent().unwrap_or_else(|| Path::new("."));
    let mut expanded = Vec::new();
    for child in parse_response_file_content(&content) {
        if let Some(filename) = child.strip_prefix('@') {
            if filename.is_empty() {
                expanded.push(child);
                continue;
            }
            let child_path: NormalizedPath = if Path::new(filename).is_absolute() {
                filename.into()
            } else {
                base_dir.join(filename).into()
            };
            expanded.extend(expand_rsp_recursive(
                state,
                &child_path,
                seen,
                dependencies,
                depth + 1,
            )?);
        } else {
            expanded.push(child);
        }
    }

    seen.remove(&canonical);
    Ok(expanded)
}

/// Check if all files in a context's dependency list are unchanged since
/// the given clock. Uses per-file journal tracking instead of global clock
/// comparison, so output file changes (like .o writes) don't invalidate
/// fast-hit entries for unrelated source contexts.
fn context_files_fresh(
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

/// Apply client environment variables to a compiler command, then overlay
/// spawn-lineage markers so orphan trackers can attribute the child to
/// zccache (see `crate::lineage`).
///
/// If `client_env` is `Some`, the inherited env is cleared and replaced with
/// the client's vars. Lineage env vars are layered on top in either case so
/// the child always carries the chain.
fn apply_client_env(
    cmd: &mut tokio::process::Command,
    client_env: &Option<Vec<(String, String)>>,
    lineage: &crate::lineage::Lineage,
) {
    if let Some(vars) = client_env {
        cmd.env_clear();
        for (key, val) in vars {
            if client_env_var_is_safe_to_replay(key) {
                cmd.env(key, val);
            }
        }
    }
    lineage.apply_to_tokio(cmd, client_env.as_deref());
}

/// Cargo jobserver env vars name process-local file descriptors. The daemon
/// receives those names through IPC, not the fds themselves, so replaying them
/// into daemon-spawned compilers produces Cargo's stale-jobserver warning.
fn client_env_var_is_safe_to_replay(key: &str) -> bool {
    !matches!(key, "MAKEFLAGS" | "CARGO_MAKEFLAGS")
}

/// Sync-command counterpart of [`apply_client_env`].
fn apply_client_env_sync(
    cmd: &mut std::process::Command,
    client_env: Option<&[(String, String)]>,
    lineage: &crate::lineage::Lineage,
) {
    if let Some(vars) = client_env {
        cmd.env_clear();
        for (key, val) in vars {
            if client_env_var_is_safe_to_replay(key) {
                cmd.env(key, val);
            }
        }
    }
    lineage.apply_to_sync(cmd, client_env);
}

/// Look up the client PID for a session. Returns `None` if the session is
/// unknown (already ended) — callers should still emit lineage with whatever
/// they know.
fn session_client_pid(state: &SharedState, sid: &SessionId) -> Option<u32> {
    state.sessions.get(sid).map(|s| s.client_pid)
}

/// Run the compiler directly without caching.
///
/// `tmp_dir` is where the synthesized Windows response file lands when the
/// command line exceeds the OS limit. Production callers pass the daemon's
/// `state.depfile_tmpdir` (under the cache root) so the contents are
/// covered by the wrapper's Defender exclusion — see issue #275.
#[allow(clippy::too_many_arguments)] // Mirrors handle_compile's surface — refactor parked.
async fn run_compiler_direct(
    compiler: &NormalizedPath,
    args: &[String],
    cwd: &Path,
    sessions: &SessionManager,
    sid: &SessionId,
    client_env: &Option<Vec<(String, String)>>,
    stdin_bytes: &[u8],
    tmp_dir: &Path,
) -> Response {
    let _rsp_guard =
        match zccache_compiler::response_file::write_response_file_if_needed(args, tmp_dir) {
            Ok(guard) => guard,
            Err(e) => {
                return Response::Error {
                    message: format!("failed to write response file: {e}"),
                };
            }
        };

    let lineage = crate::lineage::Lineage::current(
        sessions.get(sid).map(|s| s.client_pid),
        Some(sid.to_string()),
    );
    let mut cmd = tokio::process::Command::new(compiler);
    if let Some(ref rsp) = _rsp_guard {
        cmd.arg(rsp.at_arg()).current_dir(cwd);
    } else {
        cmd.args(args).current_dir(cwd);
    }
    apply_client_env(&mut cmd, client_env, &lineage);
    let compiler_priority = CompilePriority::from_client_env(client_env.as_deref());
    let result = crate::process::tokio_command_output_with_priority_stdin(
        &mut cmd,
        compiler_priority,
        if stdin_bytes.is_empty() {
            None
        } else {
            Some(stdin_bytes)
        },
    )
    .await;

    match result {
        Ok(output) => {
            let exit_code = output.status.code().unwrap_or(-1);
            write_session_log(sessions, sid, &format!("[DIRECT] exit_code={exit_code}"));
            Response::CompileResult {
                exit_code,
                stdout: Arc::new(output.stdout),
                stderr: Arc::new(output.stderr),
                cached: false,
            }
        }
        Err(e) => Response::Error {
            message: format!("failed to run compiler: {e}"),
        },
    }
}

mod connection;
mod handle_compile;
mod handle_compile_multi;
mod handle_link;
mod keys;
mod link_helpers;
mod persist;
mod rustc;
mod wal;
use connection::handle_connection;
use handle_compile::handle_compile;
use handle_compile_multi::handle_compile_multi;
use handle_link::handle_link_ephemeral;
#[cfg(test)]
use handle_link::run_post_link_deploy_hook;
use keys::*;
use link_helpers::*;
use persist::*;
use rustc::*;
use wal::*;

#[cfg(test)]
mod tests;
