//! Shared test helpers + lightweight unit tests for the artifact pipeline.
//!
//! End-to-end fetch tests live in `fetch.rs`.

#![cfg(test)]

mod fetch;

use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};

use crate::download_client::DownloadClient;
use crate::download_daemon::DownloadDaemon;
use flate2::write::GzEncoder;
use flate2::Compression;

use super::archive::{auto_archive_format, extract_tar, extract_zip, safe_join};
use super::{ArchiveFormat, FetchRequest, FetchResult};

#[derive(Clone)]
pub(super) struct TestHttpConfig {
    pub(super) body: Arc<Vec<u8>>,
    pub(super) accept_ranges: bool,
    pub(super) send_content_length: bool,
    pub(super) chunk_size: usize,
    pub(super) chunk_delay: Duration,
    pub(super) path: String,
    pub(super) request_started: Option<Arc<AtomicBool>>,
    pub(super) release_response: Option<Arc<AtomicBool>>,
}

pub(super) struct TestHttpServer {
    pub(super) url: String,
    request_count: Arc<AtomicUsize>,
    range_request_count: Arc<AtomicUsize>,
    shutdown: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl TestHttpServer {
    pub(super) fn start(config: TestHttpConfig) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{addr}/{}", config.path);
        let request_count = Arc::new(AtomicUsize::new(0));
        let range_request_count = Arc::new(AtomicUsize::new(0));
        let shutdown = Arc::new(AtomicBool::new(false));
        let request_count_clone = Arc::clone(&request_count);
        let range_request_count_clone = Arc::clone(&range_request_count);
        let shutdown_clone = Arc::clone(&shutdown);
        let config_for_thread = config.clone();
        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
        let thread = thread::spawn(move || {
            ready_tx.send(()).unwrap();
            while !shutdown_clone.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let config = config_for_thread.clone();
                        let request_count = Arc::clone(&request_count_clone);
                        let range_request_count = Arc::clone(&range_request_count_clone);
                        thread::spawn(move || {
                            let _ = handle_test_http_connection(
                                stream,
                                config,
                                request_count,
                                range_request_count,
                            );
                        });
                    }
                    Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => {
                        // Windows can surface a wide range of transient listener errors
                        // (Interrupted, ConnectionAborted/Reset, and WSA-specific errnos
                        // that map to Uncategorized). Never let one kill the accept loop:
                        // only `shutdown` exits, so a later request still finds a server.
                        thread::sleep(Duration::from_millis(10));
                    }
                }
            }
        });
        ready_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("test http server failed to start");
        wait_for_test_http_server(&addr, &config.path);
        request_count.store(0, Ordering::Relaxed);
        range_request_count.store(0, Ordering::Relaxed);
        Self {
            url,
            request_count,
            range_request_count,
            shutdown,
            thread: Some(thread),
        }
    }

    pub(super) fn request_count(&self) -> usize {
        self.request_count.load(Ordering::Relaxed)
    }

    pub(super) fn range_request_count(&self) -> usize {
        self.range_request_count.load(Ordering::Relaxed)
    }
}

fn wait_for_test_http_server(addr: &std::net::SocketAddr, path: &str) {
    let deadline = Instant::now() + Duration::from_secs(1);
    let request = format!("HEAD /{path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n");
    while Instant::now() < deadline {
        if let Ok(mut stream) = TcpStream::connect(addr) {
            if stream
                .set_read_timeout(Some(Duration::from_millis(100)))
                .is_err()
            {
                thread::sleep(Duration::from_millis(10));
                continue;
            }
            if stream
                .set_write_timeout(Some(Duration::from_millis(100)))
                .is_err()
            {
                thread::sleep(Duration::from_millis(10));
                continue;
            }
            if stream.write_all(request.as_bytes()).is_err() {
                thread::sleep(Duration::from_millis(10));
                continue;
            }
            let mut response = Vec::new();
            let mut buf = [0u8; 256];
            loop {
                match stream.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        response.extend_from_slice(&buf[..n]);
                        if response.windows(4).any(|window| window == b"\r\n\r\n") {
                            return;
                        }
                    }
                    Err(err)
                        if err.kind() == io::ErrorKind::WouldBlock
                            || err.kind() == io::ErrorKind::TimedOut =>
                    {
                        break;
                    }
                    Err(_) => break,
                }
            }
        }
        thread::sleep(Duration::from_millis(10));
    }
    panic!("test http server at {addr} did not respond in time");
}

pub(super) fn try_wait_for_test_condition(
    timeout: Duration,
    description: &str,
    mut predicate: impl FnMut() -> bool,
) -> Result<(), String> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if predicate() {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(10));
    }
    Err(format!("timed out waiting for {description}"))
}

pub(super) fn run_with_self_healing<F>(label: &str, mut attempt_fn: F)
where
    F: FnMut(usize) -> Result<(), String>,
{
    const MAX_ATTEMPTS: usize = 3;
    let mut last_err: Option<String> = None;
    for attempt in 1..=MAX_ATTEMPTS {
        match attempt_fn(attempt) {
            Ok(()) => return,
            Err(err) => {
                eprintln!("{label}: attempt {attempt}/{MAX_ATTEMPTS} failed: {err}");
                last_err = Some(err);
            }
        }
    }
    panic!(
        "{label} failed after {MAX_ATTEMPTS} attempts: {}",
        last_err.unwrap_or_else(|| "unknown error".to_string())
    );
}

// Localhost reqwest calls on Windows can transiently fail with "error sending
// request" (backlog pressure) or "error decoding response body" (mid-stream
// parse error). The segmented path surfaces these with a "http error:" prefix
// via DownloadError::Http; the explicit-multipart path (download_explicit_parts)
// stringifies the reqwest::Error directly, producing the raw message. Match
// both substrings so either path retries. Real callers don't retry these;
// tests with local HTTP servers must.
fn is_transient_http_error(err: &str) -> bool {
    err.contains("error sending request") || err.contains("error decoding response body")
}

pub(super) fn fetch_with_retry(
    http: &DownloadClient,
    req: FetchRequest,
) -> Result<FetchResult, String> {
    const MAX_ATTEMPTS: usize = 5;
    for attempt in 1..=MAX_ATTEMPTS {
        match http.fetch(req.clone()) {
            Ok(result) => return Ok(result),
            Err(err) => {
                if !is_transient_http_error(&err) || attempt == MAX_ATTEMPTS {
                    return Err(err);
                }
                thread::sleep(Duration::from_millis(25 * attempt as u64));
            }
        }
    }
    unreachable!()
}

impl Drop for TestHttpServer {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        let _ = TcpStream::connect(
            self.url
                .trim_start_matches("http://")
                .split('/')
                .next()
                .unwrap_or_default(),
        );
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn handle_test_http_connection(
    mut stream: TcpStream,
    config: TestHttpConfig,
    request_count: Arc<AtomicUsize>,
    range_request_count: Arc<AtomicUsize>,
) -> io::Result<()> {
    let mut request = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        let n = stream.read(&mut buf)?;
        if n == 0 {
            return Ok(());
        }
        request.extend_from_slice(&buf[..n]);
        if request.windows(4).any(|window| window == b"\r\n\r\n") {
            break;
        }
    }
    request_count.fetch_add(1, Ordering::Relaxed);
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

    let mut body = (*config.body).clone();
    let mut status_line = "HTTP/1.1 200 OK\r\n".to_string();
    let mut content_range = None;
    if let Some(range) = range_header {
        if config.accept_ranges {
            if let Some((start, end)) = parse_range(&range, body.len() as u64) {
                range_request_count.fetch_add(1, Ordering::Relaxed);
                status_line = "HTTP/1.1 206 Partial Content\r\n".to_string();
                content_range = Some(format!("bytes {start}-{end}/{}", body.len()));
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
    if let Some(content_range) = content_range {
        headers.push_str(&format!("Content-Range: {content_range}\r\n"));
    }

    stream.write_all(status_line.as_bytes())?;
    stream.write_all(headers.as_bytes())?;
    stream.write_all(b"\r\n")?;

    if method.eq_ignore_ascii_case("HEAD") {
        stream.flush()?;
        return Ok(());
    }

    let first_body_request = config
        .request_started
        .as_ref()
        .map(|request_started| {
            request_started
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
        })
        .unwrap_or(false);
    if first_body_request {
        if let Some(release_response) = &config.release_response {
            while !release_response.load(Ordering::Acquire) {
                thread::sleep(Duration::from_millis(5));
            }
        }
    }

    if config.chunk_size == 0 {
        stream.write_all(&body)?;
    } else {
        for chunk in body.chunks(config.chunk_size) {
            stream.write_all(chunk)?;
            stream.flush()?;
            if !config.chunk_delay.is_zero() {
                thread::sleep(config.chunk_delay);
            }
        }
    }
    stream.flush()?;
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

pub(super) struct TestDaemon {
    pub(super) endpoint: String,
    shutdown: Arc<tokio::sync::Notify>,
    thread: Option<thread::JoinHandle<()>>,
}

impl TestDaemon {
    pub(super) fn start() -> Self {
        let endpoint = unique_test_endpoint();
        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
        let endpoint_for_thread = endpoint.clone();
        let thread = thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async move {
                let mut daemon = DownloadDaemon::bind(&endpoint_for_thread).unwrap();
                ready_tx.send(daemon.shutdown_handle()).unwrap();
                daemon.run().await.unwrap();
            });
        });
        let shutdown = ready_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("download daemon failed to bind");
        let client = DownloadClient::new(Some(endpoint.clone()));
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if client.daemon_status().is_ok() {
                return Self {
                    endpoint,
                    shutdown,
                    thread: Some(thread),
                };
            }
            thread::sleep(Duration::from_millis(50));
        }
        panic!("download daemon did not start in time");
    }
}

impl Drop for TestDaemon {
    fn drop(&mut self) {
        self.shutdown.notify_one();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn unique_test_endpoint() -> String {
    static NEXT_ID: AtomicUsize = AtomicUsize::new(1);
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    #[cfg(windows)]
    {
        format!(
            r"\\.\pipe\zccache-download-test-{}-{id}",
            std::process::id()
        )
    }
    #[cfg(unix)]
    {
        std::env::temp_dir()
            .join(format!(
                "zccache-download-test-{}-{id}.sock",
                std::process::id()
            ))
            .display()
            .to_string()
    }
}

pub(super) fn sha256_hex(data: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data);
    format!("{:x}", hasher.finalize())
}

#[test]
fn auto_detect_archive_formats() {
    assert_eq!(
        auto_archive_format(Path::new("toolchain.tar.gz")).unwrap(),
        ArchiveFormat::TarGz
    );
    assert_eq!(
        auto_archive_format(Path::new("toolchain.tar.xz")).unwrap(),
        ArchiveFormat::TarXz
    );
    assert_eq!(
        auto_archive_format(Path::new("toolchain.tar.zst")).unwrap(),
        ArchiveFormat::TarZst
    );
    assert_eq!(
        auto_archive_format(Path::new("toolchain.zip")).unwrap(),
        ArchiveFormat::Zip
    );
    assert_eq!(
        auto_archive_format(Path::new("toolchain.7z")).unwrap(),
        ArchiveFormat::SevenZip
    );
}

#[test]
fn safe_join_rejects_parent_traversal() {
    let err = safe_join(Path::new("out"), Path::new("../evil")).unwrap_err();
    assert!(err.contains("unsafe"));
}

#[test]
fn zip_extraction_rejects_path_traversal() {
    let dir = tempfile::tempdir().unwrap();
    let archive = dir.path().join("bad.zip");
    {
        let file = File::create(&archive).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        let options = zip::write::SimpleFileOptions::default();
        zip.start_file("../evil.txt", options).unwrap();
        zip.write_all(b"bad").unwrap();
        zip.finish().unwrap();
    }
    let out = dir.path().join("extract");
    let err = extract_zip(&archive, &out).unwrap_err();
    assert!(err.contains("unsafe zip entry"));
}

#[test]
fn tar_gz_extracts_regular_files() {
    let dir = tempfile::tempdir().unwrap();
    let archive = dir.path().join("ok.tar.gz");
    {
        let file = File::create(&archive).unwrap();
        let encoder = GzEncoder::new(file, Compression::default());
        let mut builder = tar::Builder::new(encoder);
        let data = b"hello";
        let mut header = tar::Header::new_gnu();
        header.set_size(data.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        builder
            .append_data(&mut header, "bin/tool.txt", &data[..])
            .unwrap();
        builder.finish().unwrap();
    }
    let out = dir.path().join("extract");
    let file = File::open(&archive).unwrap();
    let decoder = flate2::read::GzDecoder::new(file);
    extract_tar(decoder, &out).unwrap();
    assert_eq!(
        fs::read(out.join("bin").join("tool.txt")).unwrap(),
        b"hello"
    );
}
