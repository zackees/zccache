//! `SharedState` — the daemon's central state object shared by every
//! request handler.
//!
//! Every connection handler receives an `Arc<SharedState>` and reads from
//! these fields directly. Most fields are append-only after `DaemonServer::bind`
//! (`sessions`, `journal`, `artifact_store`); the lock-free `DashMap`s are
//! contended by request handlers concurrently.

use super::*;

/// Shared state accessible by all connection handlers.
pub(super) struct SharedState {
    /// IPC endpoint this daemon bound. Reported through `zccache status` so
    /// wrappers can verify they reached the intended daemon identity.
    pub(super) endpoint: String,
    /// running-process BackendHandle identity served on the same direct
    /// daemon endpoint for the minimal broker-adoption path.
    pub(super) backend_identity: running_process::broker::backend_handle::DaemonProcess,
    /// Active daemon/socket namespace label.
    pub(super) daemon_namespace: String,
    /// Cache root this daemon was created with.
    pub(super) cache_dir: NormalizedPath,
    /// Private daemon lifetime/ref-count state.
    pub(super) private_daemon: PrivateDaemonLifecycle,
    pub(super) sessions: SessionManager,
    pub(super) system_includes: Mutex<SystemIncludeCache>,
    /// Dependency graph: tracks include relationships and cache verdicts.
    ///
    /// **Wrapped in `ArcSwap` per #640** so that the on-disk-loaded graph
    /// can be installed *after* `DaemonServer::bind` has handed out
    /// `Arc<SharedState>` clones to spawned tasks — a constraint the prior
    /// `Arc::get_mut`-based `set_dep_graph` could not satisfy. The initial
    /// value is `Arc::new(DepGraph::default())`; the first
    /// [`DaemonServer::set_dep_graph`] call atomically swaps in the loaded
    /// graph. Subsequent calls also swap (no one-shot constraint).
    ///
    /// All reader access is `state.dep_graph.load().method(...)`. The
    /// `Guard<Arc<DepGraph>>` returned by `.load()` derefs to `&DepGraph`,
    /// so existing method calls work unchanged once the `.load()` is
    /// inserted. Cache that guard in a local when multiple methods on the
    /// same graph snapshot are needed in one logical operation, so a
    /// concurrent swap can't split the operation across two graph
    /// generations.
    pub(super) dep_graph: arc_swap::ArcSwap<crate::depgraph::DepGraph>,
    /// In-memory artifact cache: artifact_key_hex → artifact data.
    pub(super) artifacts: DashMap<String, CachedArtifact>,
    /// Metadata cache + change journal. The watcher feeds file-change events
    /// into this, which downgrades confidence so `lookup()` re-hashes on
    /// next access. Without the watcher, stat-verify on every `lookup()` is
    /// the fallback (correct but slower).
    pub(super) cache_system: CacheSystem,
    /// File watcher for proactive metadata invalidation.
    pub(super) watcher: Mutex<Option<NotifyWatcher>>,
    /// Directories currently being watched (avoid duplicate watches).
    pub(super) watched_dirs: Mutex<HashSet<NormalizedPath>>,
    /// Shutdown signal — shared so request handlers can trigger shutdown.
    pub(super) shutdown: Arc<Notify>,
    /// Epoch seconds of last client activity (for idle timeout).
    pub(super) last_activity: AtomicU64,
    /// Daemon start time (epoch seconds).
    pub(super) start_time: u64,
    /// Global stats collector.
    pub(super) stats: StatsCollector,
    /// Phase-level profiler for hot-path breakdown.
    pub(super) profiler: PhaseProfiler,
    /// On-disk artifact cache for hardlink optimization on cache hits.
    pub(super) artifact_dir: NormalizedPath,
    /// On-disk path for the persisted [`MetadataCache`] snapshot.
    ///
    /// Written on flush (`Clear`) and shutdown (`Shutdown`); read at
    /// daemon startup so warm-side daemons spawned after `soldr load`
    /// start with their fast path already populated instead of an
    /// empty `DashMap`. See `crate::fscache::persistence`.
    pub(super) metadata_path: NormalizedPath,
    /// Path used by [`CompilerHashCache`] for persistent (path, mtime, size,
    /// hash) snapshots. Issue #517 — eliminates the ~50-60 ms cold-path
    /// blake3 of the rustc binary on every first-after-restart compile.
    /// Loaded by `Lifecycle::new`, written on shutdown alongside
    /// `metadata.bin`.
    pub(super) compiler_hash_cache_path: NormalizedPath,
    /// Path used by [`SystemIncludeCache`] for persistent `(compiler_path,
    /// mtime, size) -> include_paths` snapshots. Issue #541 — saves the
    /// ~30-50 ms `<compiler> -v -E -x c++ NUL` spawn on every
    /// first-after-restart C/C++ compile. Loaded by `Lifecycle::new`,
    /// written on graceful shutdown alongside `metadata.bin`.
    pub(super) system_includes_cache_path: NormalizedPath,
    /// Temporary directory for injected depfiles.
    pub(super) depfile_tmpdir: NormalizedPath,
    /// Ultra-fast hit cache: context_key → (clock, artifact_key_hex, timestamp).
    /// When the journal clock hasn't advanced since the last verified hit,
    /// we skip all stat/hash/depgraph work and jump straight to artifact lookup.
    pub(super) fast_hit_cache: DashMap<ContextKey, FastHitEntry>,
    /// Whether the file watcher is active. Fast-hit cache is only used when
    /// the watcher is running, since we rely on it for change detection.
    pub(super) watcher_active: AtomicBool,
    /// Response file expansion cache keyed by canonical root path.
    /// Each entry carries the transitive response-file hashes required to
    /// validate freshness before reusing the cached expansion.
    pub(super) rsp_cache: DashMap<NormalizedPath, RspCacheEntry>,
    /// Request-level fast path cache: hash(compiler, args, cwd) → pre-computed context.
    /// When the same compile request is seen again and the fast-hit cache still
    /// holds a valid entry, this allows skipping ALL heavy work: system include
    /// discovery, watch_directories, response file expansion, arg parsing,
    /// context building, and dep_graph registration.
    pub(super) request_cache: DashMap<ContentHash, RequestCacheEntry>,
    /// Session-level worktree-root cache resolved once at SessionStart.
    pub(super) session_worktree_roots: DashMap<SessionId, SessionWorktreeRoot>,
    /// Cross-root request-cache validation: (request fingerprint, root) -> last
    /// verified artifact and journal clock. This lets repeated sibling hits
    /// validate with journal checks instead of re-hashing every input.
    pub(super) request_validation_cache: DashMap<RequestValidationKey, RequestValidationEntry>,
    /// Compiler executable hash cache keyed by compiler path.
    pub(super) compiler_hash_cache: CompilerHashCache,
    /// Pre-filter for watch_directories: raw (non-canonicalized) paths we've
    /// already processed. Avoids expensive canonicalize() syscalls (~1-5ms each
    /// on Windows) for directories that are already being watched.
    pub(super) watched_raw_dirs: DashMap<NormalizedPath, ()>,
    /// PCH source registry: pch_output_path → source_header_path.
    /// When a PCH generation succeeds, we record the mapping so that
    /// consuming compilations can hash the source header instead of the
    /// non-deterministic PCH binary.
    pub(super) pch_source_map: DashMap<NormalizedPath, NormalizedPath>,
    /// JSONL compile journal for build replay.
    pub(super) journal: CompileJournal,
    /// Bytes currently in spawn_blocking persistence tasks, invisible to eviction.
    pub(super) in_flight_bytes: AtomicUsize,
    /// Limits concurrent disk persistence tasks to prevent memory pileup
    /// when disk I/O is slow and compilation requests are fast.
    pub(super) persist_semaphore: Arc<tokio::sync::Semaphore>,
    /// In-memory artifact index (bincode blob-backed) for fast startup and
    /// persistence. Hot-path reads and writes go through `state.artifacts`;
    /// this store holds the same data and snapshots it to disk periodically.
    ///
    /// Arc-wrapped so the background index-writer task (see `index_writer_tx`)
    /// can hold its own clone for batched `insert` calls without contending
    /// with the request-handler path.
    pub(super) artifact_store: Arc<ArtifactStore>,
    /// Sender to the background index-writer task. Persist call-sites push
    /// `(key_hex, ArtifactIndex)` pairs here and return immediately; the
    /// writer task drains the channel and flushes to the on-disk blob in
    /// batches.
    ///
    /// Decouples the artifact-persist semaphore (which gates concurrent disk
    /// writes) from the periodic index snapshot, so a slow flush no longer
    /// holds a persist permit while other artifacts wait. See
    /// `tests/persist_pool_bench.rs` for the data motivating this split.
    pub(super) index_writer_tx: tokio::sync::mpsc::UnboundedSender<(String, ArtifactIndex)>,
    /// Notify the index-writer to drain its WAL and exit on graceful shutdown.
    /// Without this, the writer would only see the channel close after every
    /// `Arc<SharedState>` ref (including those held by spawned persist tasks)
    /// drops — which can race with runtime abort and lose unflushed entries.
    pub(super) index_writer_shutdown: Arc<Notify>,
    /// Whether the background artifact loading has completed.
    pub(super) artifacts_loaded: AtomicBool,
    /// Whether the `died-shutdown` lifecycle event has been written for this
    /// daemon. Under burst load (issue #726), many wedge-detecting clients
    /// race to send `Request::Shutdown` within a few milliseconds and each
    /// connection handler would otherwise write the same event — 25+ duplicate
    /// rows for a single death observed. Guard the write with a compare-and-swap
    /// so only the first Shutdown handler logs.
    pub(super) shutdown_event_logged: AtomicBool,
    /// Fingerprint manager: tracks per-watch dirty state for `zccache fp` commands.
    pub(super) fingerprint: FingerprintManager,
    /// Whether the in-memory dep graph is backed by a persisted snapshot.
    ///
    /// Set to `true` when the graph is loaded from disk on startup (via
    /// `set_dep_graph`) or when a periodic/shutdown save completes
    /// successfully. Surfaced in `DaemonStatus.dep_graph_persisted` so the
    /// CLI can distinguish "persisted graph" from "first-run, not yet flushed"
    /// without inferring it from the on-disk file size.
    pub(super) dep_graph_persisted: AtomicBool,
    /// Optional load-time warning to mirror into every session log.
    ///
    /// Populated by `set_depgraph_load_warning` when the daemon's startup load
    /// of the persisted depgraph fell back to a cold session (version
    /// mismatch, corrupt header, or unexpected I/O error). The string is
    /// emitted once per session into the per-session log (`last-session.log`)
    /// at `SessionStart` time so the cold fallback is never silent. Issue #320.
    pub(super) depgraph_load_warning: Mutex<Option<String>>,
    /// In-flight `Request::GenericToolExec` coalescing map (issue #272).
    ///
    /// Concurrent callers with the same exec cache key share a `Notify` here:
    /// the first caller spawns the tool and inserts; subsequent callers wait
    /// on the same `Notify` and re-attempt the cache lookup once it fires,
    /// guaranteeing the tool runs exactly once for the herd.
    pub(super) in_flight_exec: DashMap<String, Arc<Notify>>,
    /// Pending cache-write registry (issue #610, DD-025 condition 1).
    ///
    /// Keyed by `artifact_key_hex` — every cold-miss path that defers its
    /// `state.artifacts` insert into a `tokio::spawn` task **must** register
    /// an entry here *before* spawning and notify+remove after the spawned
    /// work completes. Concurrent lookups for the same key check the
    /// registry first: they may wait briefly on the `Notify` (~5 ms ceiling
    /// per condition 3's blast-radius bound) and then either re-attempt the
    /// in-memory lookup or fall through to "miss → recompile". The failure
    /// mode (DD-025 condition 2) is always a miss, never a wrong-hit — the
    /// artifact's content identity stays bound by `blake3` (DD-005); only
    /// the *publication* is deferred.
    ///
    /// At rest the map is empty. Entries live ≤ 100 ms (30× p99 of
    /// `depgraph_update_ns + persist_enqueue` from #605 iter T2). At most
    /// `persist_semaphore.available_permits()` entries may exist
    /// concurrently — the same semaphore that bounds existing C/C++ persist
    /// spawns provides the backpressure.
    ///
    /// On daemon restart the registry is empty: recovered state comes from
    /// the WAL + on-disk artifacts (DD-008 / DD-017). Crash-mid-flight
    /// safety is verified by the adversarial test
    /// `crash_mid_flight_recovery_never_surfaces_wrong_content` in
    /// `daemon/server/tests/deferred_cold_path.rs` (PR #618).
    ///
    pub(super) pending_cache_writes: DashMap<String, Arc<Notify>>,
}
