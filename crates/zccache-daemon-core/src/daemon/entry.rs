//! Standalone `zccache-daemon` process entry point, as a library function.
//!
//! Extracted from `src/bin/zccache-daemon.rs` (issue #997) so the daemon
//! `main` can be invoked from more than one binary — the transitional
//! `zccache-daemon` bin shim today, and the `argv[0]` multi-call dispatch in
//! the `zccache` binary (issue #998) next. The bin becomes a 3-line shim that
//! installs the global allocator and calls [`run`]; all behavior lives here.
//!
//! On the long-lived foreground path, the daemon releases its launch handles
//! (exe file lock on Windows, implicit cwd handle on all OSes) via
//! [`crate::daemon::trampoline`] before entering `run_server`.

use clap::Parser;

use crate::core::NormalizedPath;

/// Upper bound for the tokio multi-thread runtime's `spawn_blocking` pool.
///
/// **Sizing rationale (ISSUE-502, Linux Docker 2026-06-25):**
///
/// The previous hardcoded `16` was undersized for the daemon's actual
/// blocking-task fan-out. Three sources of `spawn_blocking` traffic share
/// this pool:
///
/// 1. **Cache-check fan-out** (`handle_compile_multi.rs:367-389`): a
///    50-file wave spawns up to 50 concurrent metadata/hash lookups.
/// 2. **Persist concurrency** (`persist_workers_default`): on Linux now
///    scales with host parallelism (ISSUE-501), so a 32-core box may
///    have 32+ persist tasks in flight.
/// 3. **Background loaders** (depgraph / compiler-hash / metadata /
///    system-includes / artifact-store): five concurrent `spawn_blocking`
///    tasks at daemon startup, plus per-request loader fall-throughs.
///
/// A 50-wide fan-out alone exhausted the prior 16-thread budget, leaving
/// persist + loader tasks queued behind cache lookups.
///
/// We compute `clamp(parallelism * 8, 128, 512)`. The `* 8` multiplier
/// (raised from `* 4`) and the floor lift (64 → 128) come from a
/// follow-up after PR #919 / ISSUE-502: the previous 4× / 64-floor sized
/// the pool tight against the *critical* paths (cache-check fan-out +
/// persist concurrency + loaders), leaving no headroom for *non-critical*
/// blocking work (hashing under `lookup_since_async`, watcher
/// canonicalize, per-request fs metadata calls). Doubling the multiplier
/// and floor moves those non-critical paths out of the queue tail so a
/// 50-wide cargo cold burst doesn't park hashing behind persist.
///
/// Resulting sizes:
/// - 1–16 cores → 128 (floor)
/// - 32 cores → 256
/// - ≥ 64 cores → 512 (ceiling — matches tokio's stated default).
fn daemon_max_blocking_threads() -> usize {
    let parallelism = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(8);
    parallelism.saturating_mul(8).clamp(128, 512)
}

const DAEMON_PROFILE_ENV: &str = "ZCCACHE_DAEMON_PROFILE";
const TOKIO_CONSOLE_PROFILE: &str = "tokio-console";
const TOKIO_CONSOLE_BIND_ENV: &str = "TOKIO_CONSOLE_BIND";
const TOKIO_CONSOLE_DEFAULT_BIND: &str = "127.0.0.1:6669";

/// zccache daemon -- local compiler cache service.
#[derive(Debug, Parser)]
#[command(name = "zccache-daemon", version, about)]
pub struct Args {
    /// Path to configuration file.
    #[arg(long)]
    config: Option<NormalizedPath>,

    /// Log level (trace, debug, info, warn, error).
    #[arg(long, default_value = "info")]
    log_level: String,

    /// Run in foreground (don't daemonize).
    #[arg(long)]
    foreground: bool,

    /// IPC endpoint (default: platform-specific).
    #[arg(long)]
    endpoint: Option<String>,

    /// Idle timeout in seconds (0 = no timeout).
    ///
    /// Default comes from `zccache::core::config::DEFAULT_IDLE_TIMEOUT_SECS`
    /// (60 minutes), kept as the single source of truth so `Config::default`
    /// and this flag never drift apart.
    ///
    /// Reads `ZCCACHE_IDLE_TIMEOUT_SECS` from the environment when the
    /// flag is not given. Setting the env var on `zccache-cli` propagates
    /// to the daemon via `spawn_daemon`'s inherited environment, so a
    /// caller can ask for a shorter idle window without touching the
    /// command line. `0` disables the timeout (daemon runs forever).
    #[arg(
        long,
        default_value_t = crate::core::config::DEFAULT_IDLE_TIMEOUT_SECS,
        env = "ZCCACHE_IDLE_TIMEOUT_SECS"
    )]
    idle_timeout: u64,

    /// Disable loading/saving the dependency graph from/to disk.
    #[arg(long)]
    no_depgraph_cache: bool,

    /// File path to redirect the daemon's own stdout + stderr onto.
    ///
    /// Set by `zccache-cli`'s `spawn_daemon` so that errors which fire
    /// before the lifecycle log / panic hook can attach (dyld failures on
    /// macOS, Gatekeeper kills, early-init panics) leave evidence on
    /// disk instead of disappearing into `/dev/null`. When unset the
    /// daemon falls back to the legacy detach-stdio behavior.
    #[arg(long)]
    log_file: Option<NormalizedPath>,
}

/// Parse argv and run the daemon: foreground server, or status print.
///
/// This is the former `fn main` of the `zccache-daemon` binary. The binary
/// shim installs the global allocator and calls this.
pub fn run() {
    let args = Args::parse();
    run_with(args);
}

/// Parse the daemon [`Args`] from an explicit argv and run. Used by the
/// hidden `zccache daemon-run …` escape hatch (#998): an argv[0]-independent
/// way to enter the daemon (debugging, `noexec` cache dirs, or platforms where
/// argv[0] is unreliable). The caller synthesizes a leading `zccache-daemon`
/// program-name token followed by the daemon flags.
pub fn run_from<I, T>(argv: I)
where
    I: IntoIterator<Item = T>,
    T: Into<std::ffi::OsString> + Clone,
{
    let args = Args::parse_from(argv);
    run_with(args);
}

/// Run the daemon from already-parsed [`Args`]. Split out so `argv[0]`
/// dispatch (#998) and tests can drive the daemon without going through
/// `Args::parse()` on the real process argv.
pub fn run_with(args: Args) {
    if args.foreground {
        // FIRST thing in the long-lived path: drop any stdio handles we
        // inherited from the spawning process. Without this, an orphaned
        // daemon keeps its grandparent's pipe write ends alive and the
        // grandparent's pipe reader (e.g. `subprocess.Popen(stdout=PIPE)`)
        // never sees EOF after the parent exits. See issue #276.
        //
        // When the CLI hands us a `--log-file` we redirect stdout +
        // stderr onto that file instead of `/dev/null` (stdin stays
        // nulled) so failures that fire before the lifecycle log /
        // panic hook still leave evidence on disk. Must run before
        // init_tracing() so the subscriber's writes land in the log
        // file too.
        match args.log_file.as_deref() {
            Some(path) => crate::daemon::trampoline::redirect_stdio_to_log(path),
            None => crate::daemon::trampoline::detach_stdio(),
        }
        init_tracing(&args.log_level);
        // Long-lived process: release exe-file lock and cwd handle so
        // `pip install --upgrade zccache` and `rm -rf <project>` can
        // succeed while the daemon is running. See issue #134.
        crate::daemon::trampoline::unlock_exe();
        crate::daemon::trampoline::release_cwd();
        run_server(args);
    } else {
        print_status(&args);
    }
}

fn print_status(args: &Args) {
    let endpoint = args
        .endpoint
        .clone()
        .unwrap_or_else(crate::ipc::default_endpoint);

    println!("zccache-daemon v{}", env!("CARGO_PKG_VERSION"));
    println!();
    println!("  endpoint:   {endpoint}");
    println!(
        "  namespace:  {}",
        crate::core::config::daemon_namespace_label()
    );
    println!("  lock file:  {}", crate::ipc::lock_file_path().display());
    println!();

    // Try to connect and get status from a running daemon
    #[expect(
        clippy::expect_used,
        reason = "current-thread runtime construction with enable_all only fails on OS resource exhaustion; daemon-status query cannot proceed without it"
    )]
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("failed to create tokio runtime");

    match rt.block_on(query_daemon_status(&endpoint)) {
        Ok(status) => {
            println!("  status:     running");
            println!("  daemon ns:  {}", status.daemon_namespace);
            println!("  daemon ep:  {}", status.endpoint);
            println!(
                "  private:    {}",
                if status.private_daemon.enabled {
                    "yes"
                } else {
                    "no"
                }
            );
            println!("  uptime:     {}s", status.uptime_secs);
            println!("  artifacts:  {}", status.artifact_count);
            println!("  cache size: {} bytes", status.cache_size_bytes);
            println!("  metadata:   {} entries", status.metadata_entries);
            println!(
                "  hits/miss:  {} / {}",
                status.cache_hits, status.cache_misses
            );
        }
        Err(_) => {
            println!("  status:     not running");
            println!();
            println!("Start with: zccache-daemon --foreground");
        }
    }
}

async fn query_daemon_status(
    endpoint: &str,
) -> Result<crate::protocol::DaemonStatus, Box<dyn std::error::Error>> {
    let resp = crate::ipc::daemon_control_roundtrip(
        endpoint,
        crate::ipc::DaemonControlRequest::Status,
        None,
    )
    .await?;
    match resp {
        Some(crate::protocol::Response::Status(s)) => Ok(s),
        Some(other) => Err(format!("unexpected response: {other:?}").into()),
        None => Err("connection closed".into()),
    }
}

fn run_server(args: Args) {
    let endpoint = args.endpoint.unwrap_or_else(crate::ipc::default_endpoint);
    let idle_timeout = args.idle_timeout;

    // The returned guard MUST stay alive — drop unregisters the
    // OS-level signal/exception handlers. Bind it for the whole
    // `run_server` lifetime by storing it in this stack frame.
    let _crash_guard = crate::core::crash::install("zccache-daemon");
    crate::core::crash::check_previous_crashes();

    tracing::info!(%endpoint, idle_timeout, "zccache-daemon starting");

    // ── Issue #640: pre-bind probe to cut second-wave spawn herd ─────────
    //
    // Before paying the 3+ s depgraph-load cost, check whether another
    // daemon is already serving at this endpoint. A short async probe
    // (timeout: 500 ms) is enough — if it succeeds we know a live
    // daemon owns the pipe and we can exit cleanly without writing a
    // spawn event, lock file, or doing any heavy init.
    //
    // This DOES NOT help the initial cohort racing for the bind from a
    // cold start (none of them have written a lock file yet); those
    // are still caught by the post-bind PermissionDenied → INFO+exit 0
    // path landed in #639. What it DOES catch is the second wave:
    // every daemon spawn that fires AFTER one daemon has already
    // registered its lock file. In the user's #637 repro (277 spawns
    // over 3 hours, spaced anywhere from 11 ms to ~10 s apart) this
    // is most of the herd.
    //
    // A short-lived tokio runtime hosts the async probe so we don't
    // need to bring up the full multi-thread runtime before deciding
    // whether to defer.
    {
        #[expect(
            clippy::expect_used,
            reason = "current-thread probe runtime construction with enable_all only fails on OS resource exhaustion; daemon-defer probe cannot proceed without it"
        )]
        let probe_rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("failed to create probe runtime");
        let existing_daemon = probe_rt.block_on(crate::ipc::probe_existing_daemon(
            &endpoint,
            std::time::Duration::from_millis(500),
        ));
        if existing_daemon {
            tracing::info!(
                %endpoint,
                "active daemon already serving — deferring"
            );
            // Do NOT remove the lock file — it belongs to the live daemon.
            std::process::exit(0);
        }
    }

    // Issue #273: on Windows, warn once on stderr if the cache dir is
    // not on Defender's exclusion list. Non-fatal; no-ops off Windows
    // and when `ZCCACHE_QUIET` is set.
    let cache_root = crate::core::config::default_cache_dir();
    let daemon_state_root = crate::core::config::daemon_state_dir();
    tracing::info!(
        daemon_namespace = %crate::core::config::daemon_namespace_label(),
        mutable_state_dir = %daemon_state_root.display(),
        "resolved daemon cache state"
    );
    crate::core::defender::maybe_emit_first_run_banner(cache_root.as_path());

    #[expect(
        clippy::expect_used,
        reason = "multi-thread runtime construction with enable_all only fails on OS resource exhaustion; daemon cannot proceed without it"
    )]
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .max_blocking_threads(daemon_max_blocking_threads())
        .build()
        .expect("failed to create tokio runtime");

    let no_depgraph_cache = args.no_depgraph_cache;
    // Cache root for the v1 CacheManifest published after a successful bind
    // (zackees/running-process#435); cloned so it survives the async move.
    let manifest_cache_root = cache_root.clone();
    rt.block_on(async move {
        // ── Issue #640: bind FIRST, then load depgraph in background ─────
        //
        // The prior flow loaded the depgraph (3+ s on a populated cache)
        // BEFORE the bind, so for the entire load window every parallel
        // `ninja -jN` client whose connect failed ALSO spawned a daemon.
        // 277 redundant spawns per FastLED build per #637.
        //
        // With #642's `ArcSwap<DepGraph>` foundation, `set_dep_graph` is
        // now `&self` + atomic-swap-safe, so the load can fire from a
        // `spawn_blocking` AFTER `server.run()` has started the accept
        // loop. Compile requests arriving during the load window wait
        // until the persisted graph has been classified and published,
        // avoiding the fresh-daemon `cold_skip` race fixed in zccache#798.
        //
        // The pre-bind probe (#641) and bind-error race discrimination
        // (#639) both still fire in their original order — only the
        // depgraph load itself moves.

        // ── Issue #637/#639: discriminate loser-of-race from real bind failure ──
        let bind_endpoint = endpoint.clone();
        let bind_result =
            tokio::task::spawn_blocking(move || crate::daemon::DaemonServer::bind(&bind_endpoint))
                .await;
        let server = match bind_result {
            Ok(Ok(s)) => s,
            Ok(Err(e)) => {
                let is_pipe_in_use = matches!(
                    &e,
                    crate::ipc::IpcError::Io(io_err) if matches!(
                        io_err.kind(),
                        std::io::ErrorKind::PermissionDenied
                            | std::io::ErrorKind::AddrInUse
                            | std::io::ErrorKind::AlreadyExists
                    )
                );
                if is_pipe_in_use {
                    tracing::info!(
                        %endpoint,
                        error = %e,
                        "another daemon already owns endpoint — deferring"
                    );
                    // Do NOT remove the lock file — it belongs to the
                    // winning daemon, not us.
                    std::process::exit(0);
                }
                tracing::error!("failed to bind {endpoint}: {e}");
                crate::ipc::remove_lock_file();
                std::process::exit(1);
            }
            Err(e) => {
                tracing::error!("failed to join daemon bind worker for {endpoint}: {e}");
                crate::ipc::remove_lock_file();
                std::process::exit(1);
            }
        };

        // Bind won — we are THE daemon. NOW write the spawn event and
        // lock file (loser-of-race exits at bind above without
        // polluting either).
        let spawn_meta = crate::daemon::lifecycle::client_meta(env!("CARGO_PKG_VERSION"));
        crate::daemon::lifecycle::write_event(
            crate::daemon::lifecycle::EVENT_SPAWN,
            serde_json::json!({
                "endpoint": &endpoint,
                "daemon_namespace": crate::core::config::daemon_namespace_label(),
                "idle_timeout": idle_timeout,
                "version": env!("CARGO_PKG_VERSION"),
                // #755 acceptance #4: the daemon binary that bound
                // this endpoint, so coexisting fbuild-bundled vs.
                // PyPI installs land distinguishable rows.
                "client_version": spawn_meta["client_version"],
                "client_binary_path": spawn_meta["client_binary_path"],
            }),
        );
        let pid = std::process::id();
        if let Err(e) = crate::ipc::write_lock_file(pid) {
            tracing::warn!("failed to write lock file: {e}");
        }
        // #1005: advisory top-level marker of the last version that bound.
        // Diagnostics / warm-start hint only, never authoritative for identity.
        if let Err(e) = crate::core::config::write_last_version_marker() {
            tracing::debug!("failed to write last-version marker: {e}");
        }
        if let Err(e) = crate::ipc::write_backend_identity(&server.backend_identity()) {
            tracing::warn!("failed to write running-process backend identity: {e}");
        }
        // Publish the v1 CacheManifest so broker peers can discover this
        // daemon's cache roots (zackees/running-process#435). Best-effort:
        // a registry write failure is logged inside publish_manifest and
        // never blocks startup. Skipped under RUNNING_PROCESS_DISABLE=1.
        if let Some(path) = crate::ipc::publish_manifest(&manifest_cache_root) {
            tracing::debug!(manifest = %path.display(), "published running-process cache manifest");
        }

        // Install the ServiceDefinition into the running-process service-
        // definition dir so the shared broker can discover us without the
        // user running `zccache install-servicedef` manually (#720 Phase 2).
        // Idempotent + best-effort like publish_manifest above; a write
        // failure is logged and ignored. Skipped under
        // RUNNING_PROCESS_DISABLE=1. The version-policy refinement (#720
        // Phase 0) is the follow-up that turns the current exact-version
        // pin into a real compatibility floor + range.
        if let Ok(binary) = std::env::current_exe() {
            let _ = crate::ipc::publish_service_definition(&binary);
        }

        // Spawn the depgraph load. Holds a `DepGraphSetter` that survives
        // the spawn_blocking boundary; on completion atomically installs
        // the graph + warning via #642's ArcSwap.
        server.mark_dep_graph_load_pending();
        let setter = server.dep_graph_setter();
        let depgraph_path = crate::depgraph::depgraph_file_path();
        let load_handle = tokio::task::spawn_blocking(move || {
            if no_depgraph_cache {
                let _ = std::fs::remove_file(&depgraph_path);
                tracing::info!("depgraph cache disabled — starting with empty graph");
                setter.install(None, None);
                return;
            }
            let start = std::time::Instant::now();
            let outcome = crate::depgraph::classify_load(&depgraph_path);
            let warning = outcome.warning(&depgraph_path);
            match outcome {
                crate::depgraph::DepGraphLoadOutcome::Loaded { graph } => {
                    let stats = graph.stats();
                    let (cold_ctxs, warm_ctxs, stale_ctxs) = graph.state_breakdown();
                    let ctxs_with_key = graph.contexts_with_artifact_key();
                    tracing::info!(
                        contexts = stats.context_count,
                        files = stats.file_count,
                        cold = cold_ctxs,
                        warm = warm_ctxs,
                        stale = stale_ctxs,
                        with_artifact_key = ctxs_with_key,
                        elapsed_ms = start.elapsed().as_millis() as u64,
                        "loaded depgraph from disk (background)"
                    );
                    setter.install(Some(graph), None);
                }
                crate::depgraph::DepGraphLoadOutcome::Missing => {
                    setter.install(None, None);
                }
                crate::depgraph::DepGraphLoadOutcome::VersionMismatch {
                    file_version,
                    expected_version,
                } => {
                    tracing::warn!(
                        file_version,
                        expected_version,
                        "depgraph version mismatch — starting with empty graph"
                    );
                    if let Some(ref w) = warning {
                        eprintln!("{w}");
                    }
                    setter.install(None, warning);
                }
                crate::depgraph::DepGraphLoadOutcome::Corrupt { ref message }
                | crate::depgraph::DepGraphLoadOutcome::IoError { ref message } => {
                    tracing::warn!("depgraph load failed: {message} — starting with empty graph");
                    if let Some(ref w) = warning {
                        eprintln!("{w}");
                    }
                    setter.install(None, warning);
                }
            }
        });

        // Issue #784: move the compiler-hash cache load off the
        // spawn→lockfile critical path, using the same `spawn_blocking`
        // shape as the depgraph load above. The loader logs at WARN on
        // disk failure and leaves the in-memory cache empty; the
        // stat-verify in `get_or_hash_with` keeps cache-key correctness
        // either way. Fire-and-forget — the JoinHandle is dropped; if
        // shutdown fires mid-load, the shutdown save path checks the
        // `compiler_hash_cache_loaded` flag and skips the write so the
        // on-disk snapshot is preserved (see `server::run`'s shutdown
        // arm).
        let compiler_hash_loader = server.compiler_hash_cache_loader();
        tokio::task::spawn_blocking(move || {
            compiler_hash_loader.load_and_install();
        });

        // Issue #784 phase 2b: same shape for the on-disk `metadata.bin`
        // snapshot. This is the biggest of the four #784 deferrals —
        // load time scales with the number of cached files, so on a
        // FastLED-scale cache it was the dominant cost between bind and
        // lockfile. Same shutdown-save gating as the compiler-hash
        // loader: `metadata_cache_loaded` flag tells `server::run`'s
        // shutdown arm whether the in-memory state is canonical.
        let metadata_loader = server.metadata_cache_loader();
        tokio::task::spawn_blocking(move || {
            metadata_loader.load_and_install();
        });

        // Issue #784 phase 2c: same shape for the on-disk
        // system-includes snapshot. The loader briefly acquires the
        // `system_includes: Mutex<...>` via `blocking_lock()` to merge
        // entries from the loaded cache into the live one. Same
        // shutdown-save gating as the compiler-hash loader:
        // `system_includes_loaded` tells `server::run`'s shutdown arm
        // whether the in-memory state is canonical.
        let system_includes_loader = server.system_includes_loader();
        tokio::task::spawn_blocking(move || {
            system_includes_loader.load_and_install();
        });

        // Issue #784 phase 2d: same shape for the on-disk artifact
        // index blob. The store was constructed empty in
        // `bind_with_cache_dir`; the loader holds an
        // `Arc<ArtifactStore>` clone and inserts the on-disk entries
        // directly into the live `DashMap` request handlers read.
        // Coexists with the synchronous on-demand load in
        // `util::lookup_artifact_with_disk_fallback`: both paths do
        // identical `DashMap::insert` operations, so racing them
        // produces converged state. The `artifact_store_loaded` flag
        // prevents redundant disk reads.
        let artifact_store_loader = server.artifact_store_loader();
        tokio::task::spawn_blocking(move || {
            artifact_store_loader.load_and_install();
        });

        // Wire up Ctrl+C to trigger graceful shutdown
        let shutdown = server.shutdown_handle();
        tokio::spawn(async move {
            if let Ok(()) = tokio::signal::ctrl_c().await {
                tracing::info!("received Ctrl+C — shutting down");
                shutdown.notify_waiters();
            }
        });

        tracing::info!(%endpoint, "listening for connections");

        // Take `mut server` here so `server.run(&mut self)` can borrow.
        let mut server = server;
        if let Err(e) = server.run(idle_timeout).await {
            tracing::error!("server error: {e}");
            crate::ipc::remove_lock_file();
            // Best-effort: abort the background load if it hasn't
            // landed yet. If it has, the swap is harmless.
            load_handle.abort();
            std::process::exit(1);
        }

        tracing::info!("daemon exiting cleanly");
        crate::ipc::remove_lock_file();
        // The load may still be running if shutdown came in <3 s after
        // start; abort is safe (nothing for the swap to corrupt).
        load_handle.abort();
    });
}

fn init_tracing(level: &str) {
    use tracing_subscriber::EnvFilter;
    let mut filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(level));
    // When a parent process (notably soldr) launches us with a narrowed
    // `RUST_LOG=zccache_daemon=info`, the directive *only* matches the
    // `zccache_daemon` target — INFO logs emitted from sibling crates
    // (`zccache_artifact`, `zccache_fscache`, `zccache_hash`, ...) are
    // silently dropped, which has blocked perf-cluster diagnostics
    // (runs 26255457227 / 26258412256 / 26260816043 — see PERF.md).
    // Add explicit `<crate>=info` directives so the cross-crate logs
    // always survive the filter regardless of how the env was set.
    for target in [
        "zccache_artifact",
        "zccache_compiler",
        "zccache_core",
        "zccache_depgraph",
        "zccache_download",
        "zccache_fingerprint",
        "zccache_fscache",
        "zccache_gha",
        "zccache_hash",
        "zccache_ipc",
        "zccache_protocol",
        "zccache_symbols",
        "zccache_watcher",
    ] {
        if let Ok(d) = format!("{target}=info").parse() {
            filter = filter.add_directive(d);
        }
    }
    let profile = std::env::var(DAEMON_PROFILE_ENV).ok();
    let tokio_console_enabled = profile.as_deref() == Some(TOKIO_CONSOLE_PROFILE);

    if tokio_console_enabled {
        let bind = std::env::var(TOKIO_CONSOLE_BIND_ENV)
            .unwrap_or_else(|_| TOKIO_CONSOLE_DEFAULT_BIND.to_string());
        init_tokio_console_tracing(filter, &bind);
        return;
    }

    init_fmt_tracing(filter);

    if let Some(profile) = profile {
        if profile != TOKIO_CONSOLE_PROFILE {
            tracing::warn!(
                profile,
                "unknown daemon profile requested; running without extra profiling"
            );
        }
    }
}

fn init_fmt_tracing(filter: tracing_subscriber::EnvFilter) {
    use tracing_subscriber::prelude::*;

    tracing_subscriber::registry()
        .with(
            tracing_subscriber::fmt::layer()
                .with_target(true)
                .with_thread_ids(true)
                .with_filter(filter),
        )
        .init();
}

#[cfg(feature = "tokio-console")]
fn init_tokio_console_tracing(filter: tracing_subscriber::EnvFilter, bind: &str) {
    use tracing_subscriber::prelude::*;

    let console_layer = std::panic::catch_unwind(console_subscriber::spawn);
    match console_layer {
        Ok(console_layer) => {
            tracing_subscriber::registry()
                .with(console_layer)
                .with(
                    tracing_subscriber::fmt::layer()
                        .with_target(true)
                        .with_thread_ids(true)
                        .with_filter(filter),
                )
                .init();
            tracing::info!(
                bind,
                "tokio-console daemon profile enabled; connect with `tokio-console {bind}`"
            );
        }
        Err(_) => {
            init_fmt_tracing(filter);
            tracing::warn!(
                bind,
                "tokio-console daemon profile requested but unavailable; \
                 rebuild with RUSTFLAGS=\"--cfg tokio_unstable\""
            );
        }
    }
}

#[cfg(not(feature = "tokio-console"))]
fn init_tokio_console_tracing(filter: tracing_subscriber::EnvFilter, bind: &str) {
    init_fmt_tracing(filter);
    tracing::warn!(
        bind,
        "tokio-console daemon profile requested but zccache was built without \
         the `tokio-console` feature"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    // #997: the daemon arg parser is now library-testable. Before the
    // extraction this lived in a bin and could not be unit-tested.
    #[test]
    fn args_default_is_background_status_mode() {
        let args = Args::parse_from(["zccache-daemon"]);
        assert!(!args.foreground);
        assert_eq!(args.log_level, "info");
        assert!(args.endpoint.is_none());
    }

    #[test]
    fn args_foreground_and_endpoint_parse() {
        let args = Args::parse_from([
            "zccache-daemon",
            "--foreground",
            "--endpoint",
            "test-endpoint",
            "--idle-timeout",
            "0",
        ]);
        assert!(args.foreground);
        assert_eq!(args.endpoint.as_deref(), Some("test-endpoint"));
        assert_eq!(args.idle_timeout, 0);
    }
}
