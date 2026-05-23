#![allow(clippy::missing_errors_doc)]

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use reqwest::header::{
    ACCEPT_ENCODING, ACCEPT_RANGES, CONTENT_LENGTH, ETAG, IF_RANGE, LAST_MODIFIED, RANGE,
};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_util::sync::CancellationToken;
use crate::core::NormalizedPath;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DownloadOptions {
    pub force: bool,
    pub max_connections: Option<usize>,
    pub min_segment_size: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DownloadPhase {
    Pending,
    Downloading,
    Finalizing,
    Completed,
    Cancelled,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DownloadStatus {
    pub phase: DownloadPhase,
    pub total_bytes: Option<u64>,
    pub downloaded_bytes: u64,
    pub percentage: Option<f32>,
    pub active_clients: u32,
    pub destination: NormalizedPath,
    pub source_url: String,
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DownloadDaemonStatus {
    pub version: String,
    pub active_downloads: u64,
    pub connected_clients: u64,
    pub uptime_secs: u64,
    pub endpoint: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DownloadAttachResult {
    pub download_id: String,
    pub initiator: bool,
    pub status: DownloadStatus,
}

#[derive(Debug, Clone)]
struct Probe {
    total_bytes: Option<u64>,
    accept_ranges: bool,
    validator: Option<String>,
}

#[derive(Debug, Clone)]
pub struct EngineConfig {
    pub max_connections: usize,
    pub min_segment_size: u64,
}

impl Default for EngineConfig {
    fn default() -> Self {
        let cpus = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4);
        Self {
            max_connections: (cpus * 2).min(16),
            min_segment_size: 8 * 1024 * 1024,
        }
    }
}

#[derive(Debug, Error)]
pub enum DownloadError {
    #[error("http error: {0}")]
    Http(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("download cancelled")]
    Cancelled,
    #[error("remote refused ranged download")]
    RangeUnsupported,
}

pub type ProgressCallback = Arc<dyn Fn(u64, Option<u64>, DownloadPhase) + Send + Sync>;

struct SegmentedDownload<'a> {
    client: &'a reqwest::Client,
    url: &'a str,
    temp_path: &'a Path,
    metadata_dir: &'a Path,
    total: u64,
    max_connections: usize,
    min_segment_size: u64,
    validator: Option<String>,
    progress: ProgressCallback,
    cancel_token: CancellationToken,
}

struct SegmentDownload<'a> {
    client: &'a reqwest::Client,
    url: &'a str,
    segment_path: &'a Path,
    start: u64,
    end: u64,
    validator: Option<String>,
    progress: Arc<dyn Fn(u64) + Send + Sync>,
    cancel_token: CancellationToken,
}

pub async fn download_to_path(
    url: &str,
    destination: &Path,
    metadata_dir: &Path,
    options: &DownloadOptions,
    progress: ProgressCallback,
    cancel_token: CancellationToken,
) -> Result<Option<u64>, DownloadError> {
    let client = reqwest::Client::builder()
        .user_agent(format!("zccache-download/{}", crate::core::VERSION))
        .build()
        .map_err(|e| DownloadError::Http(e.to_string()))?;

    let probe = probe_remote(&client, url).await?;
    let total = probe.total_bytes;
    let engine = EngineConfig::default();
    let max_connections = options
        .max_connections
        .unwrap_or(engine.max_connections)
        .max(1);
    let min_segment_size = options
        .min_segment_size
        .unwrap_or(engine.min_segment_size)
        .max(1);

    if let Some(parent) = destination.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    tokio::fs::create_dir_all(metadata_dir).await?;

    let temp_path = destination.with_extension(format!(
        "{}part",
        destination
            .extension()
            .map(|ext| format!("{}.", ext.to_string_lossy()))
            .unwrap_or_default()
    ));

    if options.force {
        let _ = tokio::fs::remove_file(&temp_path).await;
    }

    progress(0, total, DownloadPhase::Downloading);

    let result = if should_use_segments(
        total,
        probe.accept_ranges,
        max_connections,
        min_segment_size,
    ) {
        download_segmented(SegmentedDownload {
            client: &client,
            url,
            temp_path: &temp_path,
            metadata_dir,
            total: total.expect("segment use requires total"),
            max_connections,
            min_segment_size,
            validator: probe.validator,
            progress: progress.clone(),
            cancel_token: cancel_token.clone(),
        })
        .await
    } else {
        download_single(
            &client,
            url,
            &temp_path,
            total,
            progress.clone(),
            cancel_token.clone(),
        )
        .await
    };

    match result {
        Ok(bytes) => {
            progress(bytes, total.or(Some(bytes)), DownloadPhase::Finalizing);
            if destination.exists() {
                let _ = tokio::fs::remove_file(destination).await;
            }
            tokio::fs::rename(&temp_path, destination).await?;
            progress(bytes, total.or(Some(bytes)), DownloadPhase::Completed);
            Ok(total.or(Some(bytes)))
        }
        Err(DownloadError::Cancelled) => {
            let _ = tokio::fs::remove_file(&temp_path).await;
            cleanup_segment_dir(metadata_dir).await;
            progress(0, total, DownloadPhase::Cancelled);
            Err(DownloadError::Cancelled)
        }
        Err(err) => {
            let _ = tokio::fs::remove_file(&temp_path).await;
            cleanup_segment_dir(metadata_dir).await;
            Err(err)
        }
    }
}

async fn cleanup_segment_dir(metadata_dir: &Path) {
    let _ = tokio::fs::remove_dir_all(metadata_dir).await;
}

fn should_use_segments(
    total_bytes: Option<u64>,
    accept_ranges: bool,
    max_connections: usize,
    min_segment_size: u64,
) -> bool {
    match total_bytes {
        Some(total) => accept_ranges && max_connections > 1 && total >= min_segment_size * 2,
        None => false,
    }
}

async fn probe_remote(client: &reqwest::Client, url: &str) -> Result<Probe, DownloadError> {
    let head = client
        .head(url)
        .header(ACCEPT_ENCODING, "identity")
        .send()
        .await
        .map_err(|e| DownloadError::Http(e.to_string()))?;
    if head.status().is_success() {
        let total_bytes = head
            .headers()
            .get(CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<u64>().ok());
        let accept_ranges = head
            .headers()
            .get(ACCEPT_RANGES)
            .and_then(|v| v.to_str().ok())
            .map(|v| v.eq_ignore_ascii_case("bytes"))
            .unwrap_or(false);
        let validator = head
            .headers()
            .get(ETAG)
            .or_else(|| head.headers().get(LAST_MODIFIED))
            .and_then(|v| v.to_str().ok())
            .map(ToOwned::to_owned);
        return Ok(Probe {
            total_bytes,
            accept_ranges,
            validator,
        });
    }

    let get = client
        .get(url)
        .header(ACCEPT_ENCODING, "identity")
        .header(RANGE, "bytes=0-0")
        .send()
        .await
        .map_err(|e| DownloadError::Http(e.to_string()))?;
    let total_bytes = get.content_length().map(|len| len.max(1));
    let accept_ranges = get.status() == reqwest::StatusCode::PARTIAL_CONTENT;
    let validator = get
        .headers()
        .get(ETAG)
        .or_else(|| get.headers().get(LAST_MODIFIED))
        .and_then(|v| v.to_str().ok())
        .map(ToOwned::to_owned);
    Ok(Probe {
        total_bytes,
        accept_ranges,
        validator,
    })
}

async fn download_single(
    client: &reqwest::Client,
    url: &str,
    temp_path: &Path,
    total: Option<u64>,
    progress: ProgressCallback,
    cancel_token: CancellationToken,
) -> Result<u64, DownloadError> {
    let response = client
        .get(url)
        .header(ACCEPT_ENCODING, "identity")
        .send()
        .await
        .map_err(|e| DownloadError::Http(e.to_string()))?;
    if !response.status().is_success() {
        return Err(DownloadError::Http(format!(
            "unexpected status {} for {url}",
            response.status()
        )));
    }

    let mut file = tokio::fs::File::create(temp_path).await?;
    let mut stream = response.bytes_stream();
    let mut downloaded = 0u64;
    while let Some(item) = stream.next().await {
        if cancel_token.is_cancelled() {
            return Err(DownloadError::Cancelled);
        }
        let chunk = item.map_err(|e| DownloadError::Http(e.to_string()))?;
        file.write_all(&chunk).await?;
        downloaded += chunk.len() as u64;
        progress(downloaded, total, DownloadPhase::Downloading);
    }
    file.flush().await?;
    Ok(downloaded)
}

async fn download_segmented(request: SegmentedDownload<'_>) -> Result<u64, DownloadError> {
    let SegmentedDownload {
        client,
        url,
        temp_path,
        metadata_dir,
        total,
        max_connections,
        min_segment_size,
        validator,
        progress,
        cancel_token,
    } = request;

    let segments = build_segments(total, max_connections, min_segment_size);
    let progress_total = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let mut tasks = Vec::with_capacity(segments.len());

    for (index, (start, end)) in segments.iter().copied().enumerate() {
        let client = client.clone();
        let url = url.to_string();
        let segment_path = metadata_dir.join(format!("segment-{index:03}.part"));
        let validator = validator.clone();
        let progress = Arc::clone(&progress);
        let progress_total = Arc::clone(&progress_total);
        let cancel_token = cancel_token.clone();
        tasks.push(tokio::spawn(async move {
            let downloaded = download_segment(SegmentDownload {
                client: &client,
                url: &url,
                segment_path: &segment_path,
                start,
                end,
                validator,
                progress: Arc::new(move |delta| {
                    let total_downloaded = progress_total
                        .fetch_add(delta, std::sync::atomic::Ordering::Relaxed)
                        + delta;
                    progress(total_downloaded, Some(total), DownloadPhase::Downloading);
                }),
                cancel_token,
            })
            .await?;
            Ok::<_, DownloadError>((index, segment_path, downloaded))
        }));
    }

    let mut segment_paths = Vec::with_capacity(tasks.len());
    for task in tasks {
        let (index, path, _bytes) = task
            .await
            .map_err(|e| DownloadError::Http(e.to_string()))??;
        segment_paths.push((index, path));
    }
    segment_paths.sort_by_key(|(index, _)| *index);

    let mut out = tokio::fs::File::create(temp_path).await?;
    for (_, path) in segment_paths {
        if cancel_token.is_cancelled() {
            return Err(DownloadError::Cancelled);
        }
        let mut segment = tokio::fs::File::open(&path).await?;
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            let n = segment.read(&mut buf).await?;
            if n == 0 {
                break;
            }
            out.write_all(&buf[..n]).await?;
        }
        let _ = tokio::fs::remove_file(&path).await;
    }
    out.flush().await?;
    cleanup_segment_dir(metadata_dir).await;
    Ok(total)
}

fn build_segments(total: u64, max_connections: usize, min_segment_size: u64) -> Vec<(u64, u64)> {
    let max_segments = total.div_ceil(min_segment_size) as usize;
    let segments = max_segments.max(1).min(max_connections.max(1));
    let base = total / segments as u64;
    let remainder = total % segments as u64;

    let mut out = Vec::with_capacity(segments);
    let mut start = 0u64;
    for i in 0..segments {
        let extra = if (i as u64) < remainder { 1 } else { 0 };
        let len = base + extra;
        let end = start + len - 1;
        out.push((start, end));
        start = end + 1;
    }
    out
}

async fn download_segment(request: SegmentDownload<'_>) -> Result<u64, DownloadError> {
    let SegmentDownload {
        client,
        url,
        segment_path,
        start,
        end,
        validator,
        progress,
        cancel_token,
    } = request;

    let mut request = client
        .get(url)
        .header(ACCEPT_ENCODING, "identity")
        .header(RANGE, format!("bytes={start}-{end}"));
    if let Some(value) = validator.as_deref() {
        request = request.header(IF_RANGE, value);
    }

    let response = request
        .send()
        .await
        .map_err(|e| DownloadError::Http(e.to_string()))?;
    if response.status() != reqwest::StatusCode::PARTIAL_CONTENT {
        return Err(DownloadError::RangeUnsupported);
    }

    let mut file = tokio::fs::File::create(segment_path).await?;
    let mut stream = response.bytes_stream();
    let mut downloaded = 0u64;
    while let Some(item) = stream.next().await {
        if cancel_token.is_cancelled() {
            return Err(DownloadError::Cancelled);
        }
        let chunk = item.map_err(|e| DownloadError::Http(e.to_string()))?;
        file.write_all(&chunk).await?;
        downloaded += chunk.len() as u64;
        progress(chunk.len() as u64);
    }
    file.flush().await?;
    Ok(downloaded)
}

pub fn stable_download_id(path: &Path) -> String {
    let key = crate::core::normalize_for_key(path);
    blake3::hash(key.as_bytes()).to_hex().to_string()
}

pub fn canonical_destination(path: &Path) -> Result<NormalizedPath, std::io::Error> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    let filename = absolute.file_name().map(ToOwned::to_owned).ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "destination must include a file name",
        )
    })?;
    let parent = absolute.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)?;
    let canonical_parent = std::fs::canonicalize(parent)?;
    Ok(NormalizedPath::new(canonical_parent.join(filename)))
}

pub fn percentage(downloaded: u64, total: Option<u64>) -> Option<f32> {
    total.and_then(|t| {
        if t == 0 {
            None
        } else {
            Some(((downloaded as f64 * 100.0 / t as f64) * 100.0).round() as f32 / 100.0)
        }
    })
}

pub fn uptime_secs(start: std::time::Instant) -> u64 {
    Duration::from_secs(start.elapsed().as_secs()).as_secs()
}

use thiserror::Error;

#[cfg(test)]
mod tests {
    use super::*;

    use std::net::SocketAddr;
    use std::path::PathBuf;
    use std::sync::mpsc;

    use tokio::net::TcpListener;
    use tokio::sync::oneshot;

    #[derive(Clone)]
    struct TestServerConfig {
        body: Arc<Vec<u8>>,
        accept_ranges: bool,
        send_content_length: bool,
        chunk_size: usize,
        chunk_delay_ms: u64,
        etag: Option<String>,
    }

    struct TestServer {
        url: String,
        shutdown: Option<oneshot::Sender<()>>,
        task: tokio::task::JoinHandle<()>,
    }

    impl TestServer {
        async fn start(config: TestServerConfig) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let (shutdown_tx, mut shutdown_rx) = oneshot::channel();
            let task = tokio::spawn(async move {
                loop {
                    tokio::select! {
                        _ = &mut shutdown_rx => break,
                        accepted = listener.accept() => {
                            let Ok((stream, _)) = accepted else { break; };
                            let config = config.clone();
                            tokio::spawn(async move {
                                let _ = handle_test_connection(stream, config).await;
                            });
                        }
                    }
                }
            });
            Self {
                url: format!("http://{addr}/payload.bin"),
                shutdown: Some(shutdown_tx),
                task,
            }
        }
    }

    impl Drop for TestServer {
        fn drop(&mut self) {
            if let Some(tx) = self.shutdown.take() {
                let _ = tx.send(());
            }
            self.task.abort();
        }
    }

    async fn handle_test_connection(
        mut stream: tokio::net::TcpStream,
        config: TestServerConfig,
    ) -> Result<(), std::io::Error> {
        let mut request = Vec::new();
        let mut buf = [0u8; 4096];
        loop {
            let n = stream.read(&mut buf).await?;
            if n == 0 {
                return Ok(());
            }
            request.extend_from_slice(&buf[..n]);
            if request.windows(4).any(|window| window == b"\r\n\r\n") {
                break;
            }
        }

        let request_text = String::from_utf8_lossy(&request);
        let mut lines = request_text.lines();
        let request_line = lines.next().unwrap_or_default();
        let mut parts = request_line.split_whitespace();
        let method = parts.next().unwrap_or_default();

        let range_header = request_text.lines().find_map(|line| {
            let (name, value) = line.split_once(':')?;
            if name.eq_ignore_ascii_case("range") {
                Some(value.trim().to_string())
            } else {
                None
            }
        });

        let mut status_line = "HTTP/1.1 200 OK\r\n".to_string();
        let mut body = (*config.body).clone();

        if let Some(range) = range_header {
            if config.accept_ranges {
                if let Some((start, end)) = parse_range(&range, body.len() as u64) {
                    status_line = "HTTP/1.1 206 Partial Content\r\n".to_string();
                    body = body[start as usize..=end as usize].to_vec();
                }
            }
        }

        let mut headers = String::new();
        headers.push_str("Connection: close\r\n");
        headers.push_str("Content-Type: application/octet-stream\r\n");
        if config.accept_ranges {
            headers.push_str("Accept-Ranges: bytes\r\n");
        }
        if config.send_content_length {
            headers.push_str(&format!("Content-Length: {}\r\n", body.len()));
        }
        if let Some(etag) = &config.etag {
            headers.push_str(&format!("ETag: {etag}\r\n"));
        }

        stream.write_all(status_line.as_bytes()).await?;
        stream.write_all(headers.as_bytes()).await?;
        stream.write_all(b"\r\n").await?;

        if method.eq_ignore_ascii_case("HEAD") {
            stream.flush().await?;
            return Ok(());
        }

        if config.chunk_size == 0 {
            stream.write_all(&body).await?;
        } else {
            for chunk in body.chunks(config.chunk_size) {
                stream.write_all(chunk).await?;
                stream.flush().await?;
                if config.chunk_delay_ms > 0 {
                    tokio::time::sleep(Duration::from_millis(config.chunk_delay_ms)).await;
                }
            }
        }
        stream.flush().await?;
        Ok(())
    }

    fn parse_range(header: &str, total_len: u64) -> Option<(u64, u64)> {
        let range = header.strip_prefix("bytes=")?;
        let (start, end) = range.split_once('-')?;
        let start = start.parse::<u64>().ok()?;
        let end = if end.is_empty() {
            total_len.checked_sub(1)?
        } else {
            end.parse::<u64>().ok()?
        };
        if start > end || end >= total_len {
            return None;
        }
        Some((start, end))
    }

    fn progress_recorder() -> (
        ProgressCallback,
        mpsc::Receiver<(u64, Option<u64>, DownloadPhase)>,
    ) {
        let (tx, rx) = mpsc::channel();
        let callback = Arc::new(move |downloaded, total, phase| {
            let _ = tx.send((downloaded, total, phase));
        });
        (callback, rx)
    }

    fn expected_temp_path(destination: &Path) -> PathBuf {
        destination.with_extension(format!(
            "{}part",
            destination
                .extension()
                .map(|ext| format!("{}.", ext.to_string_lossy()))
                .unwrap_or_default()
        ))
    }

    #[tokio::test]
    async fn download_single_stream_from_dummy_server() {
        let server = TestServer::start(TestServerConfig {
            body: Arc::new(b"hello download world".to_vec()),
            accept_ranges: false,
            send_content_length: true,
            chunk_size: 0,
            chunk_delay_ms: 0,
            etag: Some("\"single\"".to_string()),
        })
        .await;

        let dir = tempfile::tempdir().unwrap();
        let destination = dir.path().join("artifact.bin");
        let metadata_dir = dir.path().join("metadata");
        let (progress, rx) = progress_recorder();

        let result = download_to_path(
            &server.url,
            &destination,
            &metadata_dir,
            &DownloadOptions {
                force: false,
                max_connections: Some(1),
                min_segment_size: Some(1024),
            },
            progress,
            CancellationToken::new(),
        )
        .await
        .unwrap();

        assert_eq!(result, Some(20));
        assert_eq!(
            tokio::fs::read(&destination).await.unwrap(),
            b"hello download world"
        );

        let events: Vec<_> = rx.try_iter().collect();
        assert!(events
            .iter()
            .any(|(_, _, phase)| *phase == DownloadPhase::Completed));
    }

    #[tokio::test]
    async fn download_segmented_from_dummy_server_on_random_port() {
        let body: Vec<u8> = (0..64 * 1024).map(|i| (i % 251) as u8).collect();
        let server = TestServer::start(TestServerConfig {
            body: Arc::new(body.clone()),
            accept_ranges: true,
            send_content_length: true,
            chunk_size: 4096,
            chunk_delay_ms: 0,
            etag: Some("\"segmented\"".to_string()),
        })
        .await;

        let parsed: SocketAddr = server
            .url
            .trim_start_matches("http://")
            .split('/')
            .next()
            .unwrap()
            .parse()
            .unwrap();
        assert!(
            parsed.port() > 0,
            "server must resolve a random port from bind(0)"
        );

        let dir = tempfile::tempdir().unwrap();
        let destination = dir.path().join("segmented.bin");
        let metadata_dir = dir.path().join("segments");

        let result = download_to_path(
            &server.url,
            &destination,
            &metadata_dir,
            &DownloadOptions {
                force: false,
                max_connections: Some(4),
                min_segment_size: Some(1024),
            },
            Arc::new(|_, _, _| {}),
            CancellationToken::new(),
        )
        .await
        .unwrap();

        assert_eq!(result, Some(body.len() as u64));
        assert_eq!(tokio::fs::read(&destination).await.unwrap(), body);
        assert!(
            tokio::fs::metadata(&metadata_dir).await.is_err(),
            "segment metadata dir should be removed after success"
        );
    }

    #[tokio::test]
    async fn download_without_content_length_still_completes() {
        let body = b"unknown length body".to_vec();
        let server = TestServer::start(TestServerConfig {
            body: Arc::new(body.clone()),
            accept_ranges: false,
            send_content_length: false,
            chunk_size: 5,
            chunk_delay_ms: 0,
            etag: None,
        })
        .await;

        let dir = tempfile::tempdir().unwrap();
        let destination = dir.path().join("unknown.bin");
        let metadata_dir = dir.path().join("meta");
        let (progress, rx) = progress_recorder();

        let result = download_to_path(
            &server.url,
            &destination,
            &metadata_dir,
            &DownloadOptions::default(),
            progress,
            CancellationToken::new(),
        )
        .await
        .unwrap();

        assert_eq!(result, Some(body.len() as u64));
        assert_eq!(tokio::fs::read(&destination).await.unwrap(), body);
        let events: Vec<_> = rx.try_iter().collect();
        assert!(events.iter().any(|(_, total, _)| total.is_none()));
    }

    #[tokio::test]
    async fn cancelling_download_removes_temp_file_and_metadata() {
        let body: Vec<u8> = (0..128 * 1024).map(|i| (i % 251) as u8).collect();
        let server = TestServer::start(TestServerConfig {
            body: Arc::new(body),
            accept_ranges: false,
            send_content_length: true,
            chunk_size: 1024,
            chunk_delay_ms: 10,
            etag: Some("\"cancel\"".to_string()),
        })
        .await;

        let dir = tempfile::tempdir().unwrap();
        let destination = dir.path().join("cancel.bin");
        let metadata_dir = dir.path().join("cancel-meta");
        let temp_path = expected_temp_path(&destination);
        let cancel = CancellationToken::new();
        let cancel_clone = cancel.clone();
        let (progress, rx) = progress_recorder();

        let task = tokio::spawn(async move {
            download_to_path(
                &server.url,
                &destination,
                &metadata_dir,
                &DownloadOptions {
                    force: false,
                    max_connections: Some(1),
                    min_segment_size: Some(4096),
                },
                progress,
                cancel_clone,
            )
            .await
        });

        let first_progress = tokio::task::spawn_blocking(move || {
            rx.recv_timeout(std::time::Duration::from_secs(5))
                .expect("expected progress event")
        })
        .await
        .unwrap();
        assert_eq!(first_progress.2, DownloadPhase::Downloading);

        cancel.cancel();
        let result = task.await.unwrap();
        assert!(matches!(result, Err(DownloadError::Cancelled)));
        assert!(
            tokio::fs::metadata(&temp_path).await.is_err(),
            "temp file should be removed after cancellation"
        );
        assert!(
            tokio::fs::metadata(dir.path().join("cancel.bin"))
                .await
                .is_err(),
            "final destination should not exist after cancellation"
        );
        assert!(
            tokio::fs::metadata(dir.path().join("cancel-meta"))
                .await
                .is_err(),
            "metadata directory should be removed after cancellation"
        );
    }

    #[test]
    fn percentage_rounds_to_two_decimals() {
        assert_eq!(percentage(1, Some(3)), Some(33.33));
        assert_eq!(percentage(2, Some(3)), Some(66.67));
        assert_eq!(percentage(0, None), None);
    }
}
