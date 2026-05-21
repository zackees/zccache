// A deliberately ordinary Rust binary used as the medium-sized
// performance fixture for the soldr perf cluster.
//
// Every dep declared in Cargo.toml is exercised by name from main so
// dead-code elimination cannot prune the compile units we want to
// measure caching against. Do NOT make this file artistic.

use std::time::Duration;

use anyhow::{Context, Result};
use chrono::Utc;
use clap::Parser;
use serde::{Deserialize, Serialize};
use tracing::{info, instrument};
use uuid::Uuid;

#[derive(Parser, Debug)]
#[command(name = "medium-rust-app", version)]
struct Cli {
    /// URL to ping; defaults to a host that will not resolve so the
    /// fixture never makes real network requests.
    #[arg(long, default_value = "https://example.invalid")]
    url: String,
    /// Network timeout in seconds.
    #[arg(long, default_value_t = 5)]
    timeout_secs: u64,
}

#[derive(Serialize, Deserialize, Debug, thiserror::Error)]
#[error("medium-rust-app error: {message}")]
struct AppError {
    id: Uuid,
    when: chrono::DateTime<chrono::Utc>,
    message: String,
}

#[derive(Serialize)]
struct Response {
    request_id: Uuid,
    at: chrono::DateTime<chrono::Utc>,
    preview: String,
}

#[instrument]
async fn fetch(url: &str, timeout: Duration) -> Result<String> {
    let client = reqwest::Client::builder()
        .timeout(timeout)
        .build()
        .context("build reqwest client")?;
    let body = client
        .get(url)
        .send()
        .await
        .context("send request")?
        .text()
        .await
        .context("read body")?;
    Ok(body)
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    let cli = Cli::parse();
    let request_id = Uuid::new_v4();
    info!(?cli, %request_id, "starting");

    let timeout = Duration::from_secs(cli.timeout_secs);
    match fetch(&cli.url, timeout).await {
        Ok(body) => {
            let preview: String = body.chars().take(128).collect();
            let resp = Response {
                request_id,
                at: Utc::now(),
                preview,
            };
            println!("{}", serde_json::to_string(&resp)?);
        }
        Err(err) => {
            let app_err = AppError {
                id: request_id,
                when: Utc::now(),
                message: format!("{err:#}"),
            };
            eprintln!("{}", serde_json::to_string(&app_err)?);
        }
    }
    Ok(())
}
