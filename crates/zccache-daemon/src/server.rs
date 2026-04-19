//! Daemon server — accepts IPC connections and handles requests.

use dashmap::DashMap;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
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
use zccache_protocol::{ArtifactData, ArtifactOutput, Request, Response};
use zccache_watcher::{NotifyWatcher, SettleBuffer, SettledEvent};

use crate::compile_journal::{extract_outcome, CompileJournal, JournalContext, JournalEntry};
use crate::fingerprint::FingerprintManager;
use crate::stats::{HitPhases, MissPhases, PhaseProfiler, StatsCollector};

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

#[derive(Clone)]
struct RspDependency {
    path: NormalizedPath,
    hash: ContentHash,
}

#[derive(Clone)]
struct RspCacheEntry {
    expanded: Vec<String>,
    dependencies: Vec<RspDependency>,
}

#[derive(Clone)]
/// Cached compilation artifact with lazy payload loading.
///
/// Metadata (output names, sizes, stdout, stderr, exit code) is always in
/// memory after startup.  Output payloads (`{key}_0` file bytes) are loaded
/// lazily on the first cache hit to avoid reading gigabytes at startup.
pub(crate) struct CachedArtifact {
    pub(crate) meta: ArtifactIndex,
    /// Arc-wrapped stdout/stderr for cheap IPC response clones.
    pub(crate) stdout: Arc<Vec<u8>>,
    pub(crate) stderr: Arc<Vec<u8>>,
    /// Lazily-loaded output payloads. `None` = not yet loaded from disk.
    /// Arc-wrapped so cache-hit clones are O(1) refcount bumps.
    pub(crate) payloads: Option<Arc<[Arc<Vec<u8>>]>>,
    /// When this artifact was last used (inserted or returned as a hit).
    pub(crate) last_used: std::time::Instant,
}

impl CachedArtifact {
    /// Create from a freshly compiled `ArtifactData` (payloads already in memory).
    fn from_artifact_data(artifact: &ArtifactData) -> Self {
        let meta = ArtifactIndex::new(
            artifact.outputs.iter().map(|o| o.name.clone()).collect(),
            artifact
                .outputs
                .iter()
                .map(|o| o.data.len() as u64)
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
                    .map(|o| Arc::clone(&o.data))
                    .collect::<Vec<_>>(),
            )),
            last_used: std::time::Instant::now(),
        }
    }

    /// Create from index metadata (lazy — payloads not loaded yet).
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
) -> Option<&'a [Arc<Vec<u8>>]> {
    if cached.payloads.is_none() {
        let mut payloads = Vec::with_capacity(cached.meta.output_names.len());
        for i in 0..cached.meta.output_names.len() {
            let path = artifact_dir.join(format!("{key_hex}_{i}"));
            match std::fs::read(&path) {
                Ok(data) => payloads.push(Arc::new(data)),
                Err(_) => return None,
            }
        }
        cached.payloads = Some(Arc::from(payloads));
    }
    cached.payloads.as_deref()
}

/// Migrate legacy `.meta` files to the redb index.
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
            for (i, out) in artifact.outputs.iter().enumerate() {
                let data_path = artifact_dir.join(format!("{stem}_{i}"));
                if !data_path.exists() {
                    std::fs::write(&data_path, &*out.data).ok();
                }
            }

            let cached = CachedArtifact::from_artifact_data(&artifact);
            Some((stem, cached, path.clone()))
        })
        .collect();

    // Sequential phase: insert into redb (single writer) and DashMap,
    // then delete old .meta files.
    let count = migrated.len();
    for (stem, cached, meta_path) in migrated {
        store.insert(&stem, &cached.meta).ok();
        artifacts.insert(stem, cached);
        std::fs::remove_file(&meta_path).ok();
    }

    if count > 0 {
        tracing::info!(count, "migrated legacy .meta files to redb index");
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
    /// redb-backed artifact index for fast startup and persistence.
    artifact_store: ArtifactStore,
    /// Whether the background artifact loading has completed.
    artifacts_loaded: AtomicBool,
    /// Fingerprint manager: tracks per-watch dirty state for `zccache fp` commands.
    fingerprint: FingerprintManager,
}

/// Pre-computed compile request data for the request-level fast path.
struct RequestCacheEntry {
    context_key: ContextKey,
    source_path: NormalizedPath,
    output_path: NormalizedPath,
}

/// The daemon server that listens for IPC connections.
pub struct DaemonServer {
    listener: IpcListener,
    shutdown: Arc<Notify>,
    state: Arc<SharedState>,
}

/// Remove fast-hit cache entries older than `max_age`. Returns entries removed.
pub(crate) fn trim_fast_hit_cache(
    cache: &DashMap<ContextKey, FastHitEntry>,
    max_age: std::time::Duration,
) -> usize {
    let mut removed = 0;
    cache.retain(|_, entry| {
        if entry.cached_at.elapsed() > max_age {
            removed += 1;
            false
        } else {
            true
        }
    });
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

impl DaemonServer {
    /// Create a new daemon server bound to the given endpoint.
    pub fn bind(endpoint: &str) -> Result<Self, zccache_ipc::IpcError> {
        let listener = IpcListener::bind(endpoint)?;
        let shutdown = Arc::new(Notify::new());
        let now = now_secs();
        let instance = SERVER_INSTANCE.fetch_add(1, Ordering::Relaxed);
        let artifact_dir = zccache_core::config::artifacts_dir();
        std::fs::create_dir_all(&artifact_dir).ok();

        // Artifact loading is deferred to a background task in run() so the
        // daemon starts accepting connections immediately (Bug 6 fix).
        let artifacts: DashMap<String, CachedArtifact> = DashMap::new();

        // Open redb artifact index for fast startup + persistence.
        let index_path = zccache_core::config::index_path();
        let artifact_store = ArtifactStore::open(&index_path).map_err(|e| {
            zccache_ipc::IpcError::Io(std::io::Error::other(format!(
                "failed to open artifact index at {}: {e}",
                index_path.display()
            )))
        })?;

        Ok(Self {
            listener,
            shutdown: Arc::clone(&shutdown),
            state: Arc::new(SharedState {
                sessions: SessionManager::new(std::time::Duration::from_secs(300)),
                system_includes: Mutex::new(SystemIncludeCache::new()),
                dep_graph: DepGraph::new(),
                artifacts,
                cache_system: CacheSystem::new(),
                watcher: Mutex::new(None),
                watched_dirs: Mutex::new(HashSet::new()),
                shutdown,
                last_activity: AtomicU64::new(now),
                start_time: now,
                stats: StatsCollector::new(),
                profiler: PhaseProfiler::new(),
                artifact_dir,
                depfile_tmpdir: {
                    let dir = zccache_core::config::depfile_dir()
                        .join(format!("{}-{instance}", std::process::id()));
                    std::fs::create_dir_all(&dir).ok();
                    dir
                },
                fast_hit_cache: DashMap::new(),
                watcher_active: AtomicBool::new(false),
                rsp_cache: DashMap::new(),
                request_cache: DashMap::new(),
                watched_raw_dirs: DashMap::new(),
                pch_source_map: DashMap::new(),
                journal: CompileJournal::new(zccache_core::config::log_dir()),
                in_flight_bytes: AtomicUsize::new(0),
                persist_semaphore: Arc::new(tokio::sync::Semaphore::new(8)),
                artifact_store,
                artifacts_loaded: AtomicBool::new(false),
                fingerprint: FingerprintManager::new(),
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
    pub fn set_dep_graph(&mut self, graph: zccache_depgraph::DepGraph) {
        let state =
            Arc::get_mut(&mut self.state).expect("set_dep_graph must be called before run()");
        state.dep_graph = graph;
    }

    /// Get a snapshot of the phase profiler (for benchmarks).
    #[must_use]
    pub fn profile_snapshot(&self) -> crate::stats::ProfileSnapshot {
        self.state.profiler.snapshot()
    }

    /// Run the server, accepting connections until shutdown is signaled.
    ///
    /// `idle_timeout_secs`: if non-zero, the daemon shuts down after this many
    /// seconds with no client activity. Pass 0 to disable.
    pub async fn run(&mut self, idle_timeout_secs: u64) -> Result<(), zccache_ipc::IpcError> {
        tracing::info!("daemon server running");

        // Clean up legacy log backup directory (Bug 7).
        {
            let legacy_logs = zccache_core::config::default_cache_dir().join("logs.bak");
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
            let legacy_lock = zccache_core::config::default_cache_dir().join("daemon.lock.bak");
            let _ = std::fs::remove_file(&legacy_lock);
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
                    // Try loading from redb index first.
                    match state_ref.artifact_store.load_all() {
                        Ok(entries) if !entries.is_empty() => {
                            let count = entries.len();
                            for (key, meta) in entries {
                                artifacts.insert(key, CachedArtifact::from_index(meta));
                            }
                            count
                        }
                        _ => {
                            // Migration: load from .meta files, populate redb index.
                            migrate_meta_files(&artifact_dir, &artifacts, &state_ref.artifact_store)
                        }
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
                        Ok(()) => tracing::debug!("periodic depgraph save"),
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

                    // Save depgraph to disk before exiting.
                    let start = std::time::Instant::now();
                    let path = zccache_depgraph::depgraph_file_path();
                    if let Some(parent) = path.parent() {
                        std::fs::create_dir_all(parent).ok();
                    }
                    match zccache_depgraph::save_to_file(&self.state.dep_graph, &path) {
                        Ok(()) => tracing::info!(
                            elapsed_ms = start.elapsed().as_millis() as u64,
                            "depgraph saved"
                        ),
                        Err(e) => tracing::warn!("depgraph save failed: {e}"),
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

/// Handle a single client connection.
async fn handle_connection(
    mut conn: IpcConnection,
    state: Arc<SharedState>,
) -> Result<(), zccache_ipc::IpcError> {
    loop {
        let request: Option<Request> = conn.recv().await?;
        let Some(request) = request else {
            tracing::debug!("client disconnected");
            return Ok(());
        };

        tracing::debug!(?request, "received request");
        state.last_activity.store(now_secs(), Ordering::Relaxed);

        // Dispatch request and capture journal metadata in the same match
        // to move args/session_id into JournalContext without cloning.
        // Only env needs cloning because handlers consume it.
        let journal_start = std::time::Instant::now();
        let (response, journal_ctx): (Response, Option<JournalContext>) = match request {
            Request::Ping => (Response::Pong, None),
            Request::Shutdown => {
                conn.send(&Response::ShuttingDown).await?;
                state.shutdown.notify_one();
                return Ok(());
            }
            Request::Status => {
                let snap = state.stats.snapshot();
                let dg = state.dep_graph.stats();
                let artifact_count = state.artifacts.len() as u64;
                let cache_size_bytes: u64 = state
                    .artifacts
                    .iter()
                    .map(|entry| entry.value().meta.total_size)
                    .sum();
                let metadata_entries = state.cache_system.metadata().len() as u64;
                (
                    Response::Status(zccache_protocol::DaemonStatus {
                        version: zccache_core::VERSION.to_string(),
                        artifact_count,
                        cache_size_bytes,
                        metadata_entries,
                        uptime_secs: now_secs().saturating_sub(state.start_time),
                        cache_hits: snap.hits,
                        cache_misses: snap.misses,
                        total_compilations: snap.compilations,
                        non_cacheable: snap.non_cacheable,
                        compile_errors: snap.compile_errors,
                        time_saved_ms: snap.time_saved_ms(),
                        total_links: snap.link_total,
                        link_hits: snap.link_hits,
                        link_misses: snap.link_misses,
                        link_non_cacheable: snap.link_non_cacheable,
                        dep_graph_contexts: dg.context_count as u64,
                        dep_graph_files: dg.file_count as u64,
                        sessions_total: snap.sessions_total,
                        sessions_active: state.sessions.active_count() as u64,
                        cache_dir: zccache_core::config::default_cache_dir(),
                        dep_graph_version: zccache_depgraph::DEPGRAPH_VERSION,
                        dep_graph_disk_size: zccache_depgraph::depgraph_file_path()
                            .metadata()
                            .map(|m| m.len())
                            .unwrap_or(0),
                    }),
                    None,
                )
            }
            Request::Lookup { .. } => (
                Response::LookupResult(zccache_protocol::LookupResult::Miss),
                None,
            ),
            Request::Store { .. } => (
                Response::StoreResult(zccache_protocol::StoreResult::Stored),
                None,
            ),
            Request::Clear => (handle_clear(&state).await, None),
            Request::SessionStart {
                client_pid,
                working_dir,
                log_file,
                track_stats,
                journal_path,
            } => {
                state.stats.record_session();
                (
                    handle_session_start(
                        &state,
                        client_pid,
                        &working_dir,
                        log_file,
                        track_stats,
                        journal_path,
                    )
                    .await,
                    None,
                )
            }
            Request::Compile {
                session_id,
                args,
                cwd,
                compiler,
                env,
            } => {
                let ctx = JournalContext {
                    compiler: compiler.to_string_lossy().into_owned(),
                    args,
                    cwd: cwd.to_string_lossy().into_owned(),
                    env: env.clone(),
                    session_id: Some(session_id),
                };
                let resp = handle_compile(
                    &state,
                    ctx.session_id.as_deref().unwrap(),
                    &ctx.args,
                    &cwd,
                    &compiler,
                    env,
                )
                .await;
                (resp, Some(ctx))
            }
            Request::CompileEphemeral {
                client_pid,
                working_dir,
                compiler,
                args,
                cwd,
                env,
            } => {
                let ctx = JournalContext {
                    compiler: compiler.to_string_lossy().into_owned(),
                    args,
                    cwd: cwd.to_string_lossy().into_owned(),
                    env: env.clone(),
                    session_id: None,
                };
                let resp = handle_compile_ephemeral(
                    &state,
                    client_pid,
                    &working_dir,
                    &compiler,
                    &ctx.args,
                    &cwd,
                    env,
                )
                .await;
                (resp, Some(ctx))
            }
            Request::SessionStats { session_id } => (
                match session_id.parse::<SessionId>() {
                    Ok(sid) => {
                        if let Some(session) = state.sessions.get(&sid) {
                            let stats = session.stats_tracker.as_ref().map(|tracker| {
                                let f = tracker.finalize(session.created_at);
                                zccache_protocol::SessionStats {
                                    duration_ms: f.duration_ms,
                                    compilations: f.compilations,
                                    hits: f.hits,
                                    misses: f.misses,
                                    non_cacheable: f.non_cacheable,
                                    errors: f.errors,
                                    time_saved_ms: f.time_saved_ms,
                                    unique_sources: f.unique_sources,
                                    bytes_read: f.bytes_read,
                                    bytes_written: f.bytes_written,
                                }
                            });
                            Response::SessionStatsResult { stats }
                        } else {
                            Response::Error {
                                message: format!("unknown session: {session_id}"),
                            }
                        }
                    }
                    Err(_) => Response::Error {
                        message: format!("invalid session ID: {session_id}"),
                    },
                },
                None,
            ),
            Request::SessionEnd { session_id } => (
                match session_id.parse::<SessionId>() {
                    Ok(sid) => {
                        if let Some(session) = state.sessions.end(&sid) {
                            // Close the session journal file handle if one was open.
                            if let Some(ref path) = session.journal_path {
                                state.journal.close_session(path);
                            }
                            let stats = session.stats_tracker.map(|tracker| {
                                let f = tracker.finalize(session.created_at);
                                zccache_protocol::SessionStats {
                                    duration_ms: f.duration_ms,
                                    compilations: f.compilations,
                                    hits: f.hits,
                                    misses: f.misses,
                                    non_cacheable: f.non_cacheable,
                                    errors: f.errors,
                                    time_saved_ms: f.time_saved_ms,
                                    unique_sources: f.unique_sources,
                                    bytes_read: f.bytes_read,
                                    bytes_written: f.bytes_written,
                                }
                            });
                            Response::SessionEnded { stats }
                        } else {
                            Response::Error {
                                message: format!("unknown session: {session_id}"),
                            }
                        }
                    }
                    Err(_) => Response::Error {
                        message: format!("invalid session ID: {session_id}"),
                    },
                },
                None,
            ),
            Request::LinkEphemeral {
                client_pid,
                tool,
                args,
                cwd,
                env,
            } => {
                let ctx = JournalContext {
                    compiler: tool.to_string_lossy().into_owned(),
                    args,
                    cwd: cwd.to_string_lossy().into_owned(),
                    env: env.clone(),
                    session_id: None,
                };
                let resp =
                    handle_link_ephemeral(&state, client_pid, &tool, &ctx.args, &cwd, env).await;
                (resp, Some(ctx))
            }
            Request::FingerprintCheck {
                cache_file,
                cache_type,
                root,
                extensions,
                include_globs,
                exclude,
            } => {
                // Register watcher BEFORE check so events arriving during
                // the scan are not lost.
                watch_directory(&state, &root).await;
                let result = state.fingerprint.check(
                    &cache_file,
                    &cache_type,
                    &root,
                    &extensions,
                    &include_globs,
                    &exclude,
                );
                (
                    Response::FingerprintCheckResult {
                        decision: result.decision,
                        reason: result.reason,
                        changed_files: result.changed_files,
                    },
                    None,
                )
            }
            Request::FingerprintMarkSuccess { cache_file } => {
                state.fingerprint.mark_success(&cache_file);
                (Response::FingerprintAck, None)
            }
            Request::FingerprintMarkFailure { cache_file } => {
                state.fingerprint.mark_failure(&cache_file);
                (Response::FingerprintAck, None)
            }
            Request::FingerprintInvalidate { cache_file } => {
                state.fingerprint.invalidate(&cache_file);
                (Response::FingerprintAck, None)
            }
            Request::ListRustArtifacts => {
                let mut artifacts = Vec::new();
                for entry in state.artifacts.iter() {
                    let key = entry.key().clone();
                    let cached = entry.value();
                    // Only include artifacts that look like Rust outputs
                    // (.rlib, .rmeta, .d files).
                    let names: Vec<String> = cached.meta.output_names.to_vec();
                    let is_rust = names.iter().any(|n| {
                        n.ends_with(".rlib")
                            || n.ends_with(".rmeta")
                            || n.ends_with(".d")
                            || n.ends_with(".so")
                            || n.ends_with(".dylib")
                            || n.ends_with(".dll")
                    });
                    if is_rust {
                        artifacts.push(zccache_protocol::RustArtifactInfo {
                            cache_key: key,
                            output_names: names.clone(),
                            payload_count: names.len(),
                        });
                    }
                }
                (Response::RustArtifactList { artifacts }, None)
            }
        };

        // Log to compile journal for journalable requests.
        if let Some(ctx) = journal_ctx {
            if let Some((outcome, exit_code)) = extract_outcome(&response) {
                let latency_ns = journal_start.elapsed().as_nanos();
                // Look up session journal path for per-session logging.
                let session_journal_path = ctx.session_id.as_ref().and_then(|sid| {
                    sid.parse::<SessionId>().ok().and_then(|parsed| {
                        state
                            .sessions
                            .get(&parsed)
                            .and_then(|s| s.journal_path.clone())
                    })
                });
                state.journal.log(
                    &JournalEntry::new(ctx, outcome, exit_code, latency_ns),
                    session_journal_path.as_deref(),
                );
            }
        }

        conn.send(&response).await?;
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

    // Clear redb artifact index.
    state.artifact_store.clear().ok();

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
async fn handle_compile_ephemeral(
    state: &Arc<SharedState>,
    client_pid: u32,
    working_dir: &Path,
    compiler: &Path,
    args: &[String],
    cwd: &Path,
    env: Option<Vec<(String, String)>>,
) -> Response {
    // 1. Start ephemeral session (inline, no IPC roundtrip)
    state.stats.record_session();
    let session_resp =
        handle_session_start(state, client_pid, working_dir, None, false, None).await;
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
    let result = handle_compile(state, &session_id, args, cwd, compiler, env).await;

    // 3. End session (best-effort, no response needed)
    if let Ok(sid) = session_id.parse::<SessionId>() {
        state.sessions.end(&sid);
    }

    result
}

/// Handle a single-roundtrip ephemeral link/archive request.
///
/// Parses the tool invocation, computes a cache key from the tool binary and
/// all input file hashes, and returns a cached result or runs the real tool.
async fn handle_link_ephemeral(
    state: &Arc<SharedState>,
    _client_pid: u32,
    tool: &Path,
    args: &[String],
    cwd: &Path,
    env: Option<Vec<(String, String)>>,
) -> Response {
    use zccache_compiler::parse_archiver::{parse_archive_invocation, ParsedArchiveInvocation};
    use zccache_compiler::parse_linker::{parse_linker_invocation, ParsedLinkerInvocation};

    state.stats.record_link();

    // 1. Parse the tool invocation — try archiver first, then linker
    struct ParsedTool {
        input_files: Vec<NormalizedPath>,
        output_file: NormalizedPath,
        secondary_outputs: Vec<NormalizedPath>,
        cache_relevant_flags: Vec<String>,
        non_deterministic: bool,
        non_determinism_hint: String,
    }

    let parsed_tool = match parse_archive_invocation(tool.to_str().unwrap_or(""), args) {
        ParsedArchiveInvocation::Cacheable(c) => ParsedTool {
            non_determinism_hint: match c.family {
                zccache_compiler::parse_archiver::ArchiverFamily::MsvcLib => "/BREPRO".to_string(),
                _ => "D".to_string(),
            },
            input_files: c.input_files,
            output_file: c.output_file,
            secondary_outputs: Vec::new(),
            cache_relevant_flags: c.cache_relevant_flags,
            non_deterministic: c.non_deterministic,
        },
        ParsedArchiveInvocation::NonCacheable { reason: ar_reason } => {
            // Try linker parser
            match parse_linker_invocation(tool.to_str().unwrap_or(""), args.to_vec()) {
                ParsedLinkerInvocation::Cacheable(c) => ParsedTool {
                    non_determinism_hint: match c.family {
                        zccache_compiler::parse_linker::LinkerFamily::MsvcLink => {
                            "/DETERMINISTIC".to_string()
                        }
                        _ => "--build-id=sha1 (avoid --build-id=uuid)".to_string(),
                    },
                    input_files: c.input_files,
                    output_file: c.output_file,
                    secondary_outputs: c.secondary_outputs,
                    cache_relevant_flags: c.cache_relevant_flags,
                    non_deterministic: c.non_deterministic,
                },
                ParsedLinkerInvocation::NonCacheable {
                    reason: link_reason,
                } => {
                    tracing::debug!(
                        ar_reason = %ar_reason,
                        link_reason = %link_reason,
                        "link non-cacheable, passing through"
                    );
                    state.stats.record_link_non_cacheable();
                    return run_tool_passthrough(tool, args, cwd, env).await;
                }
            }
        }
    };

    // 2. Non-determinism check: warn but still cache
    let nd_warning = if parsed_tool.non_deterministic {
        let w = format!(
            "non-deterministic invocation (missing {} flag) — output is cached but may differ from a fresh link",
            parsed_tool.non_determinism_hint
        );
        tracing::warn!(%w);
        Some(w)
    } else {
        None
    };

    // 3. Hash the tool binary
    let tool_path = std::path::Path::new(tool);
    let tool_hash = match hash_file_via_cache(state, tool_path) {
        Some(h) => h,
        None => {
            tracing::warn!("cannot hash tool {}", tool.display());
            return run_tool_passthrough(tool, args, cwd, env).await;
        }
    };

    // 4. Hash all input files
    let cwd_path = std::path::Path::new(cwd);
    let mut key_builder = zccache_hash::link_cache_key::LinkCacheKeyBuilder::new().tool(tool_hash);

    for flag in &parsed_tool.cache_relevant_flags {
        key_builder = key_builder.flag(flag);
    }

    for input in &parsed_tool.input_files {
        let input_path = if input.is_absolute() {
            input.clone()
        } else {
            cwd_path.join(input).into()
        };
        let input_hash = match hash_file_via_cache(state, &input_path) {
            Some(h) => h,
            None => {
                tracing::warn!(
                    "cannot hash input file {}: skipping cache",
                    input_path.display()
                );
                return run_tool_passthrough(tool, args, cwd, env).await;
            }
        };
        key_builder = key_builder.input(input_hash);
    }

    let cache_key = key_builder.build();
    let key_hex = cache_key.to_hex();

    // 5. Cache lookup
    if let Some(mut entry) = state.artifacts.get_mut(&key_hex) {
        entry.last_used = std::time::Instant::now();
        // Load payloads from disk if not already loaded.
        let loaded = ensure_payloads(&mut entry, &state.artifact_dir, &key_hex).is_some();
        if loaded {
            let payloads = Arc::clone(entry.payloads.as_ref().unwrap());
            let names = Arc::clone(&entry.meta.output_names);
            let exit_code = entry.meta.exit_code;
            let stdout = entry.stdout.clone();
            let stderr = entry.stderr.clone();
            drop(entry); // Release DashMap lock

            tracing::debug!(%key_hex, "link cache hit");
            state.stats.record_link_hit();

            // Write cached output to disk
            let output_path = if parsed_tool.output_file.is_absolute() {
                parsed_tool.output_file.clone()
            } else {
                cwd_path.join(&parsed_tool.output_file).into()
            };
            let mut write_ok = true;
            for (i, payload) in payloads.iter().enumerate() {
                let target = if payloads.len() == 1 {
                    output_path.clone()
                } else {
                    output_path
                        .parent()
                        .unwrap_or(cwd_path)
                        .join(&names[i])
                        .into()
                };
                if let Some(parent) = target.parent() {
                    std::fs::create_dir_all(parent).ok();
                }
                let cache_file = state.artifact_dir.join(format!("{key_hex}_{i}"));
                if write_cached_output(&target, &cache_file, payload).is_err() {
                    write_ok = false;
                    break;
                }
            }
            if write_ok {
                return Response::LinkResult {
                    exit_code,
                    stdout,
                    stderr,
                    cached: true,
                    warning: nd_warning,
                };
            }
            // Fall through to passthrough if write failed
            return run_tool_passthrough(tool, args, cwd, env).await;
        }
        // Payloads missing — treat as cache miss, fall through
    }

    // 6. Cache miss — run the real tool
    tracing::debug!(%key_hex, "link cache miss");
    state.stats.record_link_miss();

    // Compute output path early (needed for pre-link directory snapshot).
    let output_path = if parsed_tool.output_file.is_absolute() {
        parsed_tool.output_file.clone()
    } else {
        cwd_path.join(&parsed_tool.output_file).into()
    };
    let output_dir = output_path.parent().unwrap_or(cwd_path);

    // Snapshot the output directory before the link so we can detect
    // side-effect files (e.g., runtime DLLs deployed by compiler wrappers).
    let dir_snapshot = crate::side_effect::snapshot_directory(output_dir);

    // Extract post-link deploy command from env (if any) BEFORE we consume
    // `env` in the passthrough call. See run_post_link_deploy_hook for rationale.
    let deploy_cmd = env
        .as_ref()
        .and_then(|v| {
            v.iter()
                .find(|(k, _)| k == "ZCCACHE_LINK_DEPLOY_CMD")
                .map(|(_, val)| val.clone())
        })
        .filter(|s| !s.is_empty());
    // Clone env for the hook (we need to re-use it; passthrough consumes env).
    let env_for_hook = env.clone();

    let result = run_tool_passthrough(tool, args, cwd, env).await;

    // 6b. Invoke optional post-link deploy command on successful link.
    // This handles the case where the compiler driver does NOT auto-deploy
    // runtime DLLs (e.g. a native trampoline that skips the Python wrapper
    // layer where clang-tool-chain's `post_link_dll_deployment` lives).
    // The hook runs BEFORE the side-effect scan so scanning picks up
    // whatever it deployed.
    if let (Some(cmd), Response::LinkResult { exit_code: 0, .. }) = (&deploy_cmd, &result) {
        run_post_link_deploy_hook(cmd, &output_path, env_for_hook.as_deref()).await;
    }

    // 7. If successful, cache the output
    if let Response::LinkResult {
        exit_code: 0,
        ref stdout,
        ref stderr,
        ..
    } = result
    {
        if let Ok(data) = std::fs::read(&output_path) {
            let mut outputs = vec![ArtifactOutput {
                name: parsed_tool
                    .output_file
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned(),
                data: Arc::new(data),
            }];

            // Collect secondary outputs (e.g., MSVC import lib + .exp)
            for secondary in &parsed_tool.secondary_outputs {
                let sec_path = if secondary.is_absolute() {
                    secondary.clone()
                } else {
                    cwd_path.join(secondary).into()
                };
                if let Ok(sec_data) = std::fs::read(&sec_path) {
                    outputs.push(ArtifactOutput {
                        name: secondary
                            .file_name()
                            .unwrap_or_default()
                            .to_string_lossy()
                            .into_owned(),
                        data: Arc::new(sec_data),
                    });
                }
                // Missing secondary outputs are silently skipped —
                // e.g., .exp may not always be generated.
            }

            // Detect side-effect files deployed by compiler wrapper post-link hooks
            // (e.g., ASan runtime DLLs copied next to the output binary).
            let primary_name = parsed_tool
                .output_file
                .file_name()
                .unwrap_or_default()
                .to_os_string();
            let already_captured: std::collections::HashSet<std::ffi::OsString> = outputs
                .iter()
                .map(|o| std::ffi::OsString::from(&o.name))
                .collect();
            if let Ok(side_effects) = crate::side_effect::detect_side_effects(
                &dir_snapshot,
                output_dir,
                &primary_name,
                &already_captured,
            ) {
                for se in &side_effects {
                    if let Ok(data) = std::fs::read(&se.path) {
                        tracing::debug!(
                            file = %se.file_name.to_string_lossy(),
                            size = data.len(),
                            "caching side-effect file"
                        );
                        outputs.push(ArtifactOutput {
                            name: se.file_name.to_string_lossy().into_owned(),
                            data: Arc::new(data),
                        });
                    }
                }
            }

            let artifact = ArtifactData {
                outputs,
                stdout: stdout.clone(),
                stderr: stderr.clone(),
                exit_code: 0,
            };

            // Build CachedArtifact once (no deep copies — all Arc clones).
            let cached = CachedArtifact::from_artifact_data(&artifact);

            // Persist to disk in background (meta.clone() is cheap — Arc fields only).
            {
                let artifact_dir = state.artifact_dir.clone();
                let kh = key_hex.clone();
                let persist_meta = cached.meta.clone();
                let payloads: Vec<Arc<Vec<u8>>> = artifact
                    .outputs
                    .iter()
                    .map(|o| Arc::clone(&o.data))
                    .collect();
                let payload_size: usize = payloads.iter().map(|p| p.len()).sum();
                state
                    .in_flight_bytes
                    .fetch_add(payload_size, Ordering::Relaxed);
                let guard = InFlightGuard {
                    state: Arc::clone(state),
                    size: payload_size,
                };
                let sem = Arc::clone(&state.persist_semaphore);
                let state_ref = Arc::clone(state);
                tokio::spawn(async move {
                    let _permit = sem.acquire().await.unwrap();
                    tokio::task::spawn_blocking(move || {
                        let _guard = guard;
                        for (i, payload) in payloads.iter().enumerate() {
                            let cache_path = artifact_dir.join(format!("{kh}_{i}"));
                            std::fs::write(&cache_path, &**payload).ok();
                        }
                        state_ref.artifact_store.insert(&kh, &persist_meta).ok();
                    })
                    .await
                    .ok();
                });
            }

            state.artifacts.insert(key_hex.clone(), cached);
            tracing::debug!(%key_hex, "link artifact cached");
        }
    }

    match (result, nd_warning) {
        (
            Response::LinkResult {
                exit_code,
                stdout,
                stderr,
                cached,
                ..
            },
            warning @ Some(_),
        ) => Response::LinkResult {
            exit_code,
            stdout,
            stderr,
            cached,
            warning,
        },
        (result, _) => result,
    }
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

/// Run a tool directly (passthrough) and return a LinkResult response.
async fn run_tool_passthrough(
    tool: &Path,
    args: &[String],
    cwd: &Path,
    env: Option<Vec<(String, String)>>,
) -> Response {
    let tmp_dir = std::env::temp_dir();
    let _rsp_guard =
        match zccache_compiler::response_file::write_response_file_if_needed(args, &tmp_dir) {
            Ok(guard) => guard,
            Err(e) => {
                return Response::Error {
                    message: format!("failed to write response file for {}: {e}", tool.display()),
                };
            }
        };

    let mut cmd = std::process::Command::new(tool);
    if let Some(ref rsp) = _rsp_guard {
        cmd.arg(rsp.at_arg());
    } else {
        cmd.args(args);
    }
    cmd.current_dir(cwd);

    if let Some(env_vars) = env {
        cmd.env_clear();
        for (k, v) in &env_vars {
            cmd.env(k, v);
        }
    }

    match cmd.output() {
        Ok(output) => Response::LinkResult {
            exit_code: output.status.code().unwrap_or(1),
            stdout: Arc::new(output.stdout),
            stderr: Arc::new(output.stderr),
            cached: false,
            warning: None,
        },
        Err(e) => Response::Error {
            message: format!("failed to run {}: {e}", tool.display()),
        },
    }
}

/// Run an optional post-link deploy command on the link output.
///
/// Invoked when `ZCCACHE_LINK_DEPLOY_CMD` is set in the client's env. The
/// command is expected to be a tool like `clang-tool-chain-libdeploy` that
/// takes one positional argument — the path to the just-linked binary — and
/// deploys any runtime dependencies (runtime DLLs on Windows, libc++/libunwind
/// on Linux/macOS) alongside it.
///
/// This fills a gap that exists when the compiler driver does not auto-deploy
/// runtime dependencies during link (for example a native-compiled trampoline
/// that bypasses the driver's post-link Python hooks). The subsequent
/// `side_effect::detect_side_effects` scan in the caller will then pick up
/// whatever this hook deployed and cache it alongside the primary output.
///
/// Failures are non-fatal: we log a warning and return. The link itself has
/// already succeeded — the build will continue, just without the deployed
/// runtime files cached. Consumers relying on the hook should surface
/// failures at their own layer (e.g. via a separate post-build lint).
///
/// The command is parsed as shell-style (split on whitespace) with one trailing
/// argument appended: the output path. For example:
/// ```text
/// ZCCACHE_LINK_DEPLOY_CMD=clang-tool-chain-libdeploy
/// # runs: clang-tool-chain-libdeploy <output_path>
/// ZCCACHE_LINK_DEPLOY_CMD="clang-tool-chain-libdeploy --quiet"
/// # runs: clang-tool-chain-libdeploy --quiet <output_path>
/// ```
async fn run_post_link_deploy_hook(
    cmd_str: &str,
    output_path: &Path,
    env: Option<&[(String, String)]>,
) {
    // Split command string on whitespace — first token is the executable,
    // remaining tokens are extra args. We don't support quoted args yet;
    // keep it simple.
    let mut parts = cmd_str.split_whitespace();
    let program = match parts.next() {
        Some(p) => p,
        None => {
            tracing::warn!("ZCCACHE_LINK_DEPLOY_CMD is empty — skipping deploy hook");
            return;
        }
    };
    let extra_args: Vec<&str> = parts.collect();

    let mut cmd = std::process::Command::new(program);
    cmd.args(&extra_args);
    cmd.arg(output_path);

    // Run the hook in the output directory so any relative paths the deploy
    // tool emits land sensibly next to the binary.
    if let Some(parent) = output_path.parent() {
        cmd.current_dir(parent);
    }

    // Propagate the client's env — the deploy tool may rely on PATH, TMP,
    // language-specific vars (CLANG_TOOL_CHAIN_*), etc.
    if let Some(env_vars) = env {
        cmd.env_clear();
        for (k, v) in env_vars {
            cmd.env(k, v);
        }
    }

    tracing::debug!(
        program = %program,
        output = %output_path.display(),
        "running post-link deploy hook"
    );

    match cmd.output() {
        Ok(out) if out.status.success() => {
            tracing::debug!(
                program = %program,
                "post-link deploy hook succeeded"
            );
        }
        Ok(out) => {
            tracing::warn!(
                program = %program,
                exit_code = out.status.code().unwrap_or(-1),
                stderr = %String::from_utf8_lossy(&out.stderr),
                "post-link deploy hook exited non-zero"
            );
        }
        Err(e) => {
            tracing::warn!(
                program = %program,
                error = %e,
                "post-link deploy hook failed to start"
            );
        }
    }
}

/// Handle a SessionStart request: create session, watch working directory.
async fn handle_session_start(
    state: &SharedState,
    client_pid: u32,
    working_dir: &Path,
    log_file: Option<NormalizedPath>,
    track_stats: bool,
    journal_path: Option<NormalizedPath>,
) -> Response {
    let session_config = zccache_depgraph::SessionConfig {
        client_pid,
        working_dir: working_dir.into(),
        log_file,
        track_stats,
        journal_path,
    };

    let session_id = state.sessions.create(session_config);

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

/// Build a CompileContext and UserDepFlags from a CacheableCompilation and session info.
/// Result of building a compile context — varies by compiler family.
enum BuildContextResult {
    /// C/C++ compilation (GCC, Clang, MSVC).
    Cc {
        ctx: CompileContext,
        dep_flags: UserDepFlags,
    },
    /// Rustc compilation.
    Rustc {
        /// The Rustc-specific context (for context key computation).
        rustc_ctx: Box<zccache_depgraph::RustcCompileContext>,
        /// A "compatible" CompileContext for dep_graph storage (has source_file).
        compat_ctx: CompileContext,
        /// Parsed args for extern crate info, output path derivation, etc.
        rustc_args: Box<zccache_depgraph::RustcParsedArgs>,
    },
}

fn build_compile_context(
    compilation: &zccache_compiler::CacheableCompilation,
    cwd: &Path,
    system_includes: &[NormalizedPath],
    client_env: &[(String, String)],
) -> BuildContextResult {
    if compilation.family == zccache_compiler::CompilerFamily::Rustc {
        return build_rustc_compile_context(compilation, cwd, client_env);
    }

    // Dispatch to the correct parser based on compiler family.
    let parsed = match compilation.family {
        zccache_compiler::CompilerFamily::Msvc => {
            zccache_depgraph::msvc_args::parse_msvc_args(&compilation.original_args, cwd)
        }
        _ => zccache_depgraph::args::parse_gnu_args(&compilation.original_args, cwd),
    };
    let dep_flags = parsed.dep_flags.clone();
    let mut ctx = CompileContext::from_parsed_args(parsed);

    // For multi-file compilations, the parsed source_file might be wrong
    // (it picks the first source from original_args). Override with the
    // correct per-unit source.
    let source_path = if compilation.source_file.is_absolute() {
        compilation.source_file.clone()
    } else {
        cwd.join(&compilation.source_file).into()
    };
    ctx.source_file = source_path;

    // Inject session's system includes
    for path in system_includes {
        if !ctx.include_search.system.contains(path) {
            ctx.include_search.system.push(path.clone());
        }
    }

    BuildContextResult::Cc { ctx, dep_flags }
}

/// Build compile context for a Rustc invocation.
fn build_rustc_compile_context(
    compilation: &zccache_compiler::CacheableCompilation,
    cwd: &Path,
    client_env: &[(String, String)],
) -> BuildContextResult {
    let rustc_args = zccache_depgraph::parse_rustc_args(&compilation.original_args, cwd);

    // Hash the rustc binary for compiler version identity.
    // Different rustc versions produce different output for the same source.
    let compiler_hash = zccache_hash::hash_file(&compilation.compiler).ok();

    let rustc_ctx = zccache_depgraph::RustcCompileContext::from_parsed_args(
        &rustc_args,
        client_env,
        compiler_hash,
    );

    // Create a "compatible" CompileContext for dep_graph storage.
    // Only source_file is used by the dep_graph for freshness checks.
    let compat_ctx = CompileContext {
        source_file: rustc_args.source_file.clone(),
        include_search: Default::default(),
        defines: Vec::new(),
        flags: Vec::new(),
        force_includes: Vec::new(),
        unknown_flags: Vec::new(),
    };

    BuildContextResult::Rustc {
        rustc_ctx: Box::new(rustc_ctx),
        compat_ctx,
        rustc_args: Box::new(rustc_args),
    }
}

/// Scan rustc dependencies after compilation.
///
/// Parses rustc's dep-info file which has multiple rules (one per output target),
/// all sharing the same dependencies. Extracts the unique set of source file deps.
fn scan_rustc_deps(
    rustc_args: &zccache_depgraph::RustcParsedArgs,
    source_path: &Path,
    cwd: &Path,
) -> zccache_depgraph::ScanResult {
    let mut result = if rustc_args.emit_types.iter().any(|t| t == "dep-info") {
        let name = rustc_args.crate_name.as_deref().unwrap_or("unknown");
        let ext_suffix = rustc_args.extra_filename.as_deref().unwrap_or("");
        let dir = rustc_args.out_dir.as_deref().unwrap_or(cwd);
        let depfile_path = dir.join(format!("{name}{ext_suffix}.d"));
        if depfile_path.exists() {
            if let Ok(content) = std::fs::read_to_string(&depfile_path) {
                parse_rustc_depinfo(&content, source_path, cwd)
            } else {
                zccache_depgraph::ScanResult {
                    resolved: Vec::new(),
                    unresolved: Vec::new(),
                    has_computed: false,
                }
            }
        } else {
            zccache_depgraph::ScanResult {
                resolved: Vec::new(),
                unresolved: Vec::new(),
                has_computed: false,
            }
        }
    } else {
        zccache_depgraph::ScanResult {
            resolved: Vec::new(),
            unresolved: Vec::new(),
            has_computed: false,
        }
    };

    // Add extern crate files as resolved dependencies.
    // Their content hashes will be part of the artifact key,
    // so changing an extern crate causes a cache miss.
    for ext in &rustc_args.externs {
        let dep_path: NormalizedPath = ext.path.clone().into_path_buf().into();
        if ext.path.exists() && !result.resolved.contains(&dep_path) {
            result.resolved.push(dep_path);
        }
    }

    result
}

/// Parse rustc's multi-rule dep-info format.
///
/// Rustc dep-info files contain multiple rules, one per output target:
/// ```text
/// target1.d: src/lib.rs src/util.rs
/// libtarget1.rlib: src/lib.rs src/util.rs
/// libtarget1.rmeta: src/lib.rs src/util.rs
/// src/lib.rs:
/// src/util.rs:
/// ```
///
/// We extract deps from ALL rules and deduplicate, excluding the source file.
fn parse_rustc_depinfo(
    content: &str,
    source_path: &Path,
    cwd: &Path,
) -> zccache_depgraph::ScanResult {
    let mut deps = std::collections::HashSet::new();

    for line in content.lines() {
        // Join continuation lines (backslash-newline)
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        // Find the colon separator (handling Windows drive letters like C:\)
        let colon_pos = if line.len() >= 2
            && line.as_bytes()[1] == b':'
            && line.as_bytes()[0].is_ascii_alphabetic()
        {
            // Skip drive letter colon, find next colon
            line[2..].find(':').map(|p| p + 2)
        } else {
            line.find(':')
        };

        let Some(colon) = colon_pos else { continue };
        let rhs = line[colon + 1..].trim();
        if rhs.is_empty() {
            continue; // "src/lib.rs:" — phony target, skip
        }

        // Split RHS on whitespace, respecting backslash-escaped spaces
        let mut i = 0;
        let bytes = rhs.as_bytes();
        while i < bytes.len() {
            // Skip whitespace
            while i < bytes.len() && bytes[i].is_ascii_whitespace() {
                i += 1;
            }
            if i >= bytes.len() {
                break;
            }

            // Collect a token (backslash-space is an escaped space in the path)
            let start = i;
            while i < bytes.len() && !bytes[i].is_ascii_whitespace() {
                if bytes[i] == b'\\' && i + 1 < bytes.len() {
                    i += 2; // skip escaped char
                } else {
                    i += 1;
                }
            }
            let raw = &rhs[start..i];
            // Unescape backslash-space
            let token = raw.replace("\\ ", " ");
            deps.insert(token);
        }
    }

    // Resolve paths and filter out the source file
    let source_canonical: NormalizedPath = if source_path.is_absolute() {
        source_path.into()
    } else {
        cwd.join(source_path).into()
    };

    let mut resolved = Vec::new();
    for dep in &deps {
        let dep_path = Path::new(dep);
        let abs = if dep_path.is_absolute() {
            dep_path.to_path_buf()
        } else {
            cwd.join(dep_path)
        };
        // Exclude the source file itself
        if abs == source_canonical {
            continue;
        }
        // Only include files that exist (skip phantom deps)
        if abs.exists() {
            resolved.push(abs.into());
        }
    }
    resolved.sort();

    zccache_depgraph::ScanResult {
        resolved,
        unresolved: Vec::new(),
        has_computed: false,
    }
}

/// Collect all output files from a rustc compilation.
///
/// Returns `(primary_output_data, all_outputs)` where `all_outputs` includes
/// the primary output and any additional files (rmeta, dep-info).
fn collect_rustc_outputs(
    rustc_args: &zccache_depgraph::RustcParsedArgs,
    primary_output_path: &Path,
    cwd: &Path,
) -> (Vec<u8>, Vec<(String, Vec<u8>)>) {
    let primary_data = std::fs::read(primary_output_path).unwrap_or_default();
    let primary_name = primary_output_path
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();

    let mut outputs: Vec<(String, Vec<u8>)> = vec![(primary_name, primary_data.clone())];

    // Find additional outputs based on --emit types
    let crate_name = rustc_args.crate_name.as_deref().unwrap_or("unknown");
    let ext_suffix = rustc_args.extra_filename.as_deref().unwrap_or("");
    let dir = rustc_args.out_dir.as_deref().unwrap_or(cwd);

    for emit_type in &rustc_args.emit_types {
        let candidate = match emit_type.as_str() {
            "metadata" => {
                let path = dir.join(format!("lib{crate_name}{ext_suffix}.rmeta"));
                if path != primary_output_path && path.exists() {
                    Some(path)
                } else {
                    None
                }
            }
            "link" => {
                // Could be rlib or staticlib
                let rlib = dir.join(format!("lib{crate_name}{ext_suffix}.rlib"));
                let staticlib = dir.join(format!("lib{crate_name}{ext_suffix}.a"));
                if rlib != primary_output_path && rlib.exists() {
                    Some(rlib)
                } else if staticlib != primary_output_path && staticlib.exists() {
                    Some(staticlib)
                } else {
                    None
                }
            }
            "dep-info" => {
                let path = dir.join(format!("{crate_name}{ext_suffix}.d"));
                if path.exists() {
                    Some(path)
                } else {
                    None
                }
            }
            _ => None,
        };
        if let Some(path) = candidate {
            let name = path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned();
            // Avoid duplicates
            if !outputs.iter().any(|(n, _)| n == &name) {
                if let Ok(data) = std::fs::read(&path) {
                    outputs.push((name, data));
                }
            }
        }
    }

    (primary_data, outputs)
}

/// Write cached output to disk. Optimized syscall sequence:
/// 1. Try hardlink directly (1 syscall — common case when output doesn't exist)
/// 2. If output already exists: check if it's the same file (skip if so)
/// 3. Remove existing output and retry hardlink (2 syscalls)
/// 4. Fall back to fs::write from memory (1 syscall)
///
/// After writing, the output's mtime is set to the current time. This is
/// critical for build system compatibility: cargo, make, and ninja use mtime
/// to determine if an output is fresh relative to its dependencies. Without
/// this, hardlinked outputs inherit the cache file's old mtime, causing
/// build systems to consider them stale and triggering unnecessary rebuilds.
/// See issue #15 for the full root cause analysis.
///
/// The hardlink-first order optimizes for the rebuild scenario where outputs
/// don't exist yet (1 syscall). For incremental builds where outputs exist
/// as hardlinks, the failed hardlink + same_file check is still fast.
fn write_cached_output(out_path: &Path, cache_file: &Path, data: &[u8]) -> std::io::Result<()> {
    // Fast path: hardlink directly (works when out_path doesn't exist yet).
    // This is the cheapest path — one kernel call when no output exists.
    if std::fs::hard_link(cache_file, out_path).is_ok() {
        touch_mtime(out_path);
        return Ok(());
    }
    // Hardlink failed — output probably exists. Check if it's already
    // the same file (hardlinked from a previous hit). Compare file
    // identity (inode/volume+index), NOT file size — two different
    // compilations can produce .o files with identical sizes but
    // different content (alignment, padding).
    if same_file(out_path, cache_file) {
        touch_mtime(out_path);
        return Ok(());
    }
    // Output exists but is different — remove and retry
    let _ = std::fs::remove_file(out_path);
    if std::fs::hard_link(cache_file, out_path).is_ok() {
        touch_mtime(out_path);
        return Ok(());
    }
    // Hardlink failed entirely (cross-device, no cache file) — copy from memory.
    // fs::write creates a new file with current mtime, so no touch needed.
    std::fs::write(out_path, data)
}

/// Set output mtime to current time so build systems (cargo, make, ninja)
/// see the artifact as freshly produced, not stale from the cache file's
/// original compilation time.
fn touch_mtime(path: &Path) {
    let _ = filetime::set_file_mtime(path, filetime::FileTime::now());
}

/// Check if two paths refer to the same file (hardlink check).
///
/// Returns `false` if either file doesn't exist or the check fails.
#[cfg(unix)]
fn same_file(a: &Path, b: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    match (std::fs::metadata(a), std::fs::metadata(b)) {
        (Ok(ma), Ok(mb)) => ma.dev() == mb.dev() && ma.ino() == mb.ino(),
        _ => false,
    }
}

#[cfg(windows)]
fn same_file(a: &Path, b: &Path) -> bool {
    get_file_id(a)
        .zip(get_file_id(b))
        .map(|(ia, ib)| ia == ib)
        .unwrap_or(false)
}

/// Returns (volume_serial, file_index_high, file_index_low) for a path.
#[cfg(windows)]
fn get_file_id(path: &Path) -> Option<(u32, u32, u32)> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION, FILE_ATTRIBUTE_NORMAL,
        FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    };

    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();

    unsafe {
        let handle = CreateFileW(
            wide.as_ptr(),
            0, // no access needed, just metadata
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            std::ptr::null_mut(),
        );
        if handle == windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE {
            return None;
        }

        let mut info: BY_HANDLE_FILE_INFORMATION = std::mem::zeroed();
        let ok = GetFileInformationByHandle(handle, &mut info);
        CloseHandle(handle);

        if ok == 0 {
            return None;
        }

        Some((
            info.dwVolumeSerialNumber,
            info.nFileIndexHigh,
            info.nFileIndexLow,
        ))
    }
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

/// Compute a fast fingerprint of a compile request for the request-level cache.
///
/// Streams bytes directly into blake3 without intermediate buffer allocation.
/// Zero-alloc: ~100ns for 10 args, ~500ns for 300 args.
/// Callers should pass the fully expanded argv so response-file content
/// changes also invalidate the request-level fast path.
fn request_fingerprint(compiler: &Path, args: &[String], cwd: &Path) -> ContentHash {
    let mut h = zccache_hash::StreamHasher::new();
    h.update(b"zccache-request-v1\0");
    let compiler = zccache_core::path::normalize_for_key(compiler);
    h.update(compiler.as_bytes());
    h.update(&[0]);
    for arg in args {
        h.update(arg.as_bytes());
        h.update(&[0]);
    }
    let cwd = zccache_core::path::normalize_for_key(cwd);
    h.update(cwd.as_bytes());
    h.finalize()
}

/// Handle a Compile request: parse args, check depgraph, run compiler or return cached.
async fn handle_compile(
    state_arc: &Arc<SharedState>,
    session_id: &str,
    args: &[String],
    cwd: &Path,
    compiler_path: &Path,
    client_env: Option<Vec<(String, String)>>,
) -> Response {
    let state = state_arc.as_ref();
    let compile_start = std::time::Instant::now();
    let sid = match session_id.parse::<SessionId>() {
        Ok(id) => id,
        Err(_) => {
            return Response::Error {
                message: format!("invalid session ID: {session_id}"),
            };
        }
    };
    // Expand response files before request-level caching so `@file` mutations
    // can't reuse stale fast-hit entries keyed only by raw argv.
    let expanded_args = expand_args_cached(state, args, cwd);

    // ── Ultra-fast request-level cache ────────────────────────────────
    // If we've seen this exact (compiler, args, cwd) before AND the fast-hit
    // cache still holds a valid entry, skip ALL heavy work: system include
    // discovery, watch_directories, response file expansion, arg parsing,
    // context building, and dep_graph registration.
    if state.watcher_active.load(Ordering::Acquire) {
        let request_fp = request_fingerprint(compiler_path, &expanded_args, cwd);
        if let Some(req_entry) = state.request_cache.get(&request_fp) {
            if let Some(fh_entry) = state.fast_hit_cache.get(&req_entry.context_key) {
                if fh_entry.cached_at.elapsed() < FAST_HIT_MAX_AGE
                    && context_files_fresh(
                        state,
                        &req_entry.context_key,
                        &req_entry.source_path,
                        fh_entry.clock,
                    )
                {
                    let artifact_key_hex = &fh_entry.artifact_key_hex;
                    if let Some(mut cached_ref) = state.artifacts.get_mut(artifact_key_hex) {
                        cached_ref.last_used = std::time::Instant::now();
                        let loaded =
                            ensure_payloads(&mut cached_ref, &state.artifact_dir, artifact_key_hex)
                                .is_some();
                        if loaded {
                            let payloads = Arc::clone(cached_ref.payloads.as_ref().unwrap());
                            let names = Arc::clone(&cached_ref.meta.output_names);
                            let exit_code = cached_ref.meta.exit_code;
                            let stdout = cached_ref.stdout.clone();
                            let stderr = cached_ref.stderr.clone();
                            let artifact_bytes: u64 = cached_ref.meta.total_size;
                            // Drop the DashMap reference before doing more work
                            drop(cached_ref);

                            // Write output
                            let mut write_ok = true;
                            for (i, payload) in payloads.iter().enumerate() {
                                let out_path = if i == 0 {
                                    req_entry.output_path.clone()
                                } else {
                                    cwd.join(&names[i]).into()
                                };
                                let cache_file =
                                    state.artifact_dir.join(format!("{artifact_key_hex}_{i}"));
                                if write_cached_output(&out_path, &cache_file, payload).is_err() {
                                    write_ok = false;
                                    break;
                                }
                            }
                            if write_ok {
                                state.stats.record_compilation();
                                let latency_ns = compile_start.elapsed().as_nanos() as u64;
                                state.stats.record_hit(latency_ns, artifact_bytes);
                                let src = req_entry.source_path.clone();
                                record_session_stat(&state.sessions, &sid, move |t| {
                                    t.record_hit(src, latency_ns, artifact_bytes);
                                });

                                return Response::CompileResult {
                                    exit_code,
                                    stdout,
                                    stderr,
                                    cached: true,
                                };
                            }
                        }
                    }
                }
            }
        }
    }

    // Snap the journal clock once so all file hashes in this request see a
    // consistent view (avoids per-file current_clock() syscalls).
    let snap_clock = state.cache_system.current_clock();

    state.stats.record_compilation();

    // Verify session exists
    if !state.sessions.exists(&sid) {
        return Response::Error {
            message: format!("unknown session: {session_id}"),
        };
    }

    let compiler: NormalizedPath = compiler_path.into();

    // Discover system includes for this compiler (cached per compiler path)
    let system_includes = {
        let mut cache = state.system_includes.lock().await;
        cache
            .get_or_discover(&compiler, |c| {
                let disc_args = zccache_depgraph::discovery_args();
                let output = std::process::Command::new(c).args(&disc_args).output();
                match output {
                    Ok(out) => {
                        let stderr = String::from_utf8_lossy(&out.stderr);
                        zccache_depgraph::parse_system_include_output(&stderr)
                    }
                    Err(e) => {
                        tracing::warn!("failed to run compiler for include discovery: {e}");
                        Vec::new()
                    }
                }
            })
            .to_vec()
    };

    // Watch system include directories
    watch_directories(state, &system_includes).await;

    state.sessions.touch(&sid);

    // ── Phase: expand response files + parse args ─────────────────────
    let t0 = std::time::Instant::now();
    let compiler_str = compiler.to_str().unwrap_or("");
    let parsed = zccache_compiler::parse_invocation(compiler_str, &expanded_args);
    let compilation = match parsed {
        zccache_compiler::ParsedInvocation::Cacheable(c) => c,
        zccache_compiler::ParsedInvocation::NonCacheable { reason } => {
            state.stats.record_non_cacheable();
            record_session_stat(&state.sessions, &sid, |t| t.record_non_cacheable());
            write_session_log(&state.sessions, &sid, &format!("non-cacheable: {reason}"));
            // Use raw args — compiler handles @file natively
            return run_compiler_direct(&compiler, args, cwd, &state.sessions, &sid, &client_env)
                .await;
        }
        zccache_compiler::ParsedInvocation::MultiFile {
            compilations,
            original_args,
            source_indices,
        } => {
            return handle_compile_multi(
                Arc::clone(state_arc),
                sid,
                compiler,
                compilations,
                original_args,
                source_indices,
                cwd.into(),
                system_includes,
                client_env,
            )
            .await;
        }
    };
    let parse_args_ns = t0.elapsed().as_nanos() as u64;

    let cwd_path: NormalizedPath = cwd.into();
    let source_path = if compilation.source_file.is_absolute() {
        compilation.source_file.clone()
    } else {
        cwd_path.join(&compilation.source_file)
    };
    let output_path = if compilation.output_file.is_absolute() {
        compilation.output_file.clone()
    } else {
        cwd_path.join(&compilation.output_file)
    };

    // ── Phase: build context + register ──────────────────────────────
    let t1 = std::time::Instant::now();
    let env_slice = client_env.as_deref().unwrap_or(&[]);
    let build_result = build_compile_context(&compilation, &cwd_path, &system_includes, env_slice);
    let (ctx, dep_flags, rustc_args_opt, context_key) = match build_result {
        BuildContextResult::Cc { ctx, dep_flags } => {
            let key = state.dep_graph.register(ctx.clone());
            (ctx, dep_flags, None, key)
        }
        BuildContextResult::Rustc {
            rustc_ctx,
            compat_ctx,
            rustc_args,
        } => {
            let key = rustc_ctx.context_key();
            state.dep_graph.register_with_key(key, compat_ctx.clone());
            (compat_ctx, UserDepFlags::default(), Some(rustc_args), key)
        }
    };
    let is_rustc = rustc_args_opt.is_some();
    let build_context_ns = t1.elapsed().as_nanos() as u64;

    // ── Ultra-fast path: per-file freshness skip ────────────────────
    // If the watcher is active and none of the source/header files have
    // changed since the last verified hit, skip ALL hash/depgraph work.
    // Uses per-file journal checks instead of global clock comparison so
    // output file writes don't invalidate unrelated fast-hit entries.
    if state.watcher_active.load(Ordering::Acquire) {
        if let Some(entry) = state.fast_hit_cache.get(&context_key) {
            if entry.cached_at.elapsed() < FAST_HIT_MAX_AGE
                && context_files_fresh(state, &context_key, &source_path, entry.clock)
            {
                let artifact_key_hex = &entry.artifact_key_hex;
                let t5 = std::time::Instant::now();
                // Write directly from DashMap reference — avoids cloning the
                // entire CachedArtifact (including all .o data, ~50-200KB).
                if let Some(mut cached_ref) = state.artifacts.get_mut(artifact_key_hex) {
                    cached_ref.last_used = std::time::Instant::now();
                    let artifact_lookup_ns = t5.elapsed().as_nanos() as u64;
                    let t6 = std::time::Instant::now();
                    let loaded =
                        ensure_payloads(&mut cached_ref, &state.artifact_dir, artifact_key_hex)
                            .is_some();
                    if !loaded {
                        // Fall through to slow path on payload load failure
                    } else {
                        let payloads = Arc::clone(cached_ref.payloads.as_ref().unwrap());
                        let names = Arc::clone(&cached_ref.meta.output_names);
                        let exit_code = cached_ref.meta.exit_code;
                        let stdout = cached_ref.stdout.clone();
                        let stderr = cached_ref.stderr.clone();
                        let artifact_bytes: u64 = cached_ref.meta.total_size;
                        // Drop the DashMap reference before doing more work
                        drop(cached_ref);

                        let mut write_ok = true;
                        let secondary_dir = if is_rustc {
                            output_path.parent().unwrap_or(&cwd_path).to_path_buf()
                        } else {
                            cwd_path.clone().to_path_buf()
                        };
                        for (i, payload) in payloads.iter().enumerate() {
                            let out_path = if i == 0 {
                                output_path.clone()
                            } else {
                                secondary_dir.join(&names[i]).into()
                            };
                            let cache_file =
                                state.artifact_dir.join(format!("{artifact_key_hex}_{i}"));
                            if write_cached_output(&out_path, &cache_file, payload).is_err() {
                                write_ok = false;
                                break;
                            }
                        }
                        if !write_ok {
                            // Fall through to slow path on write failure
                        } else {
                            let write_output_ns = t6.elapsed().as_nanos() as u64;

                            // Downgrade output metadata (file was re-written) but
                            // DON'T advance the journal clock — the output content is
                            // the same cached artifact, and advancing the global clock
                            // would invalidate fast-hit entries for unrelated source
                            // files in the same batch.
                            state.cache_system.metadata().downgrade(&output_path);

                            let t7 = std::time::Instant::now();
                            let latency_ns = compile_start.elapsed().as_nanos() as u64;
                            state.stats.record_hit(latency_ns, artifact_bytes);
                            let src = source_path.clone();
                            record_session_stat(&state.sessions, &sid, move |t| {
                                t.record_hit(src, latency_ns, artifact_bytes);
                            });
                            write_session_log(
                                &state.sessions,
                                &sid,
                                &format!(
                                    "[HIT_FAST] {} -> {}",
                                    source_path.display(),
                                    output_path.display()
                                ),
                            );
                            let bookkeeping_ns = t7.elapsed().as_nanos() as u64;

                            let rfp = request_fingerprint(compiler_path, &expanded_args, cwd);
                            state.request_cache.insert(
                                rfp,
                                RequestCacheEntry {
                                    context_key,
                                    source_path: source_path.clone(),
                                    output_path: output_path.clone(),
                                },
                            );

                            let total_ns = compile_start.elapsed().as_nanos() as u64;
                            state.profiler.record_hit(&HitPhases {
                                parse_args_ns,
                                build_context_ns,
                                hash_source_ns: 0,
                                hash_headers_ns: 0,
                                depgraph_check_ns: 0,
                                artifact_lookup_ns,
                                write_output_ns,
                                bookkeeping_ns,
                                total_ns,
                            });

                            return Response::CompileResult {
                                exit_code,
                                stdout,
                                stderr,
                                cached: true,
                            };
                        }
                    }
                }
            }
        }
    }

    // ── Slow path: hash + depgraph verify ────────────────────────────

    // Skip pre-compile hashing for cold contexts — the depgraph would
    // return Cold without examining any hashes, so the work is wasted.
    // Jump straight to compiler exec.
    let context_is_cold = state.dep_graph.is_cold(&context_key);

    // ── Phase: hash source ───────────────────────────────────────────
    let t2 = std::time::Instant::now();
    let mut hash_map: HashMap<NormalizedPath, ContentHash> = HashMap::new();
    if !context_is_cold {
        match hash_file(&state.cache_system, &source_path, snap_clock) {
            Ok(h) => {
                hash_map.insert(source_path.clone(), h);
            }
            Err(e) => {
                write_session_log(
                    &state.sessions,
                    &sid,
                    &format!("cache key error: {e}, falling back to direct compile"),
                );
                return run_compiler_direct(
                    &compiler,
                    args,
                    cwd,
                    &state.sessions,
                    &sid,
                    &client_env,
                )
                .await;
            }
        }
    }
    let hash_source_ns = t2.elapsed().as_nanos() as u64;

    // ── Phase: hash headers + depgraph check ────────────────────────
    let t3 = std::time::Instant::now();
    let hash_headers_ns;
    let depgraph_check_ns;
    let verdict;
    let diag_reason;

    if context_is_cold {
        // Cold context — skip hashing and depgraph check entirely.
        hash_headers_ns = 0;
        depgraph_check_ns = 0;
        verdict = zccache_depgraph::CacheVerdict::Cold;
        diag_reason = "cold_skip".to_string();
    } else {
        // Hash includes + force-includes in parallel (PCH-aware).
        {
            use rayon::prelude::*;
            let includes = state.dep_graph.get_includes(&context_key);
            let include_iter = includes
                .iter()
                .flat_map(|v| v.iter().map(|h| (h, "header_hash_fail")));
            let force_iter = ctx
                .force_includes
                .iter()
                .map(|h| (h, "force_include_hash_fail"));
            let all_paths: Vec<_> = include_iter.chain(force_iter).collect();

            let results: Vec<_> = all_paths
                .par_iter()
                .map(|(header, label)| {
                    let hash_path = resolve_pch_source(header, &state.pch_source_map)
                        .unwrap_or_else(|| (*header).clone());
                    let result = hash_file(&state.cache_system, &hash_path, snap_clock);
                    ((*header).clone(), hash_path, result, *label)
                })
                .collect();

            for (header, hash_path, result, label) in results {
                match result {
                    Ok(h) => {
                        hash_map.insert(header, h);
                    }
                    Err(e) => {
                        write_session_log(
                            &state.sessions,
                            &sid,
                            &format!("[DIAG] {label}: {} error={e}", hash_path.display()),
                        );
                    }
                }
            }
        }
        hash_headers_ns = t3.elapsed().as_nanos() as u64;

        // ── Phase: depgraph check ────────────────────────────────────
        // Fast path: recompute artifact key from fresh hashes and compare
        // with the stored key.  Skips redundant journal freshness checks
        // and path clones that check_diagnostic performs.
        if let Some(artifact_key) = state.dep_graph.try_fast_hit(&context_key, |p| {
            let path = NormalizedPath::new(p);
            hash_map.get(&path).copied()
        }) {
            depgraph_check_ns = 0;
            verdict = zccache_depgraph::CacheVerdict::Hit { artifact_key };
            diag_reason = "fast_key_match".to_string();
        } else {
            let t4 = std::time::Instant::now();
            let result = {
                let is_fresh = |p: &Path| {
                    let path = NormalizedPath::new(p);
                    !state
                        .cache_system
                        .journal()
                        .changed_since(&path, snap_clock)
                };
                let get_hash = |p: &Path| {
                    let path = NormalizedPath::new(p);
                    hash_map.get(&path).copied()
                };
                state
                    .dep_graph
                    .check_diagnostic(&context_key, is_fresh, get_hash)
            };
            depgraph_check_ns = t4.elapsed().as_nanos() as u64;
            verdict = result.0;
            diag_reason = result.1;
        }
    }

    write_session_log(
        &state.sessions,
        &sid,
        &format!(
            "[DIAG] depgraph_check: {} -> {} ctx={} verdict={} reason={}",
            source_path.display(),
            output_path.display(),
            &context_key.hash().to_hex()[..8],
            match &verdict {
                zccache_depgraph::CacheVerdict::Hit { .. } => "Hit",
                zccache_depgraph::CacheVerdict::SourceChanged { .. } => "SourceChanged",
                zccache_depgraph::CacheVerdict::HeadersChanged { .. } => "HeadersChanged",
                zccache_depgraph::CacheVerdict::Cold => "Cold",
                zccache_depgraph::CacheVerdict::NeedsPreprocessor => "NeedsPreprocessor",
            },
            diag_reason,
        ),
    );
    match verdict {
        zccache_depgraph::CacheVerdict::Hit { artifact_key }
        | zccache_depgraph::CacheVerdict::SourceChanged { artifact_key } => {
            // ── Phase: artifact lookup + write ─────────────────────────
            let t5 = std::time::Instant::now();
            let artifact_key_hex = artifact_key.hash().to_hex();
            if let Some(mut cached_ref) = state.artifacts.get_mut(&artifact_key_hex) {
                cached_ref.last_used = std::time::Instant::now();
                let artifact_lookup_ns = t5.elapsed().as_nanos() as u64;

                let t6 = std::time::Instant::now();
                let loaded =
                    ensure_payloads(&mut cached_ref, &state.artifact_dir, &artifact_key_hex)
                        .is_some();
                if !loaded {
                    // Fall through to compile on payload load failure
                } else {
                    let payloads = Arc::clone(cached_ref.payloads.as_ref().unwrap());
                    let names = Arc::clone(&cached_ref.meta.output_names);
                    let exit_code = cached_ref.meta.exit_code;
                    let stdout = cached_ref.stdout.clone();
                    let stderr = cached_ref.stderr.clone();
                    let artifact_bytes: u64 = cached_ref.meta.total_size;
                    drop(cached_ref);

                    let mut write_ok = true;
                    let secondary_dir = if is_rustc {
                        output_path.parent().unwrap_or(&cwd_path).to_path_buf()
                    } else {
                        cwd_path.clone().to_path_buf()
                    };
                    for (i, payload) in payloads.iter().enumerate() {
                        let out_path = if i == 0 {
                            output_path.clone()
                        } else {
                            secondary_dir.join(&names[i]).into()
                        };
                        let cache_file = state.artifact_dir.join(format!("{artifact_key_hex}_{i}"));
                        if write_cached_output(&out_path, &cache_file, payload).is_err() {
                            write_ok = false;
                            break;
                        }
                    }
                    if !write_ok {
                        // Fall through to compile on write failure
                    } else {
                        let write_output_ns = t6.elapsed().as_nanos() as u64;

                        // Downgrade output metadata but don't advance journal
                        // clock — same cached artifact content, advancing would
                        // invalidate fast-hit entries for other source files.
                        state.cache_system.metadata().downgrade(&output_path);

                        // ── Phase: bookkeeping ───────────────────────────────
                        let t7 = std::time::Instant::now();
                        let latency_ns = compile_start.elapsed().as_nanos() as u64;
                        state.stats.record_hit(latency_ns, artifact_bytes);
                        let src = source_path.clone();
                        record_session_stat(&state.sessions, &sid, move |t| {
                            t.record_hit(src, latency_ns, artifact_bytes);
                        });
                        write_session_log(
                            &state.sessions,
                            &sid,
                            &format!(
                                "[HIT] {} -> {}",
                                source_path.display(),
                                output_path.display()
                            ),
                        );
                        let bookkeeping_ns = t7.elapsed().as_nanos() as u64;

                        // Populate fast-hit cache for future requests
                        let current_clock = state.cache_system.current_clock();
                        state.fast_hit_cache.insert(
                            context_key,
                            FastHitEntry {
                                clock: current_clock,
                                artifact_key_hex: artifact_key_hex.clone(),
                                cached_at: std::time::Instant::now(),
                            },
                        );

                        let rfp = request_fingerprint(compiler_path, &expanded_args, cwd);
                        state.request_cache.insert(
                            rfp,
                            RequestCacheEntry {
                                context_key,
                                source_path: source_path.clone(),
                                output_path: output_path.clone(),
                            },
                        );

                        // Record phase profile
                        let total_ns = compile_start.elapsed().as_nanos() as u64;
                        state.profiler.record_hit(&HitPhases {
                            parse_args_ns,
                            build_context_ns,
                            hash_source_ns,
                            hash_headers_ns,
                            depgraph_check_ns,
                            artifact_lookup_ns,
                            write_output_ns,
                            bookkeeping_ns,
                            total_ns,
                        });

                        return Response::CompileResult {
                            exit_code,
                            stdout,
                            stderr,
                            cached: true,
                        };
                    }
                }
            }
            // Artifact key computed but no artifact stored yet — fall through to compile
            write_session_log(
                &state.sessions,
                &sid,
                &format!("[DIAG] artifact_not_found: key={artifact_key_hex}"),
            );
        }
        zccache_depgraph::CacheVerdict::Cold
        | zccache_depgraph::CacheVerdict::HeadersChanged { .. }
        | zccache_depgraph::CacheVerdict::NeedsPreprocessor => {
            // Need to compile and scan includes
        }
    }

    // Cache miss — invalidate fast-hit cache for this context
    state.fast_hit_cache.remove(&context_key);

    // Cache miss — run the compiler
    write_session_log(
        &state.sessions,
        &sid,
        &format!(
            "[MISS] {} -> {} (reason: {diag_reason})",
            source_path.display(),
            output_path.display()
        ),
    );

    // ── Phase: compiler exec (with depfile injection) ────────────────
    let t_exec = std::time::Instant::now();
    let supports_depfile = compilation.family.supports_depfile();
    let (mut extra_args, mut depfile_strategy) = zccache_depgraph::depfile::prepare_depfile(
        supports_depfile,
        &dep_flags,
        &output_path,
        &state.depfile_tmpdir,
    );

    // For MSVC, use /showIncludes to get complete dependency info
    // (equivalent to depfiles for gcc/clang). This enables cache hits
    // for files with computed includes like `#include MACRO`.
    if compilation.family == zccache_compiler::CompilerFamily::Msvc
        && depfile_strategy == DepfileStrategy::Unsupported
    {
        if !dep_flags.has_md {
            extra_args.push("/showIncludes".to_string());
        }
        depfile_strategy = DepfileStrategy::ShowIncludes;
    }

    // Combine expanded_args + extra_args for response-file length check.
    // Only allocates when extra_args is non-empty.
    let combined_args;
    let rsp_args: &[String] = if extra_args.is_empty() {
        &expanded_args
    } else {
        combined_args = [expanded_args.as_slice(), extra_args.as_slice()].concat();
        &combined_args
    };

    let _rsp_guard = match zccache_compiler::response_file::write_response_file_if_needed(
        rsp_args,
        &state.depfile_tmpdir,
    ) {
        Ok(guard) => guard,
        Err(e) => {
            return Response::Error {
                message: format!("failed to write response file: {e}"),
            };
        }
    };

    let mut cmd = tokio::process::Command::new(&compiler);
    if let Some(ref rsp) = _rsp_guard {
        cmd.arg(rsp.at_arg()).current_dir(cwd);
    } else {
        cmd.args(&expanded_args).current_dir(cwd);
        if !extra_args.is_empty() {
            cmd.args(&extra_args);
        }
    }
    apply_client_env(&mut cmd, &client_env);
    let result = cmd.output().await;

    let output = match result {
        Ok(o) => o,
        Err(e) => {
            return Response::Error {
                message: format!("failed to run compiler: {e}"),
            };
        }
    };
    let compiler_exec_ns = t_exec.elapsed().as_nanos() as u64;

    let exit_code = output.status.code().unwrap_or(-1);
    let stdout = Arc::new(output.stdout);

    // For MSVC /showIncludes: parse dependency info from stderr and
    // filter out the /showIncludes lines before returning to the client.
    let (show_includes_scan, stderr_bytes) = if depfile_strategy == DepfileStrategy::ShowIncludes {
        let (scan, filtered) = zccache_depgraph::show_includes::parse_show_includes(
            &output.stderr,
            &source_path,
            &cwd_path,
        );
        (Some(scan), filtered)
    } else {
        (None, output.stderr)
    };
    let stderr = Arc::new(stderr_bytes);

    if exit_code != 0 {
        state.stats.record_error();
        record_session_stat(&state.sessions, &sid, |t| t.record_error());
    }

    // Only cache successful compilations
    if exit_code == 0 {
        // The compiler just wrote the output file. Invalidate it in the
        // cache system so any compilation that depends on this output
        // (e.g. via -include-pch) sees the change immediately — no need
        // to wait for a watcher event.
        state.cache_system.apply_changes(vec![output_path.clone()]);

        // Read the output file(s)
        let (output_data, rustc_all_outputs) = if is_rustc {
            let (primary, all) =
                collect_rustc_outputs(rustc_args_opt.as_ref().unwrap(), &output_path, &cwd_path);
            if primary.is_empty() {
                tracing::warn!("failed to read output file {}", output_path.display());
                return Response::CompileResult {
                    exit_code,
                    stdout: Arc::clone(&stdout),
                    stderr: Arc::clone(&stderr),
                    cached: false,
                };
            }
            (primary, Some(all))
        } else {
            match std::fs::read(&output_path) {
                Ok(data) => (data, None),
                Err(e) => {
                    tracing::warn!("failed to read output file {}: {e}", output_path.display());
                    return Response::CompileResult {
                        exit_code,
                        stdout: Arc::clone(&stdout),
                        stderr: Arc::clone(&stderr),
                        cached: false,
                    };
                }
            }
        };

        // ── Phase: include scan (depfile or fallback) ────────────────
        let t_scan = std::time::Instant::now();
        let scan_result = if is_rustc {
            // Rustc: try to parse the dep-info file if --emit included dep-info.
            // The dep-info file is in --out-dir with crate name and extra-filename.
            scan_rustc_deps(rustc_args_opt.as_ref().unwrap(), &source_path, &cwd_path)
        } else {
            match &depfile_strategy {
                DepfileStrategy::Injected { path }
                | DepfileStrategy::UserSpecified { path }
                | DepfileStrategy::UserDefault { path } => {
                    let cwd_path: NormalizedPath = cwd.into();
                    match zccache_depgraph::depfile::parse_depfile_path(
                        path,
                        &source_path,
                        &cwd_path,
                    ) {
                        Ok(result) => {
                            if matches!(depfile_strategy, DepfileStrategy::Injected { .. }) {
                                let _ = std::fs::remove_file(path);
                            }
                            result
                        }
                        Err(e) => {
                            tracing::warn!("depfile parse failed, falling back to scanner: {e}");
                            write_session_log(
                                &state.sessions,
                                &sid,
                                &format!(
                                    "[DIAG] depfile_parse_fail: path={} error={e}",
                                    path.display()
                                ),
                            );
                            if matches!(depfile_strategy, DepfileStrategy::Injected { .. }) {
                                let _ = std::fs::remove_file(path);
                            }
                            zccache_depgraph::scanner::scan_recursive(
                                &source_path,
                                &ctx.include_search,
                            )
                        }
                    }
                }
                DepfileStrategy::ShowIncludes => {
                    // Already parsed from stderr above.
                    show_includes_scan.unwrap_or_else(|| {
                        zccache_depgraph::scanner::scan_recursive(&source_path, &ctx.include_search)
                    })
                }
                DepfileStrategy::Unsupported => {
                    zccache_depgraph::scanner::scan_recursive(&source_path, &ctx.include_search)
                }
            }
        };
        let include_scan_ns = t_scan.elapsed().as_nanos() as u64;

        // Register scanned paths for zero-syscall fast path on future hits.
        let tracked_paths: Vec<NormalizedPath> = std::iter::once(source_path.clone())
            .chain(scan_result.resolved.iter().cloned())
            .chain(ctx.force_includes.iter().cloned())
            .collect();
        state.cache_system.register_tracked(&tracked_paths);

        // Collect directories to watch. The actual watch_directories call
        // (which involves expensive canonicalize() on Windows) is deferred
        // to a background task to avoid blocking the response.
        let dep_dirs: Vec<NormalizedPath> = {
            let mut dirs = HashSet::new();
            if let Some(parent) = source_path.parent() {
                dirs.insert(parent.into());
            }
            for header in &scan_result.resolved {
                if let Some(parent) = header.parent() {
                    dirs.insert(parent.into());
                }
            }
            // Also watch force-include parent dirs (PCH files, etc.).
            for fi in &ctx.force_includes {
                if let Some(parent) = fi.parent() {
                    dirs.insert(parent.into());
                }
            }
            dirs.into_iter().collect()
        };

        // ── Phase: hash all files (parallel) ─────────────────────────
        // Hash source + resolved headers + force-includes using rayon
        // parallel iteration, matching the hit path's parallel strategy.
        let t_hash = std::time::Instant::now();
        let mut hash_map: HashMap<NormalizedPath, ContentHash> = HashMap::new();
        {
            use rayon::prelude::*;
            let header_iter = scan_result.resolved.iter().chain(ctx.force_includes.iter());
            let all_paths: Vec<&NormalizedPath> =
                std::iter::once(&source_path).chain(header_iter).collect();

            let results: Vec<_> = all_paths
                .par_iter()
                .map(|path| {
                    let hash_path = resolve_pch_source(path, &state.pch_source_map)
                        .unwrap_or_else(|| (*path).clone());
                    let result = hash_file(&state.cache_system, &hash_path, snap_clock);
                    ((*path).clone(), result)
                })
                .collect();

            let mut hash_failures: u32 = 0;
            for (path, result) in results {
                match result {
                    Ok(h) => {
                        hash_map.insert(path, h);
                    }
                    Err(_) => {
                        hash_failures += 1;
                    }
                }
            }
            if hash_failures > 0 {
                write_session_log(
                    &state.sessions,
                    &sid,
                    &format!(
                        "[DIAG] hash_failures: {} of {} files failed to hash for {}",
                        hash_failures,
                        1 + scan_result.resolved.len() + ctx.force_includes.len(),
                        source_path.display(),
                    ),
                );
            }
        }
        let hash_all_ns = t_hash.elapsed().as_nanos() as u64;

        // ── Phase: store artifact ────────────────────────────────────
        let t_store = std::time::Instant::now();
        let get_hash = |p: &Path| {
            let path = NormalizedPath::new(p);
            hash_map.get(&path).copied()
        };
        let include_count = scan_result.resolved.len();
        if let Some(artifact_key) = state.dep_graph.update(&context_key, scan_result, get_hash) {
            let artifact_key_hex = artifact_key.hash().to_hex();
            let ctx_hex = &context_key.hash().to_hex()[..8];
            write_session_log(
                &state.sessions,
                &sid,
                &format!(
                    "[DIAG] update: {} ctx={ctx_hex} artifact_key={} includes={include_count}",
                    source_path.display(),
                    &artifact_key_hex[..8],
                ),
            );

            // Record PCH source mapping so consuming compilations can hash
            // the source header instead of the non-deterministic PCH binary.
            if let Some(ext) = output_path.extension() {
                if ext == "pch" || ext == "gch" {
                    state
                        .pch_source_map
                        .insert(output_path.clone(), source_path.clone());
                }
            }

            // Build artifact — multi-output for Rustc, single output for C/C++.
            let artifact = if let Some(ref all_outputs) = rustc_all_outputs {
                ArtifactData {
                    outputs: all_outputs
                        .iter()
                        .map(|(name, data)| ArtifactOutput {
                            name: name.clone(),
                            data: Arc::new(data.clone()),
                        })
                        .collect(),
                    stdout: Arc::clone(&stdout),
                    stderr: Arc::clone(&stderr),
                    exit_code,
                }
            } else {
                ArtifactData {
                    outputs: vec![ArtifactOutput {
                        name: output_path
                            .file_name()
                            .unwrap_or_default()
                            .to_string_lossy()
                            .into_owned(),
                        data: Arc::new(output_data),
                    }],
                    stdout: Arc::clone(&stdout),
                    stderr: Arc::clone(&stderr),
                    exit_code,
                }
            };

            let artifact_bytes: u64 = artifact.outputs.iter().map(|o| o.data.len() as u64).sum();

            // Build CachedArtifact once (no deep copies — all Arc clones).
            let cached = CachedArtifact::from_artifact_data(&artifact);

            // Spawn disk persistence to background (meta.clone() is cheap — Arc fields only).
            {
                let artifact_dir = state.artifact_dir.clone();
                let key_hex = artifact_key_hex.clone();
                let persist_meta = cached.meta.clone();
                let payloads: Vec<Arc<Vec<u8>>> = artifact
                    .outputs
                    .iter()
                    .map(|o| Arc::clone(&o.data))
                    .collect();
                let payload_size: usize = payloads.iter().map(|p| p.len()).sum();
                state
                    .in_flight_bytes
                    .fetch_add(payload_size, Ordering::Relaxed);
                let guard = InFlightGuard {
                    state: Arc::clone(state_arc),
                    size: payload_size,
                };
                let sem = Arc::clone(&state.persist_semaphore);
                let state_ref = Arc::clone(state_arc);
                tokio::spawn(async move {
                    let _permit = sem.acquire().await.unwrap();
                    tokio::task::spawn_blocking(move || {
                        let _guard = guard;
                        for (i, payload) in payloads.iter().enumerate() {
                            let cache_path = artifact_dir.join(format!("{key_hex}_{i}"));
                            if let Err(e) = std::fs::write(&cache_path, &**payload) {
                                tracing::warn!(
                                    path = %cache_path.display(),
                                    "failed to persist artifact output: {e}"
                                );
                            }
                        }
                        state_ref
                            .artifact_store
                            .insert(&key_hex, &persist_meta)
                            .ok();
                    })
                    .await
                    .ok();
                });
            }

            state.artifacts.insert(artifact_key_hex, cached);

            let latency_ns = compile_start.elapsed().as_nanos() as u64;
            state.stats.record_miss(latency_ns, artifact_bytes);
            let src = source_path.clone();
            record_session_stat(&state.sessions, &sid, move |t| {
                t.record_miss(src, artifact_bytes);
            });
        }
        let artifact_store_ns = t_store.elapsed().as_nanos() as u64;

        // Record miss phase profile
        let total_ns = compile_start.elapsed().as_nanos() as u64;
        state.profiler.record_miss(&MissPhases {
            compiler_exec_ns,
            include_scan_ns,
            hash_all_ns,
            artifact_store_ns,
            total_ns,
        });

        // Defer expensive watch_directories to background — canonicalize()
        // on Windows costs ~1-5ms per directory. This doesn't affect cache
        // correctness; it only delays watcher-based invalidation setup.
        {
            let bg_state = Arc::clone(state_arc);
            tokio::spawn(async move {
                let state = &*bg_state;
                watch_directories(state, &dep_dirs).await;
                if let Some(out_dir) = output_path.parent() {
                    watch_directory(state, out_dir).await;
                }
                state.cache_system.apply_changes(vec![output_path]);
            });
        }
    }

    Response::CompileResult {
        exit_code,
        stdout,
        stderr,
        cached: false,
    }
}

/// Apply client environment variables to a compiler command.
/// If `client_env` is `Some`, clears the inherited env and sets only the client's vars.
fn apply_client_env(cmd: &mut tokio::process::Command, client_env: &Option<Vec<(String, String)>>) {
    if let Some(vars) = client_env {
        cmd.env_clear();
        for (key, val) in vars {
            cmd.env(key, val);
        }
    }
}

/// A deferred output write for a cache hit.
struct PendingWrite {
    out_path: NormalizedPath,
    cache_file: NormalizedPath,
    data: Vec<u8>,
}

/// Result of a per-unit cache check in multi-file compile.
enum UnitCacheResult {
    /// Cache hit — output write is deferred for batching.
    Hit {
        stdout: Arc<Vec<u8>>,
        stderr: Arc<Vec<u8>>,
        artifact_bytes: u64,
        source_path: NormalizedPath,
        pending_writes: Vec<PendingWrite>,
    },
    /// Cache miss — needs compilation.
    Miss {
        source_path: NormalizedPath,
        output_path: NormalizedPath,
        context_key: ContextKey,
        ctx: Box<CompileContext>,
    },
}

/// Check cache for a single compilation unit. Returns Hit (output written) or Miss.
///
/// If `shared_base` is provided, the CompileContext is built by cloning it and
/// overriding the source_file, avoiding redundant arg parsing for multi-file
/// compilations where all units share the same flags.
fn check_unit_cache(
    state: &SharedState,
    compilation: &zccache_compiler::CacheableCompilation,
    cwd_path: &Path,
    system_includes: &[NormalizedPath],
    shared_base: Option<&CompileContext>,
) -> UnitCacheResult {
    let t0 = std::time::Instant::now();
    let snap_clock = state.cache_system.current_clock();
    state.stats.record_compilation();

    let source_path = if compilation.source_file.is_absolute() {
        compilation.source_file.clone()
    } else {
        cwd_path.join(&compilation.source_file).into()
    };
    let output_path = if compilation.output_file.is_absolute() {
        compilation.output_file.clone()
    } else {
        cwd_path.join(&compilation.output_file).into()
    };

    let (ctx, _dep_flags) = if let Some(base) = shared_base {
        let mut ctx = base.clone();
        ctx.source_file = source_path.clone();
        (
            ctx,
            UserDepFlags {
                has_md: false,
                mf_path: None,
            },
        )
    } else {
        match build_compile_context(compilation, cwd_path, system_includes, &[]) {
            BuildContextResult::Cc { ctx, dep_flags } => (ctx, dep_flags),
            BuildContextResult::Rustc { compat_ctx, .. } => (compat_ctx, UserDepFlags::default()),
        }
    };
    let t_ctx = t0.elapsed();
    let context_key = state.dep_graph.register(ctx.clone());
    let t_register = t0.elapsed();

    // ── Ultra-fast path: per-file freshness skip ────────────────────
    // If the watcher is active and none of the source/header files have
    // changed since the last verified hit, skip ALL hash/depgraph work.
    if state.watcher_active.load(Ordering::Acquire) {
        if let Some(entry) = state.fast_hit_cache.get(&context_key) {
            if entry.cached_at.elapsed() < FAST_HIT_MAX_AGE
                && context_files_fresh(state, &context_key, &source_path, entry.clock)
            {
                let artifact_key_hex = &entry.artifact_key_hex;
                // Write outputs directly from DashMap reference — eliminates
                // cloning all .o data (~50-200KB per file) into PendingWrite.
                // Each check_unit_cache runs in its own spawn_blocking task,
                // so writes are already parallel across units.
                if let Some(mut cached_ref) = state.artifacts.get_mut(artifact_key_hex) {
                    cached_ref.last_used = std::time::Instant::now();
                    let loaded =
                        ensure_payloads(&mut cached_ref, &state.artifact_dir, artifact_key_hex)
                            .is_some();
                    if loaded {
                        let payloads = Arc::clone(cached_ref.payloads.as_ref().unwrap());
                        let names = Arc::clone(&cached_ref.meta.output_names);
                        let artifact_bytes: u64 = cached_ref.meta.total_size;
                        let stdout = cached_ref.stdout.clone();
                        let stderr = cached_ref.stderr.clone();
                        drop(cached_ref);

                        for (i, payload) in payloads.iter().enumerate() {
                            let out_path = if i == 0 {
                                output_path.clone()
                            } else {
                                cwd_path.join(&names[i]).into()
                            };
                            let cache_file =
                                state.artifact_dir.join(format!("{artifact_key_hex}_{i}"));
                            let _ = write_cached_output(&out_path, &cache_file, payload);
                        }

                        state.stats.record_hit(0, artifact_bytes);
                        state.profiler.record_hit(&HitPhases {
                            parse_args_ns: 0,
                            build_context_ns: t_ctx.as_nanos() as u64,
                            hash_source_ns: 0,
                            hash_headers_ns: 0,
                            depgraph_check_ns: 0,
                            artifact_lookup_ns: 0,
                            write_output_ns: 0,
                            bookkeeping_ns: 0,
                            total_ns: t0.elapsed().as_nanos() as u64,
                        });
                        return UnitCacheResult::Hit {
                            stdout,
                            stderr,
                            artifact_bytes,
                            source_path,
                            pending_writes: Vec::new(),
                        };
                    }
                }
            }
        }
    }

    // Hash source
    let source_hash = match hash_file(&state.cache_system, &source_path, snap_clock) {
        Ok(h) => h,
        Err(_) => {
            return UnitCacheResult::Miss {
                source_path,
                output_path,
                context_key,
                ctx: Box::new(ctx),
            };
        }
    };
    let t_hash_source = t0.elapsed();

    // Hash known headers + force-includes in parallel
    let mut hash_map: HashMap<NormalizedPath, ContentHash> = HashMap::new();
    hash_map.insert(source_path.clone(), source_hash);
    {
        use rayon::prelude::*;
        let includes = state.dep_graph.get_includes(&context_key);
        let include_iter = includes.iter().flat_map(|v| v.iter());
        let all_paths: Vec<&NormalizedPath> =
            include_iter.chain(ctx.force_includes.iter()).collect();
        let hashes: Vec<_> = all_paths
            .par_iter()
            .filter_map(|path| {
                hash_file(&state.cache_system, path, snap_clock)
                    .ok()
                    .map(|h| ((*path).clone(), h))
            })
            .collect();
        for (path, h) in hashes {
            hash_map.insert(path, h);
        }
    }
    let t_hash_headers = t0.elapsed();

    // Depgraph check
    let verdict = {
        let is_fresh = |p: &Path| {
            let path = NormalizedPath::new(p);
            !state
                .cache_system
                .journal()
                .changed_since(&path, snap_clock)
        };
        let get_hash = |p: &Path| {
            let path = NormalizedPath::new(p);
            hash_map.get(&path).copied()
        };
        state.dep_graph.check(&context_key, is_fresh, get_hash)
    };
    let t_depgraph = t0.elapsed();

    // Try to serve from cache
    if let zccache_depgraph::CacheVerdict::Hit { artifact_key }
    | zccache_depgraph::CacheVerdict::SourceChanged { artifact_key } = verdict
    {
        let artifact_key_hex = artifact_key.hash().to_hex();
        if let Some(mut cached_ref) = state.artifacts.get_mut(&artifact_key_hex) {
            cached_ref.last_used = std::time::Instant::now();
            let t_lookup = t0.elapsed();
            let loaded =
                ensure_payloads(&mut cached_ref, &state.artifact_dir, &artifact_key_hex).is_some();
            if loaded {
                let payloads = Arc::clone(cached_ref.payloads.as_ref().unwrap());
                let names = Arc::clone(&cached_ref.meta.output_names);
                let artifact_bytes: u64 = cached_ref.meta.total_size;
                let stdout = cached_ref.stdout.clone();
                let stderr = cached_ref.stderr.clone();
                drop(cached_ref);

                for (i, payload) in payloads.iter().enumerate() {
                    let out_path = if i == 0 {
                        output_path.clone()
                    } else {
                        cwd_path.join(&names[i]).into()
                    };
                    let cache_file = state.artifact_dir.join(format!("{artifact_key_hex}_{i}"));
                    let _ = write_cached_output(&out_path, &cache_file, payload);
                }

                state.stats.record_hit(0, artifact_bytes);

                // Populate fast-hit cache for future requests
                let current_clock = state.cache_system.current_clock();
                state.fast_hit_cache.insert(
                    context_key,
                    FastHitEntry {
                        clock: current_clock,
                        artifact_key_hex: artifact_key_hex.clone(),
                        cached_at: std::time::Instant::now(),
                    },
                );

                let total_ns = t0.elapsed().as_nanos() as u64;
                state.profiler.record_hit(&HitPhases {
                    parse_args_ns: 0,
                    build_context_ns: t_ctx.as_nanos() as u64,
                    hash_source_ns: (t_hash_source - t_register).as_nanos() as u64,
                    hash_headers_ns: (t_hash_headers - t_hash_source).as_nanos() as u64,
                    depgraph_check_ns: (t_depgraph - t_hash_headers).as_nanos() as u64,
                    artifact_lookup_ns: (t_lookup - t_depgraph).as_nanos() as u64,
                    write_output_ns: 0,
                    bookkeeping_ns: 0,
                    total_ns,
                });

                return UnitCacheResult::Hit {
                    stdout,
                    stderr,
                    artifact_bytes,
                    source_path,
                    pending_writes: Vec::new(),
                };
            }
        }
    }

    state.fast_hit_cache.remove(&context_key);
    UnitCacheResult::Miss {
        source_path,
        output_path,
        context_key,
        ctx: Box::new(ctx),
    }
}

/// Handle a multi-file compile: check cache per-unit in parallel, serve hits, batch misses.
#[allow(clippy::too_many_arguments)]
async fn handle_compile_multi(
    state: Arc<SharedState>,
    sid: SessionId,
    compiler: NormalizedPath,
    compilations: Vec<zccache_compiler::CacheableCompilation>,
    original_args: Arc<[String]>,
    source_indices: Vec<usize>,
    cwd_path: NormalizedPath,
    system_includes: Vec<NormalizedPath>,
    client_env: Option<Vec<(String, String)>>,
) -> Response {
    let snap_clock = state.cache_system.current_clock();
    let mut all_stdout = Vec::new();
    let mut all_stderr = Vec::new();

    // ── Pre-parse shared args once for all units ─────────────────────
    // All units share the same original_args (via Arc) — only source/output
    // differ. Parse the flags once and reuse the base CompileContext, avoiding
    // redundant arg parsing for each of the N compilation units.
    let shared_base: Arc<CompileContext> = {
        let first = &compilations[0];
        let parsed = match first.family {
            zccache_compiler::CompilerFamily::Msvc => {
                zccache_depgraph::msvc_args::parse_msvc_args(&first.original_args, &cwd_path)
            }
            _ => zccache_depgraph::args::parse_gnu_args(&first.original_args, &cwd_path),
        };
        let mut base = CompileContext::from_parsed_args(parsed);
        for path in &system_includes {
            if !base.include_search.system.contains(path) {
                base.include_search.system.push(path.clone());
            }
        }
        Arc::new(base)
    };

    // ── Phase 1: Check cache for each unit (parallel, as-completed) ──
    let mut join_set = tokio::task::JoinSet::new();
    for (idx, compilation) in compilations.iter().enumerate() {
        let state = Arc::clone(&state);
        let cwd_path = cwd_path.clone();
        let system_includes = system_includes.clone();
        let compilation = compilation.clone();
        let shared_base = Arc::clone(&shared_base);
        join_set.spawn_blocking(move || {
            (
                idx,
                check_unit_cache(
                    &state,
                    &compilation,
                    &cwd_path,
                    &system_includes,
                    Some(&shared_base),
                ),
            )
        });
    }

    // Collect results in original order
    let mut indexed_results: Vec<(usize, UnitCacheResult)> = Vec::with_capacity(compilations.len());
    while let Some(result) = join_set.join_next().await {
        match result {
            Ok(pair) => indexed_results.push(pair),
            Err(e) => {
                return Response::Error {
                    message: format!("cache check task panicked: {e}"),
                };
            }
        }
    }
    indexed_results.sort_by_key(|(idx, _)| *idx);

    let mut unit_results: Vec<UnitCacheResult> = Vec::with_capacity(indexed_results.len());
    let mut all_pending_writes: Vec<PendingWrite> = Vec::new();
    for (_, mut result) in indexed_results {
        match &result {
            UnitCacheResult::Hit {
                stdout,
                stderr,
                artifact_bytes,
                source_path,
                ..
            } => {
                all_stdout.extend_from_slice(stdout);
                all_stderr.extend_from_slice(stderr);
                let src = source_path.clone();
                let bytes = *artifact_bytes;
                record_session_stat(&state.sessions, &sid, move |t| {
                    t.record_hit(src, 0, bytes);
                });
            }
            UnitCacheResult::Miss { source_path, .. } => {
                write_session_log(
                    &state.sessions,
                    &sid,
                    &format!("multi-file cache miss: {}", source_path.display()),
                );
            }
        }
        // Drain pending writes from hits for batched parallel execution
        if let UnitCacheResult::Hit {
            ref mut pending_writes,
            ..
        } = result
        {
            all_pending_writes.append(pending_writes);
        }
        unit_results.push(result);
    }

    // ── Phase 1b: Execute all output writes in parallel ─────────────
    if !all_pending_writes.is_empty() {
        let mut write_set = tokio::task::JoinSet::new();
        for pw in all_pending_writes {
            write_set.spawn_blocking(move || {
                let _ = write_cached_output(&pw.out_path, &pw.cache_file, &pw.data);
            });
        }
        while write_set.join_next().await.is_some() {}
    }

    // For cache HIT outputs: downgrade metadata without advancing clock
    // (same artifact content). For cache MISS outputs: apply_changes is
    // done later after real compilation. This preserves fast-hit cache
    // validity for unrelated source files.
    {
        let mut output_dirs = HashSet::new();
        for (idx, comp) in compilations.iter().enumerate() {
            let out = if comp.output_file.is_absolute() {
                comp.output_file.clone()
            } else {
                cwd_path.join(&comp.output_file)
            };
            if let Some(parent) = out.parent() {
                output_dirs.insert(parent.into());
            }
            if matches!(&unit_results[idx], UnitCacheResult::Hit { .. }) {
                state.cache_system.metadata().downgrade(&out);
            }
        }
        let dirs: Vec<NormalizedPath> = output_dirs.into_iter().collect();
        watch_directories(&state, &dirs).await;
    }

    let miss_sources: Vec<&NormalizedPath> = unit_results
        .iter()
        .filter_map(|r| match r {
            UnitCacheResult::Miss { source_path, .. } => Some(source_path),
            UnitCacheResult::Hit { .. } => None,
        })
        .collect();

    if miss_sources.is_empty() {
        return Response::CompileResult {
            exit_code: 0,
            stdout: Arc::new(all_stdout),
            stderr: Arc::new(all_stderr),
            cached: true,
        };
    }

    write_session_log(
        &state.sessions,
        &sid,
        &format!(
            "multi-file: compiling {} of {} files",
            miss_sources.len(),
            compilations.len()
        ),
    );

    // Build compiler args from original_args, removing hit source files by index.
    // This preserves all original flags (including unknown ones) exactly as passed.
    let supports_depfile = compilations[0].family.supports_depfile();
    let hit_indices: HashSet<usize> = {
        let miss_set: HashSet<&NormalizedPath> = miss_sources.iter().copied().collect();
        source_indices
            .iter()
            .enumerate()
            .filter_map(|(si_pos, &arg_idx)| {
                let comp = &compilations[si_pos];
                let abs_src = if comp.source_file.is_absolute() {
                    comp.source_file.clone()
                } else {
                    cwd_path.join(&comp.source_file)
                };
                if !miss_set.contains(&abs_src) {
                    Some(arg_idx)
                } else {
                    None
                }
            })
            .collect()
    };
    let mut compiler_args: Vec<String> = original_args
        .iter()
        .enumerate()
        .filter(|(i, _)| !hit_indices.contains(i))
        .map(|(_, a)| a.clone())
        .collect();
    if supports_depfile {
        compiler_args.push("-MD".to_string());
    }

    let _rsp_guard = match zccache_compiler::response_file::write_response_file_if_needed(
        &compiler_args,
        &state.depfile_tmpdir,
    ) {
        Ok(guard) => guard,
        Err(e) => {
            return Response::Error {
                message: format!("failed to write response file: {e}"),
            };
        }
    };

    let mut cmd = tokio::process::Command::new(&compiler);
    if let Some(ref rsp) = _rsp_guard {
        cmd.arg(rsp.at_arg()).current_dir(&cwd_path);
    } else {
        cmd.args(&compiler_args).current_dir(&cwd_path);
    }
    apply_client_env(&mut cmd, &client_env);
    let result = cmd.output().await;

    let output = match result {
        Ok(o) => o,
        Err(e) => {
            return Response::Error {
                message: format!("failed to run compiler: {e}"),
            };
        }
    };

    let exit_code = output.status.code().unwrap_or(-1);
    all_stdout.extend_from_slice(&output.stdout);
    all_stderr.extend_from_slice(&output.stderr);

    if exit_code != 0 {
        state.stats.record_error();
        record_session_stat(&state.sessions, &sid, |t| t.record_error());
        return Response::CompileResult {
            exit_code,
            stdout: Arc::new(all_stdout),
            stderr: Arc::new(all_stderr),
            cached: false,
        };
    }

    // ── Phase 3: Cache each miss result individually ─────────────────
    for unit in &unit_results {
        let (source_path, output_path, context_key, ctx) = match unit {
            UnitCacheResult::Miss {
                source_path,
                output_path,
                context_key,
                ctx,
            } => (source_path, output_path, context_key, ctx),
            UnitCacheResult::Hit { .. } => continue,
        };

        let output_data = match std::fs::read(output_path) {
            Ok(data) => data,
            Err(_) => continue,
        };

        // Scan includes: use depfile if available, fall back to scanner.
        let scan_result = if supports_depfile {
            let d_path = source_path.with_extension("d");
            // Multi-file -MD places .d files relative to the source
            let cwd_d_path = cwd_path.join(
                d_path
                    .file_name()
                    .unwrap_or_else(|| std::ffi::OsStr::new("deps.d")),
            );
            let depfile_path: NormalizedPath = if d_path.exists() {
                d_path.into()
            } else if cwd_d_path.exists() {
                cwd_d_path
            } else {
                // Try deriving from source file stem in cwd
                let stem = source_path
                    .file_stem()
                    .unwrap_or_else(|| std::ffi::OsStr::new("out"));
                cwd_path.join(stem).with_extension("d").into()
            };
            match zccache_depgraph::depfile::parse_depfile_path(
                &depfile_path,
                source_path,
                &cwd_path,
            ) {
                Ok(result) => {
                    let _ = std::fs::remove_file(&depfile_path);
                    result
                }
                Err(e) => {
                    tracing::warn!(
                        "multi-file depfile parse failed for {}: {e}",
                        source_path.display()
                    );
                    zccache_depgraph::scanner::scan_recursive(source_path, &ctx.include_search)
                }
            }
        } else {
            zccache_depgraph::scanner::scan_recursive(source_path, &ctx.include_search)
        };

        let tracked_paths: Vec<NormalizedPath> = std::iter::once(source_path.clone())
            .chain(scan_result.resolved.iter().cloned())
            .collect();
        state.cache_system.register_tracked(&tracked_paths);

        // Watch parent directories of source file AND discovered headers.
        {
            let dep_dirs: Vec<NormalizedPath> = {
                let mut dirs = HashSet::new();
                if let Some(parent) = source_path.parent() {
                    dirs.insert(parent.into());
                }
                for header in &scan_result.resolved {
                    if let Some(parent) = header.parent() {
                        dirs.insert(parent.into());
                    }
                }
                dirs.into_iter().collect()
            };
            watch_directories(&state, &dep_dirs).await;
        }

        // Hash all files (source + headers) in parallel
        let hash_map: HashMap<NormalizedPath, ContentHash> = {
            use rayon::prelude::*;
            let all_paths: Vec<&NormalizedPath> = std::iter::once(source_path)
                .chain(scan_result.resolved.iter())
                .collect();
            all_paths
                .par_iter()
                .filter_map(|path| {
                    hash_file(&state.cache_system, path, snap_clock)
                        .ok()
                        .map(|h| ((*path).clone(), h))
                })
                .collect()
        };

        // Store artifact
        let get_hash = |p: &Path| {
            let path = NormalizedPath::new(p);
            hash_map.get(&path).copied()
        };
        let update_result = state.dep_graph.update(context_key, scan_result, get_hash);
        if let Some(artifact_key) = update_result {
            let artifact = ArtifactData {
                outputs: vec![ArtifactOutput {
                    name: output_path
                        .file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .into_owned(),
                    data: Arc::new(output_data),
                }],
                stdout: Arc::new(Vec::new()),
                stderr: Arc::new(Vec::new()),
                exit_code: 0,
            };

            let artifact_key_hex = artifact_key.hash().to_hex();
            let artifact_bytes: u64 = artifact.outputs.iter().map(|o| o.data.len() as u64).sum();

            // Build CachedArtifact once (no deep copies — all Arc clones).
            let cached = CachedArtifact::from_artifact_data(&artifact);

            // Spawn disk persistence to background (meta.clone() is cheap — Arc fields only).
            {
                let artifact_dir = state.artifact_dir.clone();
                let key_hex = artifact_key_hex.clone();
                let persist_meta = cached.meta.clone();
                let payloads: Vec<Arc<Vec<u8>>> = artifact
                    .outputs
                    .iter()
                    .map(|o| Arc::clone(&o.data))
                    .collect();
                let payload_size: usize = payloads.iter().map(|p| p.len()).sum();
                state
                    .in_flight_bytes
                    .fetch_add(payload_size, Ordering::Relaxed);
                let guard = InFlightGuard {
                    state: Arc::clone(&state),
                    size: payload_size,
                };
                let sem = Arc::clone(&state.persist_semaphore);
                let state_ref = Arc::clone(&state);
                tokio::spawn(async move {
                    let _permit = sem.acquire().await.unwrap();
                    tokio::task::spawn_blocking(move || {
                        let _guard = guard;
                        for (i, payload) in payloads.iter().enumerate() {
                            let cache_path = artifact_dir.join(format!("{key_hex}_{i}"));
                            if let Err(e) = std::fs::write(&cache_path, &**payload) {
                                tracing::warn!(
                                    path = %cache_path.display(),
                                    "failed to persist artifact output: {e}"
                                );
                            }
                        }
                        state_ref
                            .artifact_store
                            .insert(&key_hex, &persist_meta)
                            .ok();
                    })
                    .await
                    .ok();
                });
            }

            state.artifacts.insert(artifact_key_hex.clone(), cached);

            // Populate fast-hit cache for future requests
            let current_clock = state.cache_system.current_clock();
            state.fast_hit_cache.insert(
                *context_key,
                FastHitEntry {
                    clock: current_clock,
                    artifact_key_hex,
                    cached_at: std::time::Instant::now(),
                },
            );

            state.stats.record_miss(0, artifact_bytes);
            let src = source_path.clone();
            record_session_stat(&state.sessions, &sid, move |t| {
                t.record_miss(src, artifact_bytes);
            });
        }

        // Miss outputs have genuinely new content — advance the clock so
        // downstream consumers (link cache) see the change.
        state.cache_system.apply_changes(vec![output_path.clone()]);
    }

    Response::CompileResult {
        exit_code: 0,
        stdout: Arc::new(all_stdout),
        stderr: Arc::new(all_stderr),
        cached: false,
    }
}

/// Run the compiler directly without caching.
async fn run_compiler_direct(
    compiler: &NormalizedPath,
    args: &[String],
    cwd: &Path,
    sessions: &SessionManager,
    sid: &SessionId,
    client_env: &Option<Vec<(String, String)>>,
) -> Response {
    let tmp_dir = std::env::temp_dir();
    let _rsp_guard =
        match zccache_compiler::response_file::write_response_file_if_needed(args, &tmp_dir) {
            Ok(guard) => guard,
            Err(e) => {
                return Response::Error {
                    message: format!("failed to write response file: {e}"),
                };
            }
        };

    let mut cmd = tokio::process::Command::new(compiler);
    if let Some(ref rsp) = _rsp_guard {
        cmd.arg(rsp.at_arg()).current_dir(cwd);
    } else {
        cmd.args(args).current_dir(cwd);
    }
    apply_client_env(&mut cmd, client_env);
    let result = cmd.output().await;

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

#[cfg(test)]
mod tests {
    use super::*;

    async fn start_daemon() -> (String, tokio::task::JoinHandle<()>, Arc<Notify>) {
        let endpoint = zccache_ipc::unique_test_endpoint();
        let mut server = DaemonServer::bind(&endpoint).unwrap();
        let shutdown = server.shutdown_handle();
        let handle = tokio::spawn(async move {
            server.run(0).await.unwrap();
        });
        (endpoint, handle, shutdown)
    }

    #[cfg(windows)]
    #[test]
    fn request_fingerprint_normalizes_equivalent_windows_paths() {
        let args = vec!["-c".to_string(), "src/main.cpp".to_string()];
        let a = request_fingerprint(
            Path::new(r"C:\LLVM\bin\clang++.exe"),
            &args,
            Path::new(r"C:\Work\Project"),
        );
        let b = request_fingerprint(
            Path::new("c:/llvm/bin/clang++.exe"),
            &args,
            Path::new("c:/work/project"),
        );
        assert_eq!(a, b);
    }

    #[tokio::test]
    #[ignore] // integration-level: starts real daemon with IPC + file watcher
    async fn test_server_ping_pong() {
        zccache_test_support::test_timeout(async {
            let (endpoint, server_task, shutdown) = start_daemon().await;

            let mut client = zccache_ipc::connect(&endpoint).await.unwrap();
            client.send(&Request::Ping).await.unwrap();
            let resp: Option<Response> = client.recv().await.unwrap();
            assert_eq!(resp, Some(Response::Pong));

            shutdown.notify_one();
            server_task.await.unwrap();
        })
        .await;
    }

    #[tokio::test]
    #[ignore] // integration-level: starts real daemon with IPC + file watcher
    async fn test_server_shutdown_request() {
        zccache_test_support::test_timeout(async {
            let (endpoint, server_task, shutdown) = start_daemon().await;

            let mut client = zccache_ipc::connect(&endpoint).await.unwrap();
            client.send(&Request::Shutdown).await.unwrap();
            let resp: Option<Response> = client.recv().await.unwrap();
            assert_eq!(resp, Some(Response::ShuttingDown));

            shutdown.notify_one();
            server_task.await.unwrap();
        })
        .await;
    }

    #[tokio::test]
    #[ignore] // integration-level: starts real daemon with IPC + file watcher
    async fn test_server_clear_empty() {
        zccache_test_support::test_timeout(async {
            let (endpoint, server_task, shutdown) = start_daemon().await;

            let mut client = zccache_ipc::connect(&endpoint).await.unwrap();
            client.send(&Request::Clear).await.unwrap();
            let resp: Option<Response> = client.recv().await.unwrap();
            match resp {
                Some(Response::Cleared {
                    metadata_cleared,
                    dep_graph_contexts_cleared,
                    ..
                }) => {
                    // artifacts_removed may be >0 if persistent cache has entries
                    // from a prior run. Metadata and dep graph are always fresh.
                    assert_eq!(metadata_cleared, 0);
                    assert_eq!(dep_graph_contexts_cleared, 0);
                }
                other => panic!("expected Cleared, got: {other:?}"),
            }

            shutdown.notify_one();
            server_task.await.unwrap();
        })
        .await;
    }

    #[tokio::test]
    #[ignore] // integration-level: starts real daemon with IPC + file watcher
    async fn test_server_status() {
        zccache_test_support::test_timeout(async {
            let (endpoint, server_task, shutdown) = start_daemon().await;

            let mut client = zccache_ipc::connect(&endpoint).await.unwrap();
            client.send(&Request::Status).await.unwrap();
            let resp: Option<Response> = client.recv().await.unwrap();
            assert!(matches!(resp, Some(Response::Status(_))));

            shutdown.notify_one();
            server_task.await.unwrap();
        })
        .await;
    }

    // ── CLI session flow tests (IPC-based) ──────────────────────────────

    /// Full session lifecycle: start → compile (miss) → compile (hit) → end.
    #[tokio::test]
    #[ignore] // integration-level: starts real daemon with IPC + compiler
    async fn cli_session_lifecycle() {
        let clang = match zccache_test_support::find_clang() {
            Some(p) => p,
            None => return,
        };
        zccache_test_support::test_timeout(async move {
            let tmp = tempfile::tempdir().unwrap();
            let src = tmp.path().join("hello.cpp");
            let obj = tmp.path().join("hello.o");
            let log = tmp.path().join("session.log");
            let cwd = tmp.path().to_string_lossy().into_owned();

            std::fs::write(
                &src,
                "#include <stdio.h>\nint main() { printf(\"hello\\n\"); return 0; }\n",
            )
            .unwrap();

            let (endpoint, server_handle, shutdown) = start_daemon().await;
            let mut client = zccache_ipc::connect(&endpoint).await.unwrap();

            // session-start
            client
                .send(&Request::SessionStart {
                    client_pid: std::process::id(),
                    working_dir: cwd.clone().into(),
                    log_file: Some(log.to_string_lossy().into_owned().into()),
                    track_stats: false,
                    journal_path: None,
                })
                .await
                .unwrap();

            let session_id = match client.recv().await.unwrap() {
                Some(Response::SessionStarted { session_id, .. }) => session_id,
                other => panic!("expected SessionStarted, got: {other:?}"),
            };

            // first compile (cache miss)
            client
                .send(&Request::Compile {
                    session_id: session_id.clone(),
                    args: vec![
                        "-c".to_string(),
                        src.to_string_lossy().into_owned(),
                        "-o".to_string(),
                        obj.to_string_lossy().into_owned(),
                    ],
                    cwd: cwd.clone().into(),
                    compiler: clang.to_string_lossy().into_owned().into(),
                    env: None,
                })
                .await
                .unwrap();

            match client.recv().await.unwrap() {
                Some(Response::CompileResult {
                    exit_code, cached, ..
                }) => {
                    assert_eq!(exit_code, 0, "first compile should succeed");
                    assert!(!cached, "first compile should be a miss");
                }
                other => panic!("expected CompileResult, got: {other:?}"),
            }

            assert!(obj.exists(), ".o should exist after first compile");
            let obj_data = std::fs::read(&obj).unwrap();

            // second compile (cache hit)
            std::fs::remove_file(&obj).unwrap();

            client
                .send(&Request::Compile {
                    session_id: session_id.clone(),
                    args: vec![
                        "-c".to_string(),
                        src.to_string_lossy().into_owned(),
                        "-o".to_string(),
                        obj.to_string_lossy().into_owned(),
                    ],
                    cwd: cwd.clone().into(),
                    compiler: clang.to_string_lossy().into_owned().into(),
                    env: None,
                })
                .await
                .unwrap();

            match client.recv().await.unwrap() {
                Some(Response::CompileResult {
                    exit_code, cached, ..
                }) => {
                    assert_eq!(exit_code, 0, "cached compile should succeed");
                    assert!(cached, "second compile should be a hit");
                }
                other => panic!("expected CompileResult, got: {other:?}"),
            }

            assert!(obj.exists(), ".o should exist after cached compile");
            let cached_data = std::fs::read(&obj).unwrap();
            assert_eq!(obj_data.len(), cached_data.len(), "cached .o should match");

            // session-end
            client
                .send(&Request::SessionEnd {
                    session_id: session_id.clone(),
                })
                .await
                .unwrap();

            match client.recv().await.unwrap() {
                Some(Response::SessionEnded { .. }) => {}
                other => panic!("expected SessionEnded, got: {other:?}"),
            }

            // compile after session-end should fail
            client
                .send(&Request::Compile {
                    session_id,
                    args: vec!["-c".to_string(), src.to_string_lossy().into_owned()],
                    cwd: cwd.clone().into(),
                    compiler: clang.to_string_lossy().into_owned().into(),
                    env: None,
                })
                .await
                .unwrap();

            match client.recv().await.unwrap() {
                Some(Response::Error { message }) => {
                    assert!(
                        message.contains("unknown session"),
                        "should report unknown session after end: {message}"
                    );
                }
                other => panic!("expected Error after session-end, got: {other:?}"),
            }

            // verify log
            let log_text = std::fs::read_to_string(&log).unwrap();
            assert!(log_text.contains("[MISS]"), "log should show miss");
            assert!(log_text.contains("[HIT]"), "log should show hit");

            shutdown.notify_one();
            server_handle.await.unwrap();
        })
        .await;
    }

    /// Ending a nonexistent session returns an error.
    #[tokio::test]
    #[ignore] // integration-level: starts real daemon with IPC
    async fn cli_session_end_invalid_id() {
        zccache_test_support::test_timeout(async {
            let (endpoint, server_handle, shutdown) = start_daemon().await;
            let mut client = zccache_ipc::connect(&endpoint).await.unwrap();

            client
                .send(&Request::SessionEnd {
                    session_id: 999999.to_string(),
                })
                .await
                .unwrap();

            match client.recv().await.unwrap() {
                Some(Response::Error { message }) => {
                    assert!(
                        message.contains("unknown session") || message.contains("invalid session"),
                        "expected session error, got: {message}"
                    );
                }
                other => panic!("expected Error, got: {other:?}"),
            }

            shutdown.notify_one();
            server_handle.await.unwrap();
        })
        .await;
    }

    /// Cache clear resets: miss → hit → clear → miss again.
    #[tokio::test]
    #[ignore] // integration-level: starts real daemon with IPC + compiler
    async fn cli_clear_resets_cache() {
        let clang = match zccache_test_support::find_clang() {
            Some(p) => p,
            None => return,
        };

        zccache_test_support::test_timeout(async move {
            let tmp = tempfile::tempdir().unwrap();
            let src = tmp.path().join("clear_test.cpp");
            let obj = tmp.path().join("clear_test.o");
            let cwd = tmp.path().to_string_lossy().into_owned();

            std::fs::write(&src, "int main() { return 0; }\n").unwrap();

            let (endpoint, server_handle, shutdown) = start_daemon().await;
            let mut client = zccache_ipc::connect(&endpoint).await.unwrap();

            // Start session
            client
                .send(&Request::SessionStart {
                    client_pid: std::process::id(),
                    working_dir: cwd.clone().into(),
                    log_file: None,
                    track_stats: false,
                    journal_path: None,
                })
                .await
                .unwrap();

            let session_id = match client.recv().await.unwrap() {
                Some(Response::SessionStarted { session_id, .. }) => session_id,
                other => panic!("expected SessionStarted, got: {other:?}"),
            };

            let compile_args = vec![
                "-c".to_string(),
                src.to_string_lossy().into_owned(),
                "-o".to_string(),
                obj.to_string_lossy().into_owned(),
            ];

            // First compile → miss
            client
                .send(&Request::Compile {
                    session_id: session_id.clone(),
                    args: compile_args.clone(),
                    cwd: cwd.clone().into(),
                    compiler: clang.to_string_lossy().into_owned().into(),
                    env: None,
                })
                .await
                .unwrap();

            match client.recv().await.unwrap() {
                Some(Response::CompileResult {
                    exit_code, cached, ..
                }) => {
                    assert_eq!(exit_code, 0);
                    assert!(!cached, "first compile should be a miss");
                }
                other => panic!("expected CompileResult, got: {other:?}"),
            }

            // Second compile → hit
            std::fs::remove_file(&obj).unwrap();
            client
                .send(&Request::Compile {
                    session_id: session_id.clone(),
                    args: compile_args.clone(),
                    cwd: cwd.clone().into(),
                    compiler: clang.to_string_lossy().into_owned().into(),
                    env: None,
                })
                .await
                .unwrap();

            match client.recv().await.unwrap() {
                Some(Response::CompileResult {
                    exit_code, cached, ..
                }) => {
                    assert_eq!(exit_code, 0);
                    assert!(cached, "second compile should be a hit");
                }
                other => panic!("expected CompileResult, got: {other:?}"),
            }

            // Clear the cache
            client.send(&Request::Clear).await.unwrap();
            match client.recv().await.unwrap() {
                Some(Response::Cleared {
                    artifacts_removed, ..
                }) => {
                    assert!(
                        artifacts_removed > 0,
                        "should have cleared at least one artifact"
                    );
                }
                other => panic!("expected Cleared, got: {other:?}"),
            }

            // End old session and start a new one
            client
                .send(&Request::SessionEnd { session_id })
                .await
                .unwrap();
            let _: Option<Response> = client.recv().await.unwrap();

            client
                .send(&Request::SessionStart {
                    client_pid: std::process::id(),
                    working_dir: cwd.clone().into(),
                    log_file: None,
                    track_stats: false,
                    journal_path: None,
                })
                .await
                .unwrap();

            let session_id2 = match client.recv().await.unwrap() {
                Some(Response::SessionStarted { session_id, .. }) => session_id,
                other => panic!("expected SessionStarted, got: {other:?}"),
            };

            // Compile again → should be a miss (cache was cleared)
            std::fs::remove_file(&obj).unwrap();
            client
                .send(&Request::Compile {
                    session_id: session_id2,
                    args: compile_args,
                    cwd: cwd.into(),
                    compiler: clang.to_string_lossy().into_owned().into(),
                    env: None,
                })
                .await
                .unwrap();

            match client.recv().await.unwrap() {
                Some(Response::CompileResult {
                    exit_code, cached, ..
                }) => {
                    assert_eq!(exit_code, 0);
                    assert!(!cached, "compile after clear should be a miss");
                }
                other => panic!("expected CompileResult, got: {other:?}"),
            }

            shutdown.notify_one();
            server_handle.await.unwrap();
        })
        .await;
    }

    /// Multi-file compilations fall back to running the compiler directly.
    #[tokio::test]
    #[ignore] // integration-level: starts real daemon with IPC + compiler
    async fn cli_multi_file_compilation_runs_directly() {
        let clang = match zccache_test_support::find_clang() {
            Some(p) => p,
            None => return,
        };

        zccache_test_support::test_timeout(async move {
            let tmp = tempfile::tempdir().unwrap();
            let src_a = tmp.path().join("multi_a.cpp");
            let src_b = tmp.path().join("multi_b.cpp");
            let cwd = tmp.path().to_string_lossy().into_owned();

            std::fs::write(&src_a, "int foo() { return 1; }\n").unwrap();
            std::fs::write(&src_b, "int bar() { return 2; }\n").unwrap();

            let (endpoint, server_handle, shutdown) = start_daemon().await;
            let mut client = zccache_ipc::connect(&endpoint).await.unwrap();

            // Start session
            client
                .send(&Request::SessionStart {
                    client_pid: std::process::id(),
                    working_dir: cwd.clone().into(),
                    log_file: None,
                    track_stats: true,
                    journal_path: None,
                })
                .await
                .unwrap();

            let session_id = match client.recv().await.unwrap() {
                Some(Response::SessionStarted { session_id, .. }) => session_id,
                other => panic!("expected SessionStarted, got: {other:?}"),
            };

            // First compile: multi-file → both are cache misses
            let multi_args = vec![
                "-c".to_string(),
                src_a.to_string_lossy().into_owned(),
                src_b.to_string_lossy().into_owned(),
            ];
            client
                .send(&Request::Compile {
                    session_id: session_id.clone(),
                    args: multi_args.clone(),
                    cwd: cwd.clone().into(),
                    compiler: clang.to_string_lossy().into_owned().into(),
                    env: None,
                })
                .await
                .unwrap();

            match client.recv().await.unwrap() {
                Some(Response::CompileResult {
                    exit_code, cached, ..
                }) => {
                    assert_eq!(exit_code, 0, "multi-file compile should succeed");
                    assert!(!cached, "first multi-file compile should be a miss");
                }
                other => panic!("expected CompileResult, got: {other:?}"),
            }

            // Verify both .o files were produced
            let obj_a = tmp.path().join("multi_a.o");
            let obj_b = tmp.path().join("multi_b.o");
            assert!(obj_a.exists(), "multi_a.o should exist");
            assert!(obj_b.exists(), "multi_b.o should exist");

            // Second compile: same files → should be all cache hits
            client
                .send(&Request::Compile {
                    session_id: session_id.clone(),
                    args: multi_args,
                    cwd: cwd.clone().into(),
                    compiler: clang.to_string_lossy().into_owned().into(),
                    env: None,
                })
                .await
                .unwrap();

            match client.recv().await.unwrap() {
                Some(Response::CompileResult {
                    exit_code, cached, ..
                }) => {
                    assert_eq!(exit_code, 0, "second multi-file compile should succeed");
                    assert!(cached, "second multi-file compile should be all cache hits");
                }
                other => panic!("expected CompileResult, got: {other:?}"),
            }

            // End session and verify stats
            client
                .send(&Request::SessionEnd { session_id })
                .await
                .unwrap();

            match client.recv().await.unwrap() {
                Some(Response::SessionEnded { stats }) => {
                    if let Some(s) = stats {
                        assert!(
                            s.misses >= 2,
                            "first multi-file compile should have 2 misses, got: {}",
                            s.misses
                        );
                        assert!(
                            s.hits >= 2,
                            "second multi-file compile should have 2 hits, got: {}",
                            s.hits
                        );
                    }
                }
                other => panic!("expected SessionEnded, got: {other:?}"),
            }

            shutdown.notify_one();
            server_handle.await.unwrap();
        })
        .await;
    }

    // ── pch_source_header unit tests ────────────────────────────────────

    #[test]
    fn pch_source_header_sibling() {
        let tmp = tempfile::tempdir().unwrap();
        let header = tmp.path().join("pch.h");
        let pch = tmp.path().join("pch.h.pch");
        std::fs::write(&header, "// pch").unwrap();
        std::fs::write(&pch, "binary").unwrap();

        let result = pch_source_header(&pch);
        assert_eq!(result, Some(header.into()));
    }

    #[test]
    fn pch_source_header_build_dir() {
        // The walk-up heuristic looks for `<dir_name>/<header_name>` from ancestors.
        // e.g., for .build/tests/pch.h.pch it looks for tests/pch.h in parents.
        let tmp = tempfile::tempdir().unwrap();
        // Source: tmp/tests/pch.h (matches the `tests/pch.h` relative lookup)
        let src_dir = tmp.path().join("tests");
        std::fs::create_dir_all(&src_dir).unwrap();
        let header = src_dir.join("pch.h");
        std::fs::write(&header, "// pch").unwrap();

        // PCH: tmp/build/tests/pch.h.pch
        let build_dir = tmp.path().join("build").join("tests");
        std::fs::create_dir_all(&build_dir).unwrap();
        let pch = build_dir.join("pch.h.pch");
        std::fs::write(&pch, "binary").unwrap();

        let result = pch_source_header(&pch);
        assert_eq!(result, Some(header.into()));
    }

    #[test]
    fn pch_source_header_not_found() {
        let tmp = tempfile::tempdir().unwrap();
        let build_dir = tmp.path().join("build");
        std::fs::create_dir_all(&build_dir).unwrap();
        let pch = build_dir.join("pch.h.pch");
        std::fs::write(&pch, "binary").unwrap();

        let result = pch_source_header(&pch);
        assert_eq!(result, None);
    }

    #[test]
    fn pch_source_header_non_pch() {
        let tmp = tempfile::tempdir().unwrap();
        let obj = tmp.path().join("foo.o");
        std::fs::write(&obj, "object").unwrap();

        let result = pch_source_header(&obj);
        assert_eq!(result, None);
    }

    #[test]
    fn pch_source_header_gch_extension() {
        let tmp = tempfile::tempdir().unwrap();
        let header = tmp.path().join("pch.h");
        let gch = tmp.path().join("pch.h.gch");
        std::fs::write(&header, "// pch").unwrap();
        std::fs::write(&gch, "binary").unwrap();

        let result = pch_source_header(&gch);
        assert_eq!(result, Some(header.into()));
    }

    // ── resolve_pch_source unit tests ───────────────────────────────────

    #[test]
    fn resolve_pch_source_registry_hit() {
        let pch_map: DashMap<NormalizedPath, NormalizedPath> = DashMap::new();
        let pch_path = NormalizedPath::from("/build/tests/pch.h.pch");
        let src_path = NormalizedPath::from("/src/tests/pch.h");
        pch_map.insert(pch_path.clone(), src_path.clone());

        let result = resolve_pch_source(&pch_path, &pch_map);
        assert_eq!(result, Some(src_path));
    }

    #[test]
    fn resolve_pch_source_falls_back_to_filesystem() {
        let tmp = tempfile::tempdir().unwrap();
        let header = tmp.path().join("pch.h");
        let pch = tmp.path().join("pch.h.pch");
        std::fs::write(&header, "// pch").unwrap();
        std::fs::write(&pch, "binary").unwrap();

        let pch_map: DashMap<NormalizedPath, NormalizedPath> = DashMap::new();
        let result = resolve_pch_source(&pch, &pch_map);
        assert_eq!(result, Some(header.into()));
    }

    #[test]
    fn resolve_pch_source_non_pch_returns_none() {
        let pch_map: DashMap<NormalizedPath, NormalizedPath> = DashMap::new();
        let result = resolve_pch_source(Path::new("/build/foo.o"), &pch_map);
        assert_eq!(result, None);
    }

    // ── write_cached_output staleness tests ────────────────────────────

    /// Regression test: write_cached_output must overwrite an existing output
    /// file even when the existing file has the same size as the cached data.
    ///
    /// This reproduces the linker staleness bug where a header change produces
    /// a .o of the same size but different content — the old size-only check
    /// skipped the write, leaving a stale .o on disk with missing symbols.
    #[test]
    fn write_cached_output_overwrites_same_size_different_content() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("output.o");
        let cache = dir.path().join("cached.o");

        // Simulate: output.o exists from a previous compilation (version A).
        let old_content = b"AAAA_symbols_v1_xxxx";
        std::fs::write(&out, old_content).unwrap();

        // Simulate: cache file has new content (version B) — same size, different bytes.
        let new_content = b"BBBB_symbols_v2_yyyy";
        assert_eq!(
            old_content.len(),
            new_content.len(),
            "test requires same size"
        );
        std::fs::write(&cache, new_content).unwrap();

        // write_cached_output must replace the stale output with the cached content.
        write_cached_output(&out, &cache, new_content).unwrap();

        let result = std::fs::read(&out).unwrap();
        assert_eq!(
            result, new_content,
            "output must contain new content, not stale old content"
        );
    }

    /// write_cached_output correctly creates the output when it doesn't exist.
    #[test]
    fn write_cached_output_creates_new_file() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("output.o");
        let cache = dir.path().join("cached.o");

        let content = b"fresh object file data";
        std::fs::write(&cache, content).unwrap();

        write_cached_output(&out, &cache, content).unwrap();

        let result = std::fs::read(&out).unwrap();
        assert_eq!(result, content.as_slice());
    }

    /// write_cached_output falls back to memory copy when cache file is missing.
    #[test]
    fn write_cached_output_fallback_to_memory_copy() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("output.o");
        let cache = dir.path().join("nonexistent_cache.o");

        let content = b"data from memory";

        write_cached_output(&out, &cache, content).unwrap();

        let result = std::fs::read(&out).unwrap();
        assert_eq!(result, content.as_slice());
    }

    /// write_cached_output skips the write when output is already a hardlink
    /// to the cache file (same file identity). This is the fast path for
    /// repeated cache hits with the same artifact key.
    #[test]
    fn write_cached_output_skips_when_already_hardlinked() {
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join("cached.o");
        let out = dir.path().join("output.o");

        let content = b"cached artifact content";
        std::fs::write(&cache, content).unwrap();

        // First write: creates hardlink
        write_cached_output(&out, &cache, content).unwrap();
        assert_eq!(std::fs::read(&out).unwrap(), content.as_slice());

        // Verify they are the same file (hardlink).
        assert!(
            same_file(&out, &cache),
            "output should be a hardlink to cache file after first write"
        );

        // Second write: should detect hardlink and skip.
        // (If it didn't skip, it would still produce correct content,
        //  but the test verifies the optimization path exists.)
        write_cached_output(&out, &cache, content).unwrap();
        assert_eq!(std::fs::read(&out).unwrap(), content.as_slice());
    }

    /// Regression test for issue #15: hardlink delivery must set output mtime
    /// to current time. Without this, build systems (cargo, make, ninja) see
    /// the output as older than its dependencies and trigger unnecessary rebuilds.
    ///
    /// Root cause: hardlinks share mtime with the cache file, which was created
    /// during the original compilation (potentially minutes/hours ago). Cargo
    /// checks "is library output older than build script output?" and if the
    /// library was hardlinked from an old cache file, the answer is yes → dirty.
    #[test]
    fn write_cached_output_sets_fresh_mtime_on_hardlink() {
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join("cached.rlib");
        let out = dir.path().join("output.rlib");

        let content = b"cached rlib data";
        std::fs::write(&cache, content).unwrap();

        // Backdate the cache file to simulate an artifact from a previous build.
        let old_time = filetime::FileTime::from_unix_time(1_000_000_000, 0); // 2001-09-09
        filetime::set_file_mtime(&cache, old_time).unwrap();

        // Deliver via write_cached_output (will hardlink)
        write_cached_output(&out, &cache, content).unwrap();

        // Output must have recent mtime, NOT the 2001 mtime from the cache file.
        let out_mtime =
            filetime::FileTime::from_last_modification_time(&std::fs::metadata(&out).unwrap());
        let now = filetime::FileTime::now();
        let diff = now.unix_seconds() - out_mtime.unix_seconds();

        assert!(
            diff < 5,
            "output mtime is {diff}s old — should be <5s.\n\
             Cache file had mtime from 2001; hardlink must touch mtime to now.\n\
             Output mtime: {out_mtime:?}, expected ~{now:?}"
        );
    }

    /// Same as above but for the same_file (already hardlinked) path.
    #[test]
    fn write_cached_output_refreshes_mtime_on_existing_hardlink() {
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join("cached.rlib");
        let out = dir.path().join("output.rlib");

        let content = b"cached rlib data";
        std::fs::write(&cache, content).unwrap();

        // First delivery: creates hardlink
        write_cached_output(&out, &cache, content).unwrap();

        // Backdate both files (they share the same inode)
        let old_time = filetime::FileTime::from_unix_time(1_000_000_000, 0);
        filetime::set_file_mtime(&out, old_time).unwrap();

        // Second delivery: same_file path should still refresh mtime
        write_cached_output(&out, &cache, content).unwrap();

        let out_mtime =
            filetime::FileTime::from_last_modification_time(&std::fs::metadata(&out).unwrap());
        let now = filetime::FileTime::now();
        let diff = now.unix_seconds() - out_mtime.unix_seconds();

        assert!(
            diff < 5,
            "mtime not refreshed on existing hardlink path — {diff}s old"
        );
    }

    /// write_cached_output fallback (fs::write) naturally sets fresh mtime.
    #[test]
    fn write_cached_output_fallback_has_fresh_mtime() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("output.rlib");
        let cache = dir.path().join("nonexistent_cache.rlib");

        let content = b"data from memory";
        write_cached_output(&out, &cache, content).unwrap();

        let out_mtime =
            filetime::FileTime::from_last_modification_time(&std::fs::metadata(&out).unwrap());
        let now = filetime::FileTime::now();
        let diff = now.unix_seconds() - out_mtime.unix_seconds();

        assert!(
            diff < 5,
            "fallback path should produce fresh mtime — {diff}s old"
        );
    }

    // ── run_post_link_deploy_hook unit tests ────────────────────────────
    //
    // These tests use a tiny helper program that writes a file next to the
    // provided output path and exits 0, simulating a real deploy tool like
    // `clang-tool-chain-libdeploy`. They verify:
    //   - the hook runs when invoked
    //   - failures don't panic / propagate (hook is best-effort)
    //   - the env is propagated

    /// Run the hook with a command that creates a sidecar file next to the
    /// output. Verifies the sidecar appears — this is the contract that
    /// `side_effect::detect_side_effects` relies on.
    #[cfg(unix)] // uses /bin/sh; Windows has its own test below
    #[tokio::test]
    async fn post_link_deploy_hook_runs_and_creates_sidecar() {
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("app");
        std::fs::write(&output, b"binary").unwrap();

        // Fake deploy tool: creates a sidecar DLL next to the passed path.
        let script = dir.path().join("fake_deploy.sh");
        std::fs::write(
            &script,
            "#!/bin/sh\ntouch \"$(dirname \"$1\")/libruntime.so\"\n",
        )
        .unwrap();
        std::process::Command::new("chmod")
            .args(["+x"])
            .arg(&script)
            .status()
            .unwrap();

        let cmd_str = script.to_string_lossy().to_string();
        run_post_link_deploy_hook(&cmd_str, &output, None).await;

        assert!(
            dir.path().join("libruntime.so").exists(),
            "hook should have created the sidecar"
        );
    }

    /// Hook that exits non-zero must not panic — failures are best-effort.
    #[cfg(unix)]
    #[tokio::test]
    async fn post_link_deploy_hook_failure_is_non_fatal() {
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("app");
        std::fs::write(&output, b"binary").unwrap();

        // Just exit 1 — no side effect.
        run_post_link_deploy_hook("false", &output, None).await;
        // If we reached here without panic, the test passes. A warning should
        // have been logged by the hook.
    }

    /// Nonexistent program — hook should log a warning, not panic.
    #[tokio::test]
    async fn post_link_deploy_hook_nonexistent_program_is_non_fatal() {
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("app.dll");
        std::fs::write(&output, b"binary").unwrap();

        run_post_link_deploy_hook(
            "this-program-does-not-exist-zccache-test-12345",
            &output,
            None,
        )
        .await;
        // No panic = pass.
    }

    /// Empty command string — must early-return without attempting to spawn.
    #[tokio::test]
    async fn post_link_deploy_hook_empty_cmd_is_noop() {
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("app.dll");
        std::fs::write(&output, b"binary").unwrap();

        run_post_link_deploy_hook("", &output, None).await;
        run_post_link_deploy_hook("   ", &output, None).await;
        // No panic = pass.
    }

    /// Env is propagated to the hook process.
    #[cfg(unix)]
    #[tokio::test]
    async fn post_link_deploy_hook_propagates_env() {
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("app");
        std::fs::write(&output, b"binary").unwrap();

        // Script reads $ZCCACHE_TEST_MARKER from env and writes it to a
        // marker file next to the output.
        let script = dir.path().join("read_env.sh");
        std::fs::write(
            &script,
            "#!/bin/sh\nprintf '%s' \"$ZCCACHE_TEST_MARKER\" > \"$(dirname \"$1\")/marker.txt\"\n",
        )
        .unwrap();
        std::process::Command::new("chmod")
            .args(["+x"])
            .arg(&script)
            .status()
            .unwrap();

        let env = vec![
            (
                "PATH".to_string(),
                std::env::var("PATH").unwrap_or_default(),
            ),
            ("ZCCACHE_TEST_MARKER".to_string(), "hello-hook".to_string()),
        ];
        run_post_link_deploy_hook(&script.to_string_lossy(), &output, Some(&env)).await;

        let marker = std::fs::read_to_string(dir.path().join("marker.txt")).unwrap();
        assert_eq!(marker, "hello-hook");
    }
}
