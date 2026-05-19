//! zccache daemon process.
//!
//! The daemon maintains in-memory caches, manages the artifact store,
//! runs the file watcher, and handles IPC requests from CLI/wrappers.
//!
//! On the long-lived foreground path, the daemon releases its launch
//! handles (exe file lock on Windows, implicit cwd handle on all OSes)
//! via [`zccache_daemon::trampoline`] before entering [`run_server`].

#[cfg(unix)]
#[global_allocator]
static GLOBAL: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

#[cfg(windows)]
#[global_allocator]
static GLOBAL_WIN: mimalloc::MiMalloc = mimalloc::MiMalloc;

use clap::Parser;
use zccache_core::NormalizedPath;

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

    /// Idle timeout in seconds (0 = no timeout). Default: 3600.
    #[arg(long, default_value = "3600")]
    idle_timeout: u64,

    /// Disable loading/saving the dependency graph from/to disk.
    #[arg(long)]
    no_depgraph_cache: bool,
}

fn main() {
    let args = Args::parse();

    if args.foreground {
        // FIRST thing in the long-lived path: drop any stdio handles we
        // inherited from the spawning process. Without this, an orphaned
        // daemon keeps its grandparent's pipe write ends alive and the
        // grandparent's pipe reader (e.g. `subprocess.Popen(stdout=PIPE)`)
        // never sees EOF after the parent exits. See issue #276. Must run
        // before init_tracing() so the subscriber's stdout/stderr writes
        // land in the null device too.
        zccache_daemon::trampoline::detach_stdio();
        init_tracing(&args.log_level);
        // Long-lived process: release exe-file lock and cwd handle so
        // `pip install --upgrade zccache` and `rm -rf <project>` can
        // succeed while the daemon is running. See issue #134.
        zccache_daemon::trampoline::unlock_exe();
        zccache_daemon::trampoline::release_cwd();
        run_server(args);
    } else {
        print_status(&args);
    }
}

fn print_status(args: &Args) {
    let endpoint = args
        .endpoint
        .clone()
        .unwrap_or_else(zccache_ipc::default_endpoint);

    println!("zccache-daemon v{}", env!("CARGO_PKG_VERSION"));
    println!();
    println!("  endpoint:   {endpoint}");
    println!("  lock file:  {}", zccache_ipc::lock_file_path().display());
    println!();

    // Try to connect and get status from a running daemon
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("failed to create tokio runtime");

    match rt.block_on(query_daemon_status(&endpoint)) {
        Ok(status) => {
            println!("  status:     running");
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
) -> Result<zccache_protocol::DaemonStatus, Box<dyn std::error::Error>> {
    let mut conn = zccache_ipc::connect(endpoint).await?;
    // Client-style round trip: opt into the 5-minute default so a hung
    // daemon surfaces as a Timeout rather than blocking forever.
    conn.set_recv_timeout(zccache_ipc::DEFAULT_CLIENT_RECV_TIMEOUT);
    conn.send(&zccache_protocol::Request::Status).await?;
    let resp: Option<zccache_protocol::Response> = conn.recv().await?;
    match resp {
        Some(zccache_protocol::Response::Status(s)) => Ok(s),
        Some(other) => Err(format!("unexpected response: {other:?}").into()),
        None => Err("connection closed".into()),
    }
}

fn run_server(args: Args) {
    let endpoint = args.endpoint.unwrap_or_else(zccache_ipc::default_endpoint);
    let idle_timeout = args.idle_timeout;

    zccache_daemon::crash::install_panic_hook();
    zccache_daemon::crash::check_previous_crashes();

    tracing::info!(%endpoint, idle_timeout, "zccache-daemon starting");

    // Write lock file so CLI can detect us
    let pid = std::process::id();
    if let Err(e) = zccache_ipc::write_lock_file(pid) {
        tracing::warn!("failed to write lock file: {e}");
    }

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .max_blocking_threads(DAEMON_MAX_BLOCKING_THREADS)
        .build()
        .expect("failed to create tokio runtime");

    // Load dep graph from disk (before entering async block).
    let dep_graph = if args.no_depgraph_cache {
        let path = zccache_depgraph::depgraph_file_path();
        let _ = std::fs::remove_file(&path);
        tracing::info!("depgraph cache disabled — starting with empty graph");
        None
    } else {
        let path = zccache_depgraph::depgraph_file_path();
        let start = std::time::Instant::now();
        match zccache_depgraph::load_from_file(&path) {
            Ok(graph) => {
                let stats = graph.stats();
                tracing::info!(
                    contexts = stats.context_count,
                    files = stats.file_count,
                    elapsed_ms = start.elapsed().as_millis() as u64,
                    "loaded depgraph from disk"
                );
                Some(graph)
            }
            Err(zccache_depgraph::SnapshotError::Io(ref e))
                if e.kind() == std::io::ErrorKind::NotFound =>
            {
                None
            }
            Err(zccache_depgraph::SnapshotError::VersionMismatch { file, expected }) => {
                tracing::warn!(
                    file_version = file,
                    expected_version = expected,
                    "depgraph version mismatch — starting with empty graph"
                );
                None
            }
            Err(e) => {
                tracing::warn!("depgraph load failed: {e} — starting with empty graph");
                None
            }
        }
    };

    rt.block_on(async {
        let mut server = match zccache_daemon::DaemonServer::bind(&endpoint) {
            Ok(s) => s,
            Err(e) => {
                tracing::error!("failed to bind {endpoint}: {e}");
                zccache_ipc::remove_lock_file();
                std::process::exit(1);
            }
        };

        // Inject pre-loaded dep graph if we have one.
        if let Some(graph) = dep_graph {
            server.set_dep_graph(graph);
        }

        // Wire up Ctrl+C to trigger graceful shutdown
        let shutdown = server.shutdown_handle();
        tokio::spawn(async move {
            if let Ok(()) = tokio::signal::ctrl_c().await {
                tracing::info!("received Ctrl+C — shutting down");
                shutdown.notify_one();
            }
        });

        tracing::info!(%endpoint, "listening for connections");

        if let Err(e) = server.run(idle_timeout).await {
            tracing::error!("server error: {e}");
            zccache_ipc::remove_lock_file();
            std::process::exit(1);
        }

        tracing::info!("daemon exiting cleanly");
        zccache_ipc::remove_lock_file();
    });
}

fn init_tracing(level: &str) {
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(level));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(true)
        .with_thread_ids(true)
        .init();
}
