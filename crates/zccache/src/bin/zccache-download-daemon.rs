#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

use clap::Parser;
use zccache::download_protocol::daemon_mgmt;
use zccache::download_protocol::{Request, Response};

#[derive(Debug, Parser)]
#[command(name = "zccache-download-daemon", version, about)]
struct Args {
    #[arg(long, default_value = "info")]
    log_level: String,

    #[arg(long)]
    foreground: bool,

    #[arg(long)]
    endpoint: Option<String>,
}

fn main() {
    let args = Args::parse();
    if args.foreground {
        init_tracing(&args.log_level);
        run_server(args);
    } else {
        print_status(&args);
    }
}

fn print_status(args: &Args) {
    let endpoint = args
        .endpoint
        .clone()
        .unwrap_or_else(daemon_mgmt::default_endpoint);
    println!("zccache-download-daemon v{}", env!("CARGO_PKG_VERSION"));
    println!();
    println!("  endpoint:   {endpoint}");
    println!("  lock file:  {}", daemon_mgmt::lock_file_path().display());
    match query_daemon_status(&endpoint) {
        Ok(status) => {
            println!("  status:     running");
            println!("  uptime:     {}s", status.uptime_secs);
            println!("  downloads:  {}", status.active_downloads);
            println!("  clients:    {}", status.connected_clients);
        }
        Err(_) => {
            println!("  status:     not running");
        }
    }
}

/// Connect to a running daemon over IPC and query its status.
fn query_daemon_status(endpoint: &str) -> Result<zccache::download::DownloadDaemonStatus, String> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("failed to create runtime: {e}"))?;
    rt.block_on(async {
        let mut conn = zccache::ipc::connect(endpoint)
            .await
            .map_err(|e| format!("download daemon not running at {endpoint}: {e}"))?;
        conn.send(&Request::Status)
            .await
            .map_err(|e| format!("failed to query download daemon: {e}"))?;
        match conn.recv::<Response>().await {
            Ok(Some(Response::Status(status))) => Ok(status),
            Ok(Some(Response::Error { message })) => Err(message),
            Ok(Some(other)) => Err(format!("unexpected response: {other:?}")),
            Ok(None) => Err("download daemon closed connection unexpectedly".to_string()),
            Err(e) => Err(format!("broken connection to download daemon: {e}")),
        }
    })
}

fn run_server(args: Args) {
    let endpoint = args.endpoint.unwrap_or_else(daemon_mgmt::default_endpoint);
    let pid = std::process::id();
    if let Err(err) = daemon_mgmt::write_lock_file(pid) {
        tracing::warn!("failed to write download daemon lock file: {err}");
    }

    #[expect(
        clippy::expect_used,
        reason = "multi-thread runtime construction with enable_all only fails on OS resource exhaustion (timer/IO driver registration); download daemon cannot proceed by any other means at that point"
    )]
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("failed to create runtime");
    rt.block_on(async move {
        let bind_endpoint = endpoint.clone();
        let bind_result = tokio::task::spawn_blocking(move || {
            zccache::download_daemon::DownloadDaemon::bind(&bind_endpoint)
        })
        .await;
        let mut server = match bind_result {
            Ok(Ok(server)) => server,
            Ok(Err(err)) => {
                eprintln!("failed to bind download daemon at {endpoint}: {err}");
                daemon_mgmt::remove_lock_file();
                std::process::exit(1);
            }
            Err(err) => {
                eprintln!("failed to join download daemon bind worker for {endpoint}: {err}");
                daemon_mgmt::remove_lock_file();
                std::process::exit(1);
            }
        };
        let shutdown = server.shutdown_handle();
        tokio::spawn(async move {
            if let Ok(()) = tokio::signal::ctrl_c().await {
                shutdown.notify_one();
            }
        });
        if let Err(err) = server.run().await {
            eprintln!("download daemon error: {err}");
            daemon_mgmt::remove_lock_file();
            std::process::exit(1);
        }
        daemon_mgmt::remove_lock_file();
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
