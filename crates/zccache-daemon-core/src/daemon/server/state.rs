//! `SharedState` — the daemon's central state object shared by every
//! request handler.
//!
//! Every connection handler receives an `Arc<SharedState>` and reads from
//! these fields directly. Most fields are append-only after `DaemonServer::bind`
//! (`sessions`, `journal`, `artifact_store`); the lock-free `DashMap`s are
//! contended by request handlers concurrently.

use super::*;

const STAGING_LOCK_FILE: &str = ".active.lock";
const CONFIGURED_STAGING_CHILD: &str = "zccache-staging";

/// Lock file naming the single live writer of a cache root (#1162).
const CACHE_ROOT_WRITER_LOCK_FILE: &str = ".writer.lock";

/// How old a staging directory with no `.active.lock` must be before the
/// cleaner may treat it as crash debris (soldr#1250).
///
/// `StagingRoot::new` creates the directory and only then opens its lock, so
/// for a brief moment a perfectly healthy staging root exists with no lock
/// file. A cleaner that removes lockless directories on sight deletes live
/// roots during that window, and the creating daemon's own `open` then fails
/// with `ENOENT`.
///
/// Absence of a lock file therefore cannot mean "abandoned" on its own — it
/// means "cannot judge yet". Age is what separates a directory being born
/// from one whose daemon died before it could write a lock: the create/lock
/// gap is a couple of syscalls, so anything older than this by orders of
/// magnitude is genuinely debris.
const STAGING_ABANDONED_MIN_AGE: std::time::Duration = std::time::Duration::from_secs(60);

/// Per-daemon private output staging. The held lock distinguishes an active
/// daemon from crash debris, so startup cleanup cannot remove another live
/// daemon's compiler outputs.
pub(super) struct StagingRoot {
    path: NormalizedPath,
    lock: Option<std::fs::File>,
}

impl StagingRoot {
    pub(super) fn new(
        cache_dir: &Path,
        configured_parent: Option<&Path>,
        instance: u64,
    ) -> std::io::Result<Self> {
        use fs2::FileExt;
        use std::io::Write;

        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let parent = configured_parent
            .map(|root| root.join(CONFIGURED_STAGING_CHILD))
            .unwrap_or_else(|| cache_dir.join("staging"));
        let path = parent.join(format!("{}-{instance}-{nonce}", std::process::id()));
        std::fs::create_dir_all(&path)?;
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(path.join(STAGING_LOCK_FILE))?;
        // Never wait behind a cleaner that observed this just-created
        // directory before we acquired its lock. Failing daemon startup is
        // safer than returning a staging root a concurrent cleaner unlinked.
        file.try_lock_exclusive()?;
        writeln!(file, "{}", std::process::id())?;
        Ok(Self {
            path: path.into(),
            lock: Some(file),
        })
    }

    pub(super) fn path(&self) -> &Path {
        self.path.as_path()
    }

    pub(super) fn cleanup_abandoned(&self) -> std::io::Result<usize> {
        self.cleanup_abandoned_older_than(STAGING_ABANDONED_MIN_AGE)
    }

    /// [`Self::cleanup_abandoned`] with an explicit minimum age for the
    /// lockless case, so tests can exercise both sides of the age gate
    /// without sleeping.
    fn cleanup_abandoned_older_than(&self, min_age: std::time::Duration) -> std::io::Result<usize> {
        use fs2::FileExt;

        let Some(parent) = self.path.parent() else {
            return Ok(0);
        };
        let mut removed = 0;
        for entry in std::fs::read_dir(parent)?.flatten() {
            let path = entry.path();
            if !path.is_dir() || path == self.path.as_path() {
                continue;
            }
            // Deliberately NOT `create(true)` (soldr#1250). Creating the lock
            // file here manufactures the very artifact whose absence should
            // have protected the directory: the cleaner would then find its
            // own brand-new file unlocked, conclude the root was abandoned,
            // and delete a staging root that another daemon is mid-way
            // through creating.
            let lock = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(path.join(STAGING_LOCK_FILE));
            let lock = match lock {
                Ok(lock) => lock,
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                    // No lock file: either a root being born, or debris from a
                    // daemon that died before writing one. Only age tells them
                    // apart, and guessing wrong deletes live output.
                    if staging_dir_is_older_than(&path, min_age) {
                        std::fs::remove_dir_all(&path)?;
                        removed += 1;
                    }
                    continue;
                }
                Err(_) => continue,
            };
            if lock.try_lock_exclusive().is_err() {
                continue;
            }
            FileExt::unlock(&lock)?;
            drop(lock);
            std::fs::remove_dir_all(&path)?;
            removed += 1;
        }
        Ok(removed)
    }
}

/// Is this staging directory old enough that a missing `.active.lock` can
/// only mean crash debris?
///
/// Errs toward "no": an unreadable mtime, or a clock that makes the directory
/// look like it is from the future, both return false. Skipping real debris
/// costs disk until the next pass; removing a live root costs a failed build.
fn staging_dir_is_older_than(path: &Path, min_age: std::time::Duration) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    let Ok(modified) = metadata.modified() else {
        return false;
    };
    let Ok(age) = std::time::SystemTime::now().duration_since(modified) else {
        return false;
    };
    age >= min_age
}

impl Drop for StagingRoot {
    fn drop(&mut self) {
        if let Some(lock) = self.lock.take() {
            let _ = fs2::FileExt::unlock(&lock);
            drop(lock);
        }
        let _ = std::fs::remove_dir_all(self.path.as_path());
    }
}

/// Exclusive claim on a cache root, held for as long as the daemon writes to
/// it (#1162 finding 1).
///
/// `ArtifactStore::flush` serializes the whole in-memory index and atomically
/// renames it over `index.bin` — no merge, no read-modify-write. Two writers on
/// one root therefore do not interleave, they *overwrite*: each flush discards
/// everything the other inserted. The blobs survive on disk but become
/// unreferenced, so the damage shows up much later as unexplained cold misses.
///
/// Nothing else excludes them. The embedded service uses a synthetic
/// `embedded:` endpoint and never binds IPC, so the IPC singleton lockfile does
/// not stop a standalone daemon from opening the same root — one stray
/// `zccache` compile against `ZCCACHE_CACHE_DIR=X` is enough.
///
/// Contention is a misconfiguration worth surfacing, not papering over, so
/// acquisition is `try_lock` and a loser refuses to start rather than silently
/// coexisting.
/// Release is explicit rather than `Drop`-driven because `Arc<SharedState>`
/// outlives daemon shutdown: background holders (index writer, maintenance,
/// loaders) keep clones alive after the server task has joined. Waiting for the
/// last `Arc` would hold the root long past the point where the daemon stopped
/// writing, and a sequential restart on the same root — which is legitimate,
/// and which the integration suite does — would be refused. `Drop` remains as
/// the crash backstop.
pub(super) struct CacheRootWriterLock {
    lock: std::sync::Mutex<Option<std::fs::File>>,
}

impl CacheRootWriterLock {
    /// Claim `cache_dir` for this process, or report who already holds it.
    ///
    /// Returns [`std::io::ErrorKind::WouldBlock`] when another live writer
    /// holds the root; `lifecycle::cache_root_error` preserves that kind, so
    /// callers can tell contention from a genuine filesystem fault.
    pub(super) fn acquire(cache_dir: &Path) -> std::io::Result<Self> {
        use fs2::FileExt;
        use std::io::Write;

        std::fs::create_dir_all(cache_dir)?;
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(cache_dir.join(CACHE_ROOT_WRITER_LOCK_FILE))?;
        if file.try_lock_exclusive().is_err() {
            crate::core::lifecycle::write_event_in_cache_root(
                cache_dir,
                "daemon_cache_root_contended",
                serde_json::json!({
                    "cache_root": cache_dir.display().to_string(),
                    "pid": std::process::id(),
                }),
            );
            tracing::warn!(
                cache_root = %cache_dir.display(),
                pid = std::process::id(),
                "cache root already has a live writer; refusing to start a second one"
            );
            return Err(std::io::Error::new(
                std::io::ErrorKind::WouldBlock,
                "another live daemon already holds this cache root as its writer",
            ));
        }
        // Best-effort provenance for whoever inspects the lock file; the lock
        // itself is what enforces exclusion, so a failed write is not fatal.
        let _ = file.set_len(0);
        let _ = writeln!(file, "{}", std::process::id());
        Ok(Self {
            lock: std::sync::Mutex::new(Some(file)),
        })
    }

    /// Give up the claim, so the next daemon on this root can take it.
    ///
    /// Call this once the daemon has finished its shutdown persistence — that
    /// is the moment it stops writing, which is what the claim actually
    /// guards. Idempotent, so the `Drop` backstop after an explicit release is
    /// a no-op.
    pub(super) fn release(&self) {
        let mut guard = self
            .lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(file) = guard.take() {
            let _ = fs2::FileExt::unlock(&file);
        }
    }
}

impl Drop for CacheRootWriterLock {
    fn drop(&mut self) {
        self.release();
    }
}

/// RAII marker covering one complete compile-cache request.
///
/// The compiler-child counter in `daemon::process` is intentionally narrower:
/// it excludes cache hits, pre-hashing, and post-compile publication. Metadata
/// GC uses this request-level counter to prefer quiet periods without making
/// correctness depend on finding one.
pub(super) struct ActiveCacheRequest<'a> {
    state: &'a SharedState,
}

impl Drop for ActiveCacheRequest<'_> {
    fn drop(&mut self) {
        if self
            .state
            .active_cache_requests
            .fetch_sub(1, Ordering::AcqRel)
            == 1
        {
            self.state.cache_requests_idle.notify_one();
        }
    }
}

/// Shared state accessible by all connection handlers.
pub(super) struct SharedState {
    /// IPC endpoint this daemon bound. Reported through `zccache status` so
    /// wrappers can verify they reached the intended daemon identity.
    pub(super) endpoint: String,
    /// running-process BackendHandle identity served on the same direct
    /// daemon endpoint for the minimal broker-adoption path. Slice 24
    /// of zccache#782: migrated to the `protocol_v2::backend_handle`
    /// namespace (upstream re-export of the cross-version-stable type).
    pub(super) backend_identity:
        running_process::broker::protocol_v2::backend_handle::DaemonProcess,
    /// Active daemon/socket namespace label.
    pub(super) daemon_namespace: String,
    /// Cache root this daemon was created with.
    pub(super) cache_dir: NormalizedPath,
    /// Private daemon lifetime/ref-count state.
    pub(super) private_daemon: PrivateDaemonLifecycle,
    pub(super) sessions: SessionManager,
    /// Request-owned staged telemetry for active tracked sessions.
    pub(super) session_staged_profiles:
        DashMap<SessionId, Arc<crate::daemon::staged_stats::StagedProfiler>>,
    /// Per-output-directory exclusion for the legacy link side-effect scan.
    ///
    /// The scan attributes every changed sibling to one link invocation, so
    /// two managed links must not overlap their snapshot/link/capture windows
    /// in the same directory. Weak values let idle directory locks disappear.
    pub(super) link_output_locks: DashMap<NormalizedPath, std::sync::Weak<Mutex<()>>>,
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
    /// Metadata-cache consumers active from request entry through hit
    /// materialization or miss publication (compile, link, generic exec, and
    /// caller-owned exec probes).
    pub(super) active_cache_requests: AtomicUsize,
    /// Wakes deferred metadata GC when the active request count reaches zero.
    pub(super) cache_requests_idle: Notify,
    /// Daemon start time (epoch seconds).
    pub(super) start_time: u64,
    /// Global stats collector.
    pub(super) stats: StatsCollector,
    /// Phase-level profiler for hot-path breakdown.
    pub(super) profiler: PhaseProfiler,
    /// On-disk artifact cache for hardlink optimization on cache hits.
    pub(super) artifact_dir: NormalizedPath,
    /// Private compiler/linker outputs, isolated from cache clear/eviction.
    pub(super) staging: StagingRoot,
    /// Exclusive writer claim on this cache root, so a second writer cannot
    /// clobber our flushes (#1162). Released explicitly once the shutdown
    /// drain has persisted everything; see [`CacheRootWriterLock::release`].
    pub(super) cache_root_lock: CacheRootWriterLock,
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
    /// Monotonic count of watcher-arm failures since daemon start (issue
    /// #1156). Surfaced in `zccache status` so an operator can see that a
    /// daemon is running on its slow paths without trawling startup logs.
    pub(super) watcher_degradations: AtomicU64,
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
    /// Session IDs that were explicitly ended in this daemon process, stamped
    /// with when they ended.
    ///
    /// A never-seen UUID is allowed to compile for wrapper recovery after a
    /// daemon restart, but an ID that this process already ended should not be
    /// accepted again.
    ///
    /// #1165: the value was `()`, which left a reaper nothing to age against —
    /// an entry was removed only if that exact id was created again, so a
    /// long-lived daemon kept one tombstone per completed session forever. It
    /// is stamped now so [`ENDED_SESSION_TTL`] can expire it.
    pub(super) ended_sessions: DashMap<SessionId, std::time::Instant>,
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
    /// Serializes background and host-requested disk-maintenance passes for
    /// this exact cache root.
    pub(super) disk_maintenance: Mutex<()>,
    /// Shared by publishers and exclusively owned by maintenance/Clear from
    /// cache-file mutation through index/live-map mutation.
    pub(super) artifact_publication: Arc<tokio::sync::RwLock<()>>,
    /// Reuses one shared OS staged-store lock for all active staged deliveries
    /// in this daemon and cache root. The final lease drop releases it, letting
    /// cross-process maintenance acquire the exclusive lock.
    pub(super) staged_materialization_lock:
        Arc<StdMutex<std::sync::Weak<StagedMaterializationLock>>>,
    /// Limits concurrent disk persistence tasks to prevent memory pileup
    /// when disk I/O is slow and compilation requests are fast.
    pub(super) persist_semaphore: Arc<tokio::sync::Semaphore>,
    /// Issue #813 / #816 — global compile-concurrency cap.
    /// `Some(sem)` enforces an upper bound on the number of compiler
    /// child processes the daemon will spawn at once across ALL
    /// clients; cap is `max(1, num_cpus - 1)` on interactive hosts,
    /// `num_cpus` on CI, overridable via `ZCCACHE_MAX_PARALLEL_COMPILES`.
    /// `None` when the override is `0` (or `unlimited`) — preserves the
    /// historical uncapped behavior for users who want it.
    pub(super) compile_concurrency: Option<Arc<tokio::sync::Semaphore>>,
    /// Shared admission for ordinary compiler children and exclusive
    /// admission for unusually memory-intensive C/Rust amalgamations.
    pub(super) compile_resource_gate: super::compile_resource_gate::CompileResourceGate,
    /// Optional host-only classifier for embedded compiler admission. It can
    /// request exclusivity but never owns or acquires the daemon's gates.
    pub(super) host_admission_classifier:
        Option<Arc<dyn super::compile_resource_gate::HostAdmissionClassifier>>,
    /// Issue #1216 — compile-queue counters backing the `CompileProgress`
    /// heartbeats. `tokio::sync::Semaphore` reports available permits but
    /// not its waiter count or original capacity, so the gate maintains
    /// its own gauge. Always present, even when the cap is disabled — an
    /// uncapped daemon still reports in-flight compiles.
    pub(super) compile_queue: Arc<super::compile_progress::CompileQueueGauge>,
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
    pub(super) index_writer_tx: tokio::sync::mpsc::UnboundedSender<IndexWriterCommand>,
    /// Notify the index-writer to drain its WAL and exit on graceful shutdown.
    /// Without this, the writer would only see the channel close after every
    /// `Arc<SharedState>` ref (including those held by spawned persist tasks)
    /// drops — which can race with runtime abort and lose unflushed entries.
    pub(super) index_writer_shutdown: Arc<Notify>,
    /// Whether the background artifact loading has completed.
    pub(super) artifacts_loaded: AtomicBool,
    /// Whether the background compiler-hash-cache load has completed.
    ///
    /// Issue #784: the on-disk snapshot is loaded post-lockfile from a
    /// `spawn_blocking` task. The shutdown save path checks this before
    /// calling `save_to_disk` — saving while the load is still pending
    /// could write a partial snapshot over the persisted file. False
    /// until the loader's `install()` runs; once true the in-memory
    /// DashMap is considered canonical for save.
    pub(super) compiler_hash_cache_loaded: AtomicBool,
    /// Whether the background `metadata.bin` load has completed.
    ///
    /// Issue #784 phase 2b: the on-disk metadata cache is loaded
    /// post-lockfile from a `spawn_blocking` task. The shutdown save
    /// path in `run.rs` checks this before persisting — saving while
    /// the load is still pending could write a partial snapshot over
    /// the on-disk file. Mirrors `compiler_hash_cache_loaded` above.
    pub(super) metadata_cache_loaded: AtomicBool,
    /// Whether the background system-include-cache load has completed.
    ///
    /// Issue #784 phase 2c: the on-disk snapshot is loaded post-lockfile
    /// from a `spawn_blocking` task. The shutdown save path in `run.rs`
    /// checks this before persisting — saving while the load is still
    /// pending could write a partial snapshot over the on-disk file.
    /// Mirrors `compiler_hash_cache_loaded` above.
    pub(super) system_includes_loaded: AtomicBool,
    /// Whether the on-disk artifact-index blob has been merged into
    /// the live `artifact_store`.
    ///
    /// Issue #784 phase 2d: `bind_with_cache_dir` constructs the store
    /// empty; a background `spawn_blocking` calls `load_from_disk`
    /// after the readiness lockfile. `lookup_artifact_with_disk_fallback`
    /// (in `util.rs`) also triggers a synchronous `load_from_disk` on
    /// the first cache-miss in the load window so the existing
    /// disk-fallback contract holds. Either call site flips this to
    /// `true` on completion; subsequent misses short-circuit instead
    /// of re-reading the blob.
    pub(super) artifact_store_loaded: AtomicBool,
    /// Whether the `died-shutdown` lifecycle event has been written for this
    /// daemon. Under burst load (issue #726), many wedge-detecting clients
    /// race to send `Request::Shutdown` within a few milliseconds and each
    /// connection handler would otherwise write the same event — 25+ duplicate
    /// rows for a single death observed. Guard the write with a compare-and-swap
    /// so only the first Shutdown handler logs.
    pub(super) shutdown_event_logged: AtomicBool,
    /// Latched once any shutdown source fires. Background tasks use this
    /// instead of consuming the public shutdown Notify, so legacy
    /// `shutdown_handle().notify_one()` callers still wake the accept loop.
    pub(super) shutdown_requested: AtomicBool,
    /// Latched when an index-writer send fails, i.e. the writer task is gone
    /// (#1177). The condition is permanent — the receiver does not come back —
    /// so this bounds the report to one per daemon instead of one per compile.
    pub(super) index_writer_gone: AtomicBool,
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
    /// Whether startup has finished classifying the persisted depgraph.
    ///
    /// `zccache-daemon` marks this false before it moves the disk load onto a
    /// background thread. Compile requests must not register/check contexts
    /// against the empty default graph while this is false, otherwise the
    /// first warm lookup races into the `cold_skip` path even though a valid
    /// persisted graph is about to be installed. Issue #798.
    pub(super) dep_graph_load_complete: AtomicBool,
    /// Wakes compile requests waiting for startup depgraph classification.
    pub(super) dep_graph_load_notify: Arc<Notify>,
    /// Optional load-time warning to mirror into every session log.
    ///
    /// Populated by `set_depgraph_load_warning` when the daemon's startup load
    /// of the persisted depgraph fell back to a cold session (version
    /// mismatch, corrupt header, or unexpected I/O error). The string is
    /// emitted once per session into the per-session log (`last-session.log`)
    /// at `SessionStart` time so the cold fallback is never silent. Issue #320.
    pub(super) depgraph_load_warning: StdMutex<Option<String>>,
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
    /// a publisher here *before* spawning and complete it after the spawned
    /// work finishes. Same-key publishers share an entry and are counted so
    /// one completion cannot hide another publisher. Proven-hit lookups wait
    /// for request-specific verdict/output readiness, bounded by the payload
    /// wait timeout, then either materialize or fall through to recompile.
    /// The failure mode (DD-025 condition 2) is always a miss, never a wrong-hit — the
    /// artifact's content identity stays bound by `blake3` (DD-005); only
    /// the *publication* is deferred.
    ///
    /// At rest the map is empty. Entries live until every registered
    /// publisher for the key completes; the persist semaphore bounds active
    /// persistence work and shutdown applies its own bounded drain.
    ///
    /// On daemon restart the registry is empty: recovered state comes from
    /// the WAL + on-disk artifacts (DD-008 / DD-017). Crash-mid-flight
    /// safety is verified by the adversarial test
    /// `crash_mid_flight_recovery_never_surfaces_wrong_content` in
    /// `daemon/server/tests/deferred_cold_path.rs` (PR #618).
    ///
    pub(super) pending_cache_writes: DashMap<String, pending_writes::PendingWrite>,
    /// Persistent caller-owned opaque exec results (#1433 / #838).
    /// Keys retain the `zccache-exec-probe-v1` derivation contract; values
    /// live in a dedicated namespace under this daemon's normal cache root.
    pub(super) exec_store: KvStore,
}

impl SharedState {
    pub(super) fn begin_cache_request(&self) -> ActiveCacheRequest<'_> {
        self.active_cache_requests.fetch_add(1, Ordering::AcqRel);
        ActiveCacheRequest { state: self }
    }

    pub(super) fn active_cache_requests(&self) -> usize {
        self.active_cache_requests.load(Ordering::Acquire)
    }

    /// Return the lock that owns legacy side-effect capture for `output_dir`.
    ///
    /// The returned `Arc` is independent of the DashMap entry guard so callers
    /// can await `lock_owned` without retaining a map shard lock.
    pub(super) fn link_output_lock(&self, output_dir: NormalizedPath) -> Arc<Mutex<()>> {
        match self.link_output_locks.entry(output_dir) {
            dashmap::mapref::entry::Entry::Occupied(mut entry) => {
                if let Some(lock) = entry.get().upgrade() {
                    lock
                } else {
                    let lock = Arc::new(Mutex::new(()));
                    entry.insert(Arc::downgrade(&lock));
                    lock
                }
            }
            dashmap::mapref::entry::Entry::Vacant(entry) => {
                let lock = Arc::new(Mutex::new(()));
                entry.insert(Arc::downgrade(&lock));
                lock
            }
        }
    }
}

#[cfg(test)]
mod staging_tests {
    use super::*;

    use std::time::Duration;

    /// Backdate a directory so the age gate sees it as debris without the
    /// test having to sleep.
    fn backdate(path: &Path, by: Duration) {
        let when = std::time::SystemTime::now() - by;
        filetime::set_file_mtime(path, filetime::FileTime::from_system_time(when)).unwrap();
    }

    /// #1162 finding 1: `index.bin` is last-writer-wins, so a second writer on
    /// one root silently discards the first's inserts. The second must be
    /// refused, and refused distinguishably — `cache_root_error` preserves the
    /// `ErrorKind`, so `WouldBlock` is what tells contention apart from a real
    /// filesystem fault.
    #[test]
    fn a_second_writer_is_refused_while_the_first_holds_the_root() {
        let temp = tempfile::tempdir().unwrap();
        let first = CacheRootWriterLock::acquire(temp.path()).unwrap();

        let second = CacheRootWriterLock::acquire(temp.path());
        let error = second.err().expect("a second writer must not be granted");
        assert_eq!(
            error.kind(),
            std::io::ErrorKind::WouldBlock,
            "contention must be distinguishable from a filesystem fault, got: {error}"
        );

        drop(first);
    }

    /// A sequential restart must be able to reclaim the root *while the old
    /// state is still alive*.
    ///
    /// The first cut of this released only on `Drop`, which looked fine in
    /// unit tests and then failed five daemon-restart integration tests:
    /// background holders keep `Arc<SharedState>` alive after the server task
    /// has joined, so the old claim outlived the daemon that owned it and the
    /// restart was refused with `WouldBlock`. Release is therefore explicit,
    /// at the end of the shutdown drain, and this test holds `first` across
    /// the reacquire to prove it does not depend on the drop.
    #[test]
    fn an_explicitly_released_root_is_reclaimable_while_the_old_lock_is_alive() {
        let temp = tempfile::tempdir().unwrap();
        let first = CacheRootWriterLock::acquire(temp.path()).unwrap();

        first.release();

        let _second = CacheRootWriterLock::acquire(temp.path())
            .expect("a released root must be reclaimable by the next daemon");

        // Held deliberately: the point is that release, not drop, freed it.
        drop(first);
    }

    /// Release runs again from `Drop`, so it must tolerate being called twice.
    #[test]
    fn releasing_twice_is_harmless() {
        let temp = tempfile::tempdir().unwrap();
        let lock = CacheRootWriterLock::acquire(temp.path()).unwrap();
        lock.release();
        lock.release();
    }

    /// The claim must be released on drop, or a daemon restart would be locked
    /// out of its own root by its predecessor's debris.
    #[test]
    fn dropping_the_writer_lock_releases_the_root_for_the_next_daemon() {
        let temp = tempfile::tempdir().unwrap();

        let first = CacheRootWriterLock::acquire(temp.path()).unwrap();
        drop(first);

        CacheRootWriterLock::acquire(temp.path())
            .expect("a released root must be claimable by the next writer");
    }

    /// Distinct roots are independent: the lock must not serialize unrelated
    /// daemons (or the isolated-root tests from #1254/#1261 would deadlock).
    #[test]
    fn distinct_cache_roots_do_not_contend() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();

        let _first = CacheRootWriterLock::acquire(a.path()).unwrap();
        CacheRootWriterLock::acquire(b.path())
            .expect("a different cache root must be claimable concurrently");
    }

    #[test]
    fn embedded_host_can_place_private_staging_outside_a_deep_cache_root() {
        let cache = tempfile::tempdir().unwrap();
        let staging = tempfile::tempdir().unwrap();

        let root = StagingRoot::new(cache.path(), Some(staging.path()), 7).unwrap();

        assert!(root.path().starts_with(staging.path()));
        assert!(root
            .path()
            .starts_with(staging.path().join(CONFIGURED_STAGING_CHILD)));
        assert!(!root.path().starts_with(cache.path()));
        assert!(root.path().join(STAGING_LOCK_FILE).is_file());
    }

    #[test]
    fn configured_staging_cleanup_is_bounded_to_the_owned_child() {
        let cache = tempfile::tempdir().unwrap();
        let configured = tempfile::tempdir().unwrap();
        let unrelated = configured.path().join("unrelated-user-data");
        std::fs::create_dir_all(&unrelated).unwrap();
        std::fs::write(unrelated.join("keep.txt"), b"keep").unwrap();

        let debris = configured
            .path()
            .join(CONFIGURED_STAGING_CHILD)
            .join("abandoned");
        std::fs::create_dir_all(&debris).unwrap();
        std::fs::write(debris.join("orphan.o"), b"orphan").unwrap();
        backdate(&debris, Duration::from_secs(3600));

        let cleaner = StagingRoot::new(cache.path(), Some(configured.path()), 1).unwrap();
        assert_eq!(cleaner.cleanup_abandoned().unwrap(), 1);
        assert!(unrelated.join("keep.txt").is_file());
        assert!(!debris.exists());
    }

    #[test]
    fn explicit_staging_roots_remain_independent_in_one_process() {
        let cache_a = tempfile::tempdir().unwrap();
        let cache_b = tempfile::tempdir().unwrap();
        let staging_a = tempfile::tempdir().unwrap();
        let staging_b = tempfile::tempdir().unwrap();

        let root_a = StagingRoot::new(cache_a.path(), Some(staging_a.path()), 1).unwrap();
        let root_b = StagingRoot::new(cache_b.path(), Some(staging_b.path()), 2).unwrap();

        assert!(root_a.path().starts_with(staging_a.path()));
        assert!(root_b.path().starts_with(staging_b.path()));
        assert!(!root_a.path().starts_with(staging_b.path()));
        assert!(!root_b.path().starts_with(staging_a.path()));
    }

    #[test]
    fn abandoned_cleanup_preserves_live_roots_and_removes_crash_debris() {
        let temp = tempfile::tempdir().unwrap();
        let live_a = StagingRoot::new(temp.path(), None, 1).unwrap();
        let live_b = StagingRoot::new(temp.path(), None, 2).unwrap();
        std::fs::write(live_b.path().join("active.o"), b"active").unwrap();

        let abandoned = temp.path().join("staging").join("abandoned");
        std::fs::create_dir_all(&abandoned).unwrap();
        std::fs::write(abandoned.join("orphan.o"), b"orphan").unwrap();
        // Debris now has to look old. This assertion used to pass on a
        // freshly created directory, which is exactly what made the race in
        // soldr#1250 reachable.
        backdate(&abandoned, Duration::from_secs(3600));

        assert_eq!(live_a.cleanup_abandoned().unwrap(), 1);
        assert!(live_b.path().join("active.o").exists());
        assert!(!abandoned.exists());
    }

    // soldr#1250: the window in `StagingRoot::new` between `create_dir_all`
    // and opening the lock.

    #[test]
    fn a_staging_root_being_born_survives_a_concurrent_cleaner() {
        let temp = tempfile::tempdir().unwrap();
        let cleaner = StagingRoot::new(temp.path(), None, 1).unwrap();

        // Exactly the on-disk state `StagingRoot::new` leaves behind after
        // `create_dir_all` and before it opens `.active.lock`.
        let being_born = temp.path().join("staging").join("999-0-12345");
        std::fs::create_dir_all(&being_born).unwrap();

        assert_eq!(
            cleaner.cleanup_abandoned().unwrap(),
            0,
            "a lockless directory that is seconds old is a root being born, not debris"
        );
        assert!(
            being_born.exists(),
            "deleting this is what makes the creating daemon fail ENOENT on its own lock"
        );
    }

    #[test]
    fn the_cleaner_does_not_create_the_lock_file_it_tests_for() {
        let temp = tempfile::tempdir().unwrap();
        let cleaner = StagingRoot::new(temp.path(), None, 1).unwrap();
        let being_born = temp.path().join("staging").join("999-0-12345");
        std::fs::create_dir_all(&being_born).unwrap();

        let _ = cleaner.cleanup_abandoned().unwrap();

        // Survival first. Without this line the assertion below passes
        // vacuously under the original bug -- the directory is gone, so of
        // course its lock file is absent. Confirmed by re-introducing
        // `create(true)`: this test passed while the other two failed.
        assert!(being_born.exists(), "precondition: the root must survive");
        // The original bug: `create(true)` manufactured the lock file, so the
        // cleaner then found its own new file unlocked and judged the root
        // abandoned. Absence must stay absence.
        assert!(
            !being_born.join(STAGING_LOCK_FILE).exists(),
            "the cleaner must not fabricate the artifact whose absence protects the root"
        );
    }

    #[test]
    fn lockless_debris_is_still_reclaimed_once_it_is_old_enough() {
        let temp = tempfile::tempdir().unwrap();
        let cleaner = StagingRoot::new(temp.path(), None, 1).unwrap();

        let debris = temp.path().join("staging").join("dead-0-1");
        std::fs::create_dir_all(&debris).unwrap();
        std::fs::write(debris.join("orphan.o"), b"orphan").unwrap();
        backdate(&debris, Duration::from_secs(3600));

        assert_eq!(cleaner.cleanup_abandoned().unwrap(), 1);
        assert!(
            !debris.exists(),
            "the age gate must not turn a leak into permanent debris"
        );
    }

    #[test]
    fn the_age_gate_is_what_decides_the_lockless_case() {
        let temp = tempfile::tempdir().unwrap();
        let cleaner = StagingRoot::new(temp.path(), None, 1).unwrap();
        let young = temp.path().join("staging").join("young-0-1");
        std::fs::create_dir_all(&young).unwrap();

        // Same directory, same run: preserved under a real threshold, taken
        // when the threshold is zero. Nothing else distinguishes the two.
        assert_eq!(
            cleaner
                .cleanup_abandoned_older_than(Duration::from_secs(60))
                .unwrap(),
            0
        );
        assert!(young.exists());
        assert_eq!(
            cleaner
                .cleanup_abandoned_older_than(Duration::ZERO)
                .unwrap(),
            1
        );
        assert!(!young.exists());
    }

    #[test]
    fn a_held_lock_still_protects_a_live_root_regardless_of_age() {
        let temp = tempfile::tempdir().unwrap();
        let cleaner = StagingRoot::new(temp.path(), None, 1).unwrap();
        let live = StagingRoot::new(temp.path(), None, 2).unwrap();
        std::fs::write(live.path().join("active.o"), b"active").unwrap();

        // Age must never override a held lock: a long-running daemon is old.
        backdate(live.path(), Duration::from_secs(3600));

        assert_eq!(cleaner.cleanup_abandoned().unwrap(), 0);
        assert!(live.path().join("active.o").exists());
    }
}
