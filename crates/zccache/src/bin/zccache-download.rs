use std::path::PathBuf;

use clap::{Parser, Subcommand};
use zccache::download::DownloadOptions;
use zccache::download_client::{
    is_terminal, ArchiveFormat, DownloadClient, DownloadSource, FetchRequest, FetchResult,
    FetchState, WaitMode,
};

#[derive(Debug, Parser)]
#[command(name = "zccache-download", version, about)]
struct Cli {
    #[arg(long)]
    endpoint: Option<String>,

    #[arg(long)]
    json: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    Get {
        url: String,
        destination: PathBuf,
        #[arg(long)]
        detach: bool,
        #[arg(long)]
        force: bool,
        #[arg(long)]
        max_connections: Option<usize>,
        #[arg(long)]
        min_segment_size: Option<u64>,
    },
    Fetch {
        #[arg(long)]
        url: Option<String>,
        #[arg(long = "part-url")]
        part_urls: Vec<String>,
        destination: PathBuf,
        #[arg(long)]
        expanded: Option<PathBuf>,
        #[arg(long)]
        expected_sha256: Option<String>,
        #[arg(long, default_value = "auto")]
        archive_format: String,
        #[arg(long)]
        no_wait: bool,
        #[arg(long)]
        dry_run: bool,
        #[arg(long)]
        force: bool,
        #[arg(long)]
        max_connections: Option<usize>,
        #[arg(long)]
        min_segment_size: Option<u64>,
    },
    Exists {
        #[arg(long)]
        url: Option<String>,
        #[arg(long = "part-url")]
        part_urls: Vec<String>,
        destination: PathBuf,
        #[arg(long)]
        expanded: Option<PathBuf>,
        #[arg(long)]
        expected_sha256: Option<String>,
        #[arg(long, default_value = "auto")]
        archive_format: String,
    },
    Wait {
        url: String,
        destination: PathBuf,
        #[arg(long)]
        timeout_ms: Option<u64>,
    },
    Status {
        url: String,
        destination: PathBuf,
    },
    Cancel {
        url: String,
        destination: PathBuf,
    },
    Daemon {
        #[command(subcommand)]
        command: DaemonCommand,
    },
}

#[derive(Debug, Subcommand)]
enum DaemonCommand {
    Start,
    Stop,
    Status,
}

fn main() -> std::process::ExitCode {
    let cli = Cli::parse();
    let client = DownloadClient::new(cli.endpoint);

    match cli.command {
        Command::Get {
            url,
            destination,
            detach,
            force,
            max_connections,
            min_segment_size,
        } => match client.download(
            &url,
            &destination,
            DownloadOptions {
                force,
                max_connections,
                min_segment_size,
            },
        ) {
            Ok(mut handle) => {
                if detach {
                    println!(
                        "attached download_id={} initiator={}",
                        handle.download_id(),
                        handle.initiator()
                    );
                    return std::process::ExitCode::SUCCESS;
                }
                loop {
                    match handle.wait(Some(250)).or_else(|_| handle.status()) {
                        Ok(status) => {
                            print_status(&status, cli.json);
                            if is_terminal(&status) {
                                return if matches!(
                                    status.phase,
                                    zccache::download::DownloadPhase::Completed
                                ) {
                                    std::process::ExitCode::SUCCESS
                                } else {
                                    std::process::ExitCode::FAILURE
                                };
                            }
                        }
                        Err(err) => {
                            eprintln!("{err}");
                            return std::process::ExitCode::FAILURE;
                        }
                    }
                }
            }
            Err(err) => {
                eprintln!("{err}");
                std::process::ExitCode::FAILURE
            }
        },
        Command::Fetch {
            url,
            part_urls,
            destination,
            expanded,
            expected_sha256,
            archive_format,
            no_wait,
            dry_run,
            force,
            max_connections,
            min_segment_size,
        } => {
            let source = match resolve_fetch_source(url, part_urls) {
                Ok(source) => source,
                Err(err) => {
                    eprintln!("{err}");
                    return std::process::ExitCode::FAILURE;
                }
            };
            let mut request = FetchRequest::new(source, destination);
            request.destination_path_expanded = expanded.map(Into::into);
            request.expected_sha256 = expected_sha256;
            request.archive_format = parse_archive_format(&archive_format);
            request.wait_mode = if no_wait {
                WaitMode::NoWait
            } else {
                WaitMode::Block
            };
            request.dry_run = dry_run;
            request.force = force;
            request.download_options = DownloadOptions {
                force,
                max_connections,
                min_segment_size,
            };
            match client.fetch(request) {
                Ok(result) => {
                    print_fetch_result(&result, cli.json);
                    std::process::ExitCode::SUCCESS
                }
                Err(err) => {
                    eprintln!("{err}");
                    std::process::ExitCode::FAILURE
                }
            }
        }
        Command::Exists {
            url,
            part_urls,
            destination,
            expanded,
            expected_sha256,
            archive_format,
        } => {
            let source = match resolve_fetch_source(url, part_urls) {
                Ok(source) => source,
                Err(err) => {
                    eprintln!("{err}");
                    return std::process::ExitCode::FAILURE;
                }
            };
            let mut request = FetchRequest::new(source, destination);
            request.destination_path_expanded = expanded.map(Into::into);
            request.expected_sha256 = expected_sha256;
            request.archive_format = parse_archive_format(&archive_format);
            match client.exists(&request) {
                Ok(state) => {
                    print_fetch_state(&state, cli.json);
                    std::process::ExitCode::SUCCESS
                }
                Err(err) => {
                    eprintln!("{err}");
                    std::process::ExitCode::FAILURE
                }
            }
        }
        Command::Wait {
            url,
            destination,
            timeout_ms,
        } => match client.download(&url, &destination, DownloadOptions::default()) {
            Ok(mut handle) => match handle.wait(timeout_ms) {
                Ok(status) => {
                    print_status(&status, cli.json);
                    std::process::ExitCode::SUCCESS
                }
                Err(err) => {
                    eprintln!("{err}");
                    std::process::ExitCode::FAILURE
                }
            },
            Err(err) => {
                eprintln!("{err}");
                std::process::ExitCode::FAILURE
            }
        },
        Command::Status { url, destination } => {
            match client.download(&url, &destination, DownloadOptions::default()) {
                Ok(mut handle) => match handle.status() {
                    Ok(status) => {
                        print_status(&status, cli.json);
                        std::process::ExitCode::SUCCESS
                    }
                    Err(err) => {
                        eprintln!("{err}");
                        std::process::ExitCode::FAILURE
                    }
                },
                Err(err) => {
                    eprintln!("{err}");
                    std::process::ExitCode::FAILURE
                }
            }
        }
        Command::Cancel { url, destination } => {
            match client.download(&url, &destination, DownloadOptions::default()) {
                Ok(mut handle) => match handle.cancel() {
                    Ok(status) => {
                        print_status(&status, cli.json);
                        std::process::ExitCode::SUCCESS
                    }
                    Err(err) => {
                        eprintln!("{err}");
                        std::process::ExitCode::FAILURE
                    }
                },
                Err(err) => {
                    eprintln!("{err}");
                    std::process::ExitCode::FAILURE
                }
            }
        }
        Command::Daemon { command } => match command {
            DaemonCommand::Start => match client.start_daemon() {
                Ok(()) => std::process::ExitCode::SUCCESS,
                Err(err) => {
                    eprintln!("{err}");
                    std::process::ExitCode::FAILURE
                }
            },
            DaemonCommand::Stop => match client.stop_daemon() {
                Ok(true) | Ok(false) => std::process::ExitCode::SUCCESS,
                Err(err) => {
                    eprintln!("{err}");
                    std::process::ExitCode::FAILURE
                }
            },
            DaemonCommand::Status => match client.daemon_status() {
                Ok(status) => {
                    if cli.json {
                        println!(
                            "{{\"version\":\"{}\",\"active_downloads\":{},\"connected_clients\":{},\"uptime_secs\":{},\"endpoint\":\"{}\"}}",
                            status.version,
                            status.active_downloads,
                            status.connected_clients,
                            status.uptime_secs,
                            status.endpoint,
                        );
                    } else {
                        println!("version={}", status.version);
                        println!("active_downloads={}", status.active_downloads);
                        println!("connected_clients={}", status.connected_clients);
                        println!("uptime_secs={}", status.uptime_secs);
                        println!("endpoint={}", status.endpoint);
                    }
                    std::process::ExitCode::SUCCESS
                }
                Err(err) => {
                    eprintln!("{err}");
                    std::process::ExitCode::FAILURE
                }
            },
        },
    }
}

fn print_status(status: &zccache::download::DownloadStatus, json: bool) {
    if json {
        println!(
            "{{\"phase\":\"{:?}\",\"total_bytes\":{},\"downloaded_bytes\":{},\"percentage\":{},\"active_clients\":{},\"destination\":\"{}\",\"source_url\":\"{}\",\"error\":{}}}",
            status.phase,
            status
                .total_bytes
                .map(|v| v.to_string())
                .unwrap_or_else(|| "null".to_string()),
            status.downloaded_bytes,
            status
                .percentage
                .map(|v| format!("{v:.2}"))
                .unwrap_or_else(|| "null".to_string()),
            status.active_clients,
            status.destination.display(),
            status.source_url,
            status
                .error
                .as_ref()
                .map(|e| format!("\"{}\"", e.replace('"', "\\\"")))
                .unwrap_or_else(|| "null".to_string()),
        );
    } else {
        println!(
            "phase={:?} downloaded={} total={} percent={} clients={} destination={}",
            status.phase,
            status.downloaded_bytes,
            status
                .total_bytes
                .map(|v| v.to_string())
                .unwrap_or_else(|| "?".to_string()),
            status
                .percentage
                .map(|v| format!("{v:.2}"))
                .unwrap_or_else(|| "?".to_string()),
            status.active_clients,
            status.destination.display(),
        );
        if let Some(err) = &status.error {
            println!("error={err}");
        }
    }
}

fn print_fetch_result(result: &FetchResult, json: bool) {
    if json {
        println!(
            "{{\"status\":\"{:?}\",\"cache_path\":\"{}\",\"expanded_path\":{},\"bytes\":{},\"sha256\":\"{}\"}}",
            result.status,
            result.cache_path.display(),
            result
                .expanded_path
                .as_ref()
                .map(|p| format!("\"{}\"", p.display()))
                .unwrap_or_else(|| "null".to_string()),
            result
                .bytes
                .map(|v| v.to_string())
                .unwrap_or_else(|| "null".to_string()),
            result.sha256,
        );
    } else {
        println!("status={:?}", result.status);
        println!("cache_path={}", result.cache_path.display());
        println!("sha256={}", result.sha256);
        if let Some(expanded_path) = &result.expanded_path {
            println!("expanded_path={}", expanded_path.display());
        }
        if let Some(bytes) = result.bytes {
            println!("bytes={bytes}");
        }
    }
}

fn print_fetch_state(state: &FetchState, json: bool) {
    if json {
        println!(
            "{{\"kind\":\"{:?}\",\"cache_path\":\"{}\",\"expanded_path\":{},\"bytes\":{},\"sha256\":{},\"reason\":{}}}",
            state.kind,
            state.cache_path.display(),
            state
                .expanded_path
                .as_ref()
                .map(|p| format!("\"{}\"", p.display()))
                .unwrap_or_else(|| "null".to_string()),
            state
                .bytes
                .map(|v| v.to_string())
                .unwrap_or_else(|| "null".to_string()),
            state
                .sha256
                .as_ref()
                .map(|v| format!("\"{}\"", v))
                .unwrap_or_else(|| "null".to_string()),
            state
                .reason
                .as_ref()
                .map(|e| format!("\"{}\"", e.replace('"', "\\\"")))
                .unwrap_or_else(|| "null".to_string()),
        );
    } else {
        println!("kind={:?}", state.kind);
        println!("cache_path={}", state.cache_path.display());
        if let Some(expanded_path) = &state.expanded_path {
            println!("expanded_path={}", expanded_path.display());
        }
        if let Some(bytes) = state.bytes {
            println!("bytes={bytes}");
        }
        if let Some(sha256) = &state.sha256 {
            println!("sha256={sha256}");
        }
        if let Some(reason) = &state.reason {
            println!("reason={reason}");
        }
    }
}

fn parse_archive_format(value: &str) -> ArchiveFormat {
    match value.to_ascii_lowercase().as_str() {
        "none" => ArchiveFormat::None,
        "zst" => ArchiveFormat::Zst,
        "zip" => ArchiveFormat::Zip,
        "xz" => ArchiveFormat::Xz,
        "tar.gz" | "targz" => ArchiveFormat::TarGz,
        "tar.xz" | "tarxz" => ArchiveFormat::TarXz,
        "tar.zst" | "tarzst" => ArchiveFormat::TarZst,
        "7z" | "sevenz" => ArchiveFormat::SevenZip,
        _ => ArchiveFormat::Auto,
    }
}

fn resolve_fetch_source(
    url: Option<String>,
    part_urls: Vec<String>,
) -> Result<DownloadSource, String> {
    match (url, part_urls.is_empty()) {
        (Some(url), true) => Ok(DownloadSource::Url(url)),
        (None, false) => Ok(DownloadSource::MultipartUrls(part_urls)),
        (Some(_), false) => Err("use either --url or --part-url, not both".to_string()),
        (None, true) => Err("provide either --url or at least one --part-url".to_string()),
    }
}
