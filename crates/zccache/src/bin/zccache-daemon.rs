//! zccache daemon process.
//!
//! The daemon maintains in-memory caches, manages the artifact store,
//! runs the file watcher, and handles IPC requests from CLI/wrappers.
//!
//! On the long-lived foreground path, the daemon releases its launch
//! handles (exe file lock on Windows, implicit cwd handle on all OSes)
//! via [`zccache::daemon::trampoline`] before entering [`run_server`].

#[cfg(unix)]
#[global_allocator]
static GLOBAL: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

#[cfg(windows)]
#[global_allocator]
static GLOBAL_WIN: mimalloc::MiMalloc = mimalloc::MiMalloc;

use clap::Parser;
use zccache::core::NormalizedPath;

const DAEMON_MAX_BLOCKING_THREADS: usize = 16;

/// zccache daemon -- local compiler cache service.
#[derive(Debug, Parser)]
#[command(name = "zccache-daemon", version, about)]
struct Args {
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
        default_value_t = zccache::core::config::DEFAULT_IDLE_TIMEOUT_SECS,
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

fn main() {
    let args = Args::parse();

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
            Some(path) => zccache::daemon::trampoline::redirect_stdio_to_log(path),
            None => zccache::daemon::trampoline::detach_stdio(),
        }
        init_tracing(&args.log_level);
        // Long-lived process: release exe-file lock and cwd handle so
        // `pip install --upgrade zccache` and `rm -rf <project>` can
        // succeed while the daemon is running. See issue #134.
        zccache::daemon::trampoline::unlock_exe();
        zccache::daemon::trampoline::release_cwd();
        run_server(args);
    } else {
        print_status(&args);
    }
}

fn print_status(args: &Args) {
    let endpoint = args
        .endpoint
        .clone()
        .unwrap_or_else(zccache::ipc::default_endpoint);

    println!("zccache-daemon v{}", env!("CARGO_PKG_VERSION"));
    println!();
    println!("  endpoint:   {endpoint}");
    println!(
        "  namespace:  {}",
        zccache::core::config::daemon_namespace_label()
    );
    println!("  lock file:  {}", zccache::ipc::lock_file_path().display());
    println!();

    // Try to connect and get status from a running daemon
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
) -> Result<zccache::protocol::DaemonStatus, Box<dyn std::error::Error>> {
    let mut conn = zccache::ipc::connect(endpoint).await?;
    // Client-style round trip: opt into the 5-minute default so a hung
    // daemon surfaces as a Timeout rather than blocking forever.
    conn.set_recv_timeout(zccache::ipc::DEFAULT_CLIENT_RECV_TIMEOUT);
    conn.send(&zccache::protocol::Request::Status).await?;
    let resp: Option<zccache::protocol::Response> = conn.recv().await?;
    match resp {
        Some(zccache::protocol::Response::Status(s)) => Ok(s),
        Some(other) => Err(format!("unexpected response: {other:?}").into()),
        None => Err("connection closed".into()),
    }
}

fn run_server(args: Args) {
    let endpoint = args.endpoint.unwrap_or_else(zccache::ipc::default_endpoint);
    let idle_timeout = args.idle_timeout;

    // The returned guard MUST stay alive — drop unregisters the
    // OS-level signal/exception handlers. Bind it for the whole
    // `run_server` lifetime by storing it in this stack frame.
    let _crash_guard = zccache::core::crash::install("zccache-daemon");
    zccache::core::crash::check_previous_crashes();

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
        let probe_rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("failed to create probe runtime");
        let existing_daemon = probe_rt.block_on(zccache::ipc::probe_existing_daemon(
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
    let cache_root = zccache::core::config::default_cache_dir();
    zccache::core::defender::maybe_emit_first_run_banner(cache_root.as_path());

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .max_blocking_threads(DAEMON_MAX_BLOCKING_THREADS)
        .build()
        .expect("failed to create tokio runtime");

    let no_depgraph_cache = args.no_depgraph_cache;
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
        // loop. Compile requests arriving during the load window see
        // the empty default graph and take the cold-path miss
        // (correct — the cache_system's stat-verify safety net catches
        // any stale-hit risk; sub-optimal speed for ~3 s and then the
        // post-load `set_dep_graph` atomically publishes the warm graph
        // for every subsequent `state.dep_graph.load()`).
        //
        // The pre-bind probe (#641) and bind-error race discrimination
        // (#639) both still fire in their original order — only the
        // depgraph load itself moves.

        // ── Issue #637/#639: discriminate loser-of-race from real bind failure ──
        let server = match zccache::daemon::DaemonServer::bind(&endpoint) {
            Ok(s) => s,
            Err(e) => {
                let is_pipe_in_use = matches!(
                    &e,
                    zccache::ipc::IpcError::Io(io_err) if matches!(
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
                zccache::ipc::remove_lock_file();
                std::process::exit(1);
            }
        };

        // Bind won — we are THE daemon. NOW write the spawn event and
        // lock file (loser-of-race exits at bind above without
        // polluting either).
        zccache::daemon::lifecycle::write_event(
            zccache::daemon::lifecycle::EVENT_SPAWN,
            serde_json::json!({
                "endpoint": &endpoint,
                "daemon_namespace": zccache::core::config::daemon_namespace_label(),
                "idle_timeout": idle_timeout,
                "version": env!("CARGO_PKG_VERSION"),
            }),
        );
        let pid = std::process::id();
        if let Err(e) = zccache::ipc::write_lock_file(pid) {
            tracing::warn!("failed to write lock file: {e}");
        }

        // Spawn the depgraph load. Holds a `DepGraphSetter` that survives
        // the spawn_blocking boundary; on completion atomically installs
        // the graph + warning via #642's ArcSwap.
        let setter = server.dep_graph_setter();
        let depgraph_path = zccache::depgraph::depgraph_file_path();
        let load_handle = tokio::task::spawn_blocking(move || {
            if no_depgraph_cache {
                let _ = std::fs::remove_file(&depgraph_path);
                tracing::info!("depgraph cache disabled — starting with empty graph");
                setter.install(None, None);
                return;
            }
            let start = std::time::Instant::now();
            let outcome = zccache::depgraph::classify_load(&depgraph_path);
            let warning = outcome.warning(&depgraph_path);
            match outcome {
                zccache::depgraph::DepGraphLoadOutcome::Loaded { graph } => {
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
                zccache::depgraph::DepGraphLoadOutcome::Missing => {
                    setter.install(None, None);
                }
                zccache::depgraph::DepGraphLoadOutcome::VersionMismatch {
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
                zccache::depgraph::DepGraphLoadOutcome::Corrupt { ref message }
                | zccache::depgraph::DepGraphLoadOutcome::IoError { ref message } => {
                    tracing::warn!("depgraph load failed: {message} — starting with empty graph");
                    if let Some(ref w) = warning {
                        eprintln!("{w}");
                    }
                    setter.install(None, warning);
                }
            }
        });

        // Wire up Ctrl+C to trigger graceful shutdown
        let shutdown = server.shutdown_handle();
        tokio::spawn(async move {
            if let Ok(()) = tokio::signal::ctrl_c().await {
                tracing::info!("received Ctrl+C — shutting down");
                shutdown.notify_one();
            }
        });

        tracing::info!(%endpoint, "listening for connections");

        // Take `mut server` here so `server.run(&mut self)` can borrow.
        let mut server = server;
        if let Err(e) = server.run(idle_timeout).await {
            tracing::error!("server error: {e}");
            zccache::ipc::remove_lock_file();
            // Best-effort: abort the background load if it hasn't
            // landed yet. If it has, the swap is harmless.
            load_handle.abort();
            std::process::exit(1);
        }

        tracing::info!("daemon exiting cleanly");
        zccache::ipc::remove_lock_file();
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
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(true)
        .with_thread_ids(true)
        .init();
}
