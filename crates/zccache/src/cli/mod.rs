#![allow(clippy::missing_errors_doc)]

use crate::core::NormalizedPath;
use std::path::Path;

pub mod commands;
pub mod defender;
mod runtime;
pub mod snapshot_fp;
pub mod symbols;

// Re-export daemon-lifecycle helpers moved to `runtime.rs` (issue #365 wave 6)
// so the public API path is unchanged.
#[allow(deprecated)]
// gc_daemon_spawn_logs is deprecated but still re-exported for the public API.
pub use runtime::{
    connect_client, ensure_daemon, gc_daemon_spawn_logs, gc_log_directory, gc_log_directory_in,
    gc_runtime_binaries, gc_runtime_binaries_in, prepare_daemon_exe, prepare_daemon_exe_in,
    run_async, runtime_binaries_dir, spawn_daemon,
};

pub use crate::download_client::{
    ArchiveFormat, DownloadSource, FetchRequest, FetchResult, FetchState, FetchStateKind,
    FetchStatus, WaitMode,
};

#[derive(Debug, Clone)]
pub struct InoConvertOptions {
    pub clang_args: Vec<String>,
    pub inject_arduino_include: bool,
}

impl Default for InoConvertOptions {
    fn default() -> Self {
        Self {
            clang_args: Vec::new(),
            inject_arduino_include: true,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InoConvertResult {
    pub cache_hit: bool,
    pub skipped_write: bool,
}

#[derive(Debug, Clone)]
pub struct DownloadParams {
    pub source: DownloadSource,
    pub archive_path: Option<std::path::PathBuf>,
    pub unarchive_path: Option<std::path::PathBuf>,
    pub expected_sha256: Option<String>,
    pub archive_format: ArchiveFormat,
    pub max_connections: Option<usize>,
    pub min_segment_size: Option<u64>,
    pub wait_mode: WaitMode,
    pub dry_run: bool,
    pub force: bool,
}

impl DownloadParams {
    #[must_use]
    pub fn new(source: impl Into<DownloadSource>) -> Self {
        Self {
            source: source.into(),
            archive_path: None,
            unarchive_path: None,
            expected_sha256: None,
            archive_format: ArchiveFormat::Auto,
            max_connections: None,
            min_segment_size: None,
            wait_mode: WaitMode::Block,
            dry_run: false,
            force: false,
        }
    }
}

pub fn run_ino_convert_cached(
    input: &Path,
    output: &Path,
    options: &InoConvertOptions,
) -> Result<InoConvertResult, Box<dyn std::error::Error>> {
    let input_hash = crate::hash::hash_file(input)?;
    let mut hasher = crate::hash::StreamHasher::new();
    hasher.update(b"zccache-ino-convert-v1");
    hasher.update(input_hash.as_bytes());
    hasher.update(input.as_os_str().to_string_lossy().as_bytes());
    hasher.update(if options.inject_arduino_include {
        b"include-arduino-h"
    } else {
        b"no-arduino-h"
    });
    if let Some(libclang_hash) = crate::compiler::arduino::libclang_hash() {
        hasher.update(libclang_hash.as_bytes());
    }
    for arg in &options.clang_args {
        hasher.update(arg.as_bytes());
        hasher.update(b"\0");
    }
    let cache_key = hasher.finalize().to_hex();

    let cache_dir = crate::core::config::default_cache_dir().join("ino");
    std::fs::create_dir_all(&cache_dir)?;
    let cached_cpp = cache_dir.join(format!("{cache_key}.ino.cpp"));

    if cached_cpp.exists() {
        return restore_cached_ino_output(&cached_cpp, output);
    }

    let generated = crate::compiler::arduino::generate_ino_cpp(
        input,
        &crate::compiler::arduino::ArduinoConversionOptions {
            clang_args: options.clang_args.clone(),
            inject_arduino_include: options.inject_arduino_include,
        },
    )?;

    write_file_atomically(&cached_cpp, generated.cpp.as_bytes())?;
    restore_cached_ino_output(&cached_cpp, output).map(|_| InoConvertResult {
        cache_hit: false,
        skipped_write: false,
    })
}

fn restore_cached_ino_output(
    cached_cpp: &Path,
    output: &Path,
) -> Result<InoConvertResult, Box<dyn std::error::Error>> {
    if output.exists() {
        let output_hash = crate::hash::hash_file(output)?;
        let cached_hash = crate::hash::hash_file(cached_cpp)?;
        if output_hash == cached_hash {
            return Ok(InoConvertResult {
                cache_hit: true,
                skipped_write: true,
            });
        }
    }

    if let Some(parent) = output.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::copy(cached_cpp, output)?;
    Ok(InoConvertResult {
        cache_hit: true,
        skipped_write: false,
    })
}

fn write_file_atomically(path: &Path, data: &[u8]) -> Result<(), std::io::Error> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)?;

    let tmp = tempfile::NamedTempFile::new_in(parent)?;
    std::fs::write(tmp.path(), data)?;
    match tmp.persist(path) {
        Ok(_) => Ok(()),
        Err(err) => Err(err.error),
    }
}

/// Expose the daemon endpoint resolver as `pub` so the thin `zccache-cli`
/// pyo3 crate (which lives in a sibling workspace member) can hand it
/// to Python callers without round-tripping through a CLI subcommand.
/// Marked `pub` rather than `pub(crate)` because `zccache-cli` is a
/// separate crate per the Wave 7 monocrate split.
pub fn resolve_endpoint(explicit: Option<&str>) -> String {
    if let Some(ep) = explicit {
        return ep.to_string();
    }
    if let Ok(ep) = std::env::var("ZCCACHE_ENDPOINT") {
        return ep;
    }
    crate::ipc::default_endpoint()
}

pub fn infer_download_archive_path(
    source: &DownloadSource,
    archive_format: ArchiveFormat,
) -> std::path::PathBuf {
    let file_name = infer_download_file_name(source, archive_format);
    crate::core::config::default_cache_dir()
        .join("downloads")
        .join("artifacts")
        .join(file_name)
        .into_path_buf()
}

#[must_use]
pub fn build_download_request(params: DownloadParams) -> FetchRequest {
    let archive_path = params
        .archive_path
        .unwrap_or_else(|| infer_download_archive_path(&params.source, params.archive_format));
    let mut request = FetchRequest::new(params.source, archive_path);
    request.destination_path_expanded = params.unarchive_path;
    request.expected_sha256 = params.expected_sha256;
    request.archive_format = params.archive_format;
    request.wait_mode = params.wait_mode;
    request.dry_run = params.dry_run;
    request.force = params.force;
    request.download_options.force = params.force;
    request.download_options.max_connections = params.max_connections;
    request.download_options.min_segment_size = params.min_segment_size;
    request
}

pub fn client_download(
    endpoint: Option<&str>,
    params: DownloadParams,
) -> Result<FetchResult, String> {
    let request = build_download_request(params);
    let client = crate::download_client::DownloadClient::new(endpoint.map(ToOwned::to_owned));
    client.fetch(request)
}

pub fn client_download_exists(
    endpoint: Option<&str>,
    params: DownloadParams,
) -> Result<FetchState, String> {
    let request = build_download_request(params);
    let client = crate::download_client::DownloadClient::new(endpoint.map(ToOwned::to_owned));
    client.exists(&request)
}

fn infer_download_file_name(source: &DownloadSource, archive_format: ArchiveFormat) -> String {
    let base = infer_source_file_name(source);
    let hash = blake3::hash(download_source_key(source).as_bytes())
        .to_hex()
        .to_string();
    let suffix = archive_suffix(archive_format);

    if base.contains('.') || suffix.is_empty() {
        format!("{hash}-{base}")
    } else {
        format!("{hash}-{base}{suffix}")
    }
}

fn infer_source_file_name(source: &DownloadSource) -> String {
    match source {
        DownloadSource::Url(url) => {
            infer_url_file_name(url).unwrap_or_else(|| "download".to_string())
        }
        DownloadSource::MultipartUrls(urls) => infer_multipart_file_name(urls),
    }
}

fn infer_url_file_name(url: &str) -> Option<String> {
    url.split(['?', '#'])
        .next()
        .and_then(|value| value.rsplit('/').next())
        .filter(|value| !value.is_empty())
        .map(sanitize_download_file_name)
        .filter(|value| !value.is_empty())
}

fn infer_multipart_file_name(urls: &[String]) -> String {
    let base = urls
        .first()
        .and_then(|url| infer_url_file_name(url))
        .map(|name| strip_part_suffix(&name).to_string())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "multipart-download".to_string());
    if base.contains('.') {
        base
    } else {
        "multipart-download".to_string()
    }
}

fn strip_part_suffix(value: &str) -> &str {
    if let Some((base, suffix)) = value.rsplit_once(".part-") {
        if !base.is_empty() && !suffix.is_empty() {
            return base;
        }
    }
    if let Some((base, suffix)) = value.rsplit_once(".part_") {
        if !base.is_empty() && !suffix.is_empty() {
            return base;
        }
    }
    if let Some(index) = value.rfind(".part") {
        let suffix = &value[index + ".part".len()..];
        if !suffix.is_empty()
            && suffix
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_')
        {
            return &value[..index];
        }
    }
    value
}

fn download_source_key(source: &DownloadSource) -> String {
    match source {
        DownloadSource::Url(url) => url.clone(),
        DownloadSource::MultipartUrls(urls) => urls.join("\n"),
    }
}

fn sanitize_download_file_name(value: &str) -> String {
    value
        .chars()
        .map(|ch| match ch {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect()
}

fn archive_suffix(format: ArchiveFormat) -> &'static str {
    match format {
        ArchiveFormat::Auto | ArchiveFormat::None => "",
        ArchiveFormat::Zst => ".zst",
        ArchiveFormat::Zip => ".zip",
        ArchiveFormat::Xz => ".xz",
        ArchiveFormat::TarGz => ".tar.gz",
        ArchiveFormat::TarXz => ".tar.xz",
        ArchiveFormat::TarZst => ".tar.zst",
        ArchiveFormat::SevenZip => ".7z",
    }
}

#[derive(Debug, Clone)]
pub struct SessionStartResponse {
    pub session_id: String,
    pub journal_path: Option<String>,
}

pub fn client_start(endpoint: Option<&str>) -> Result<(), String> {
    let endpoint = resolve_endpoint(endpoint);
    run_async(async move { ensure_daemon(&endpoint).await })
}

pub fn client_stop(endpoint: Option<&str>) -> Result<bool, String> {
    let endpoint = resolve_endpoint(endpoint);
    run_async(async move {
        let mut conn = match connect_client(&endpoint).await {
            Ok(c) => c,
            Err(_) => return Ok(false),
        };
        conn.send(&crate::protocol::Request::Shutdown)
            .await
            .map_err(|e| format!("failed to send to daemon: {e}"))?;
        match conn.recv::<crate::protocol::Response>().await {
            Ok(Some(crate::protocol::Response::ShuttingDown)) => Ok(true),
            Ok(Some(crate::protocol::Response::Error { message })) => Err(message),
            Ok(None) => Err("lost connection to daemon (no response received)".to_string()),
            Ok(Some(other)) => Err(format!("unexpected response from daemon: {other:?}")),
            Err(e) => Err(format!("broken connection to daemon: {e}")),
        }
    })
}

pub fn client_status(endpoint: Option<&str>) -> Result<crate::protocol::DaemonStatus, String> {
    let endpoint = resolve_endpoint(endpoint);
    run_async(async move {
        let mut conn = connect_client(&endpoint)
            .await
            .map_err(|e| format!("daemon not running at {endpoint}: {e}"))?;
        conn.send(&crate::protocol::Request::Status)
            .await
            .map_err(|e| format!("failed to send to daemon: {e}"))?;
        match conn.recv::<crate::protocol::Response>().await {
            Ok(Some(crate::protocol::Response::Status(status))) => Ok(status),
            Ok(Some(crate::protocol::Response::Error { message })) => Err(message),
            Ok(None) => Err("lost connection to daemon (no response received)".to_string()),
            Ok(Some(other)) => Err(format!("unexpected response from daemon: {other:?}")),
            Err(e) => Err(format!("broken connection to daemon: {e}")),
        }
    })
}

pub fn client_session_start(
    endpoint: Option<&str>,
    cwd: &Path,
    log_file: Option<&Path>,
    track_stats: bool,
    journal_path: Option<&Path>,
) -> Result<SessionStartResponse, String> {
    let endpoint = resolve_endpoint(endpoint);
    let cwd = cwd.to_path_buf();
    let log_file = log_file.map(NormalizedPath::from);
    let journal_path = journal_path.map(NormalizedPath::from);

    run_async(async move {
        ensure_daemon(&endpoint).await?;
        let mut conn = connect_client(&endpoint)
            .await
            .map_err(|e| format!("cannot connect to daemon at {endpoint}: {e}"))?;
        conn.send(&crate::protocol::Request::SessionStart {
            client_pid: std::process::id(),
            working_dir: cwd.into(),
            log_file,
            track_stats,
            journal_path,
            profile: false,
        })
        .await
        .map_err(|e| format!("failed to send to daemon: {e}"))?;

        match conn.recv::<crate::protocol::Response>().await {
            Ok(Some(crate::protocol::Response::SessionStarted {
                session_id,
                journal_path,
            })) => Ok(SessionStartResponse {
                session_id,
                journal_path: journal_path.map(|p| p.display().to_string()),
            }),
            Ok(Some(crate::protocol::Response::Error { message })) => Err(message),
            Ok(None) => Err("lost connection to daemon (no response received)".to_string()),
            Ok(Some(other)) => Err(format!("unexpected response from daemon: {other:?}")),
            Err(e) => Err(format!("broken connection to daemon: {e}")),
        }
    })
}

/// End a session — daemon-unreachable is treated as a successful no-op.
///
/// Thin `String`-error wrapper around [`session_end_idempotent`]. All in-process
/// callers (Python bindings, soldr, future tools) route through here, so the
/// idempotency contract that #151 / #159 established for the CLI subprocess
/// path applies equally to library users. Without this, soldr's at-exit
/// `zccache session-end` from `rust-plan save` fails Windows CI with
/// "cannot connect to daemon at \\.\pipe\zccache-…" when the daemon already
/// exited — every workspace test passed but teardown failed.
pub fn client_session_end(
    endpoint: Option<&str>,
    session_id: &str,
) -> Result<Option<crate::protocol::SessionStats>, String> {
    let endpoint = resolve_endpoint(endpoint);
    session_end_idempotent(&endpoint, session_id).map_err(|e| e.to_string())
}

/// Is this connect-time error a "daemon process is gone entirely" error?
///
/// The conservative set: `NotFound` (Unix socket missing, Windows pipe
/// missing), `ConnectionRefused` (Unix socket exists but no listener;
/// Windows backoff helper synthesizes this when all pipe instances are
/// permanently busy), and `BrokenPipe` (race: pipe vanished between
/// open and use). Other errors (`TimedOut`, protocol mismatches, etc.)
/// are NOT daemon-gone — they should still fail loudly.
///
/// `IpcError::Timeout` is explicitly **NOT** in the unreachable set. A
/// timed-out recv means we connected successfully but the peer did not
/// respond in the configured window — that's either a hung daemon (a
/// real fault) or a per-call budget that was too tight (caller error).
/// Either way: propagate, don't silently swallow.
///
/// Used by `session_end_idempotent` (issue #159) and the CLI's
/// `cmd_session_end` (issue #150 / #151) to map "the daemon already
/// died" connect-time failures onto a success no-op. Other request
/// types keep their existing strict error semantics.
#[must_use]
pub fn is_daemon_unreachable_err(err: &crate::ipc::IpcError) -> bool {
    use std::io::ErrorKind;
    match err {
        crate::ipc::IpcError::Io(io) => matches!(
            io.kind(),
            ErrorKind::NotFound | ErrorKind::ConnectionRefused | ErrorKind::BrokenPipe
        ),
        _ => false,
    }
}

/// End a session, treating a vanished daemon as success.
///
/// This is the shared library entry point for ending a session. It is
/// the contract used by the CLI's `zccache session-end <uuid>`
/// subcommand AND by any in-process caller (e.g. soldr's at-exit
/// `rust-plan save`) — both must agree on what "the daemon already
/// died" means.
///
/// # Return shape
///
/// - `Ok(Some(stats))` — daemon was reached and returned stats for the
///   session.
/// - `Ok(None)` — daemon was reached but returned no stats (session
///   was tracked without stats), OR the daemon was unreachable at
///   connect time. Both are no-ops from the caller's perspective:
///   the session is implicitly ended when the daemon dies (see #137
///   for the daemon-side mirror), and a caller that just wants to
///   "end the session, don't care if the daemon is still alive"
///   should treat both as success.
/// - `Err(IpcError)` — anything else: timeouts, protocol mismatches,
///   send/recv mid-conversation failures, daemon error responses.
///   These are real faults and must be surfaced.
///
/// # Why a separate function
///
/// Issue #159: soldr was failing Windows CI on every main commit
/// because its in-process session-end (called from `rust-plan save`)
/// did not share code with `cmd_session_end`, so #151's
/// connect-failure idempotency only applied to the CLI subprocess
/// path. Promoting this contract to the library lets all callers —
/// current and future — share the same behavior.
pub fn session_end_idempotent(
    endpoint: &str,
    session_id: &str,
) -> Result<Option<crate::protocol::SessionStats>, crate::ipc::IpcError> {
    let endpoint = endpoint.to_string();
    let session_id = session_id.to_string();

    // Build a dedicated current-thread runtime. Can't use the existing
    // `run_async` helper because its `Output = Result<T, String>` shape
    // doesn't compose with our `Result<_, IpcError>` return type.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| {
            crate::ipc::IpcError::Endpoint(format!("failed to create tokio runtime: {e}"))
        })?;

    runtime.block_on(async move {
        let mut conn = match connect_client(&endpoint).await {
            Ok(c) => c,
            Err(e) => {
                if is_daemon_unreachable_err(&e) {
                    eprintln!(
                        "session-end: daemon unreachable at {endpoint}, treating session {session_id} as ended"
                    );
                    return Ok(None);
                }
                return Err(e);
            }
        };

        conn.send(&crate::protocol::Request::SessionEnd {
            session_id: session_id.clone(),
        })
        .await?;

        match conn.recv::<crate::protocol::Response>().await? {
            Some(crate::protocol::Response::SessionEnded { stats }) => Ok(stats),
            Some(crate::protocol::Response::Error { message }) => Err(
                crate::ipc::IpcError::Endpoint(format!("session-end failed: {message}")),
            ),
            None => Err(crate::ipc::IpcError::ConnectionClosed),
            Some(other) => Err(crate::ipc::IpcError::Endpoint(format!(
                "unexpected response from daemon: {other:?}"
            ))),
        }
    })
}

pub fn client_session_stats(
    endpoint: Option<&str>,
    session_id: &str,
) -> Result<Option<crate::protocol::SessionStats>, String> {
    let endpoint = resolve_endpoint(endpoint);
    let session_id = session_id.to_string();
    run_async(async move {
        let mut conn = connect_client(&endpoint)
            .await
            .map_err(|e| format!("cannot connect to daemon at {endpoint}: {e}"))?;
        conn.send(&crate::protocol::Request::SessionStats {
            session_id: session_id.clone(),
        })
        .await
        .map_err(|e| format!("failed to send to daemon: {e}"))?;

        match conn.recv::<crate::protocol::Response>().await {
            Ok(Some(crate::protocol::Response::SessionStatsResult { stats })) => Ok(stats),
            Ok(Some(crate::protocol::Response::Error { message })) => Err(message),
            Ok(None) => Err("lost connection to daemon (no response received)".to_string()),
            Ok(Some(other)) => Err(format!("unexpected response from daemon: {other:?}")),
            Err(e) => Err(format!("broken connection to daemon: {e}")),
        }
    })
}

#[derive(Debug, Clone)]
pub struct FingerprintCheckResponse {
    pub decision: String,
    pub reason: Option<String>,
    pub changed_files: Vec<String>,
}

pub fn fingerprint_check(
    endpoint: Option<&str>,
    cache_file: &Path,
    cache_type: &str,
    root: &Path,
    extensions: &[String],
    include_globs: &[String],
    exclude: &[String],
) -> Result<FingerprintCheckResponse, String> {
    let endpoint = resolve_endpoint(endpoint);
    let cache_file = cache_file.to_path_buf();
    let cache_type = cache_type.to_string();
    let root = root.to_path_buf();
    let extensions = extensions.to_vec();
    let include_globs = include_globs.to_vec();
    let exclude = exclude.to_vec();

    run_async(async move {
        ensure_daemon(&endpoint).await?;
        let mut conn = connect_client(&endpoint)
            .await
            .map_err(|e| format!("cannot connect to daemon at {endpoint}: {e}"))?;

        conn.send(&crate::protocol::Request::FingerprintCheck {
            cache_file: cache_file.into(),
            cache_type,
            root: root.into(),
            extensions,
            include_globs,
            exclude,
        })
        .await
        .map_err(|e| format!("failed to send to daemon: {e}"))?;

        match conn.recv::<crate::protocol::Response>().await {
            Ok(Some(crate::protocol::Response::FingerprintCheckResult {
                decision,
                reason,
                changed_files,
            })) => Ok(FingerprintCheckResponse {
                decision,
                reason,
                changed_files,
            }),
            Ok(Some(crate::protocol::Response::Error { message })) => Err(message),
            Ok(None) => Err("lost connection to daemon (no response received)".to_string()),
            Ok(Some(other)) => Err(format!("unexpected response from daemon: {other:?}")),
            Err(e) => Err(format!("broken connection to daemon: {e}")),
        }
    })
}

pub fn fingerprint_mark_success(endpoint: Option<&str>, cache_file: &Path) -> Result<(), String> {
    fingerprint_mark(endpoint, cache_file, true)
}

pub fn fingerprint_mark_failure(endpoint: Option<&str>, cache_file: &Path) -> Result<(), String> {
    fingerprint_mark(endpoint, cache_file, false)
}

fn fingerprint_mark(
    endpoint: Option<&str>,
    cache_file: &Path,
    success: bool,
) -> Result<(), String> {
    let endpoint = resolve_endpoint(endpoint);
    let cache_file = cache_file.to_path_buf();
    run_async(async move {
        ensure_daemon(&endpoint).await?;
        let mut conn = connect_client(&endpoint)
            .await
            .map_err(|e| format!("cannot connect to daemon at {endpoint}: {e}"))?;
        let request = if success {
            crate::protocol::Request::FingerprintMarkSuccess {
                cache_file: cache_file.into(),
            }
        } else {
            crate::protocol::Request::FingerprintMarkFailure {
                cache_file: cache_file.into(),
            }
        };
        conn.send(&request)
            .await
            .map_err(|e| format!("failed to send to daemon: {e}"))?;
        match conn.recv::<crate::protocol::Response>().await {
            Ok(Some(crate::protocol::Response::FingerprintAck)) => Ok(()),
            Ok(Some(crate::protocol::Response::Error { message })) => Err(message),
            Ok(None) => Err("lost connection to daemon (no response received)".to_string()),
            Ok(Some(other)) => Err(format!("unexpected response from daemon: {other:?}")),
            Err(e) => Err(format!("broken connection to daemon: {e}")),
        }
    })
}

pub fn fingerprint_invalidate(endpoint: Option<&str>, cache_file: &Path) -> Result<(), String> {
    let endpoint = resolve_endpoint(endpoint);
    let cache_file = cache_file.to_path_buf();
    run_async(async move {
        ensure_daemon(&endpoint).await?;
        let mut conn = connect_client(&endpoint)
            .await
            .map_err(|e| format!("cannot connect to daemon at {endpoint}: {e}"))?;
        conn.send(&crate::protocol::Request::FingerprintInvalidate {
            cache_file: cache_file.into(),
        })
        .await
        .map_err(|e| format!("failed to send to daemon: {e}"))?;
        match conn.recv::<crate::protocol::Response>().await {
            Ok(Some(crate::protocol::Response::FingerprintAck)) => Ok(()),
            Ok(Some(crate::protocol::Response::Error { message })) => Err(message),
            Ok(None) => Err("lost connection to daemon (no response received)".to_string()),
            Ok(Some(other)) => Err(format!("unexpected response from daemon: {other:?}")),
            Err(e) => Err(format!("broken connection to daemon: {e}")),
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn infer_download_path_keeps_url_filename() {
        let path = infer_download_archive_path(
            &DownloadSource::Url("https://example.com/releases/toolchain.tar.gz?download=1".into()),
            ArchiveFormat::Auto,
        );
        let file_name = path.file_name().unwrap().to_string_lossy();
        assert!(file_name.ends_with("-toolchain.tar.gz"));
    }

    #[test]
    fn infer_download_path_uses_archive_format_suffix_when_needed() {
        let path = infer_download_archive_path(
            &DownloadSource::Url("https://example.com/download".into()),
            ArchiveFormat::Zip,
        );
        let file_name = path.file_name().unwrap().to_string_lossy();
        assert!(file_name.ends_with(".zip"));
    }

    #[test]
    fn build_download_request_derives_archive_path_when_missing() {
        let request = build_download_request(DownloadParams::new("https://example.com/file.zip"));
        let file_name = request
            .destination_path
            .file_name()
            .unwrap()
            .to_string_lossy();
        assert!(file_name.ends_with("-file.zip"));
    }

    #[test]
    fn infer_download_path_strips_multipart_suffix_from_first_part() {
        let path = infer_download_archive_path(
            &DownloadSource::MultipartUrls(vec![
                "https://example.com/toolchain.tar.zst.part-aa".into(),
                "https://example.com/toolchain.tar.zst.part-ab".into(),
            ]),
            ArchiveFormat::Auto,
        );
        let file_name = path.file_name().unwrap().to_string_lossy();
        assert!(file_name.ends_with("-toolchain.tar.zst"));
    }

    #[test]
    fn prepare_daemon_exe_in_copies_to_target_dir() {
        let tmp = tempfile::tempdir().expect("create tempdir");
        let src = tmp.path().join("zccache-daemon.exe");
        std::fs::write(&src, b"fake-daemon-bytes").expect("write source");

        let dest_dir = tmp.path().join("runtime-binaries");
        let copied =
            prepare_daemon_exe_in(&src, &dest_dir).expect("prepare_daemon_exe_in succeeds");

        assert!(
            copied.is_file(),
            "copy at {} should exist",
            copied.display()
        );
        assert_eq!(
            copied.parent().unwrap(),
            dest_dir,
            "copy should land inside dest_dir"
        );
        assert!(
            copied
                .file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("zccache-daemon."),
            "filename should start with zccache-daemon., got {}",
            copied.display()
        );
        assert!(
            copied.extension().and_then(|s| s.to_str()) == Some("exe"),
            "extension should be preserved"
        );
        assert_eq!(
            std::fs::read(&copied).unwrap(),
            b"fake-daemon-bytes",
            "copy contents should match source"
        );
    }

    #[test]
    fn prepare_daemon_exe_in_creates_missing_dest_dir() {
        let tmp = tempfile::tempdir().expect("create tempdir");
        let src = tmp.path().join("zccache-daemon");
        std::fs::write(&src, b"x").expect("write source");

        let dest_dir = tmp.path().join("nested").join("runtime-binaries");
        assert!(!dest_dir.exists(), "precondition: dest_dir does not exist");

        let copied = prepare_daemon_exe_in(&src, &dest_dir).expect("create + copy");
        assert!(dest_dir.is_dir(), "dest_dir should now exist");
        assert!(copied.is_file());
    }

    #[test]
    fn gc_runtime_binaries_in_removes_unlocked_entries() {
        let tmp = tempfile::tempdir().expect("create tempdir");
        let dir = tmp.path().join("runtime-binaries");
        std::fs::create_dir_all(&dir).expect("create dir");

        let a = dir.join("zccache-daemon.111.exe");
        let b = dir.join("zccache-daemon.222.exe");
        std::fs::write(&a, b"a").unwrap();
        std::fs::write(&b, b"b").unwrap();

        gc_runtime_binaries_in(&dir);

        assert!(!a.exists(), "{} should be GC'd", a.display());
        assert!(!b.exists(), "{} should be GC'd", b.display());
        assert!(dir.is_dir(), "directory itself remains");
    }

    #[test]
    fn gc_runtime_binaries_in_is_noop_for_missing_dir() {
        let tmp = tempfile::tempdir().expect("create tempdir");
        let dir = tmp.path().join("does-not-exist");
        gc_runtime_binaries_in(&dir);
    }

    /// Issue #159: `session_end_idempotent` is the shared library entry
    /// point for ending a session — used by the CLI `session-end` command
    /// AND by tools like soldr that call into the library directly. When
    /// the daemon process is gone (pipe / socket missing), this function
    /// must return `Ok(None)` rather than propagating the connect-time
    /// I/O error. Soldr's at-exit `rust-plan save` previously failed
    /// Windows CI because its in-process session-end did NOT go through
    /// `cmd_session_end` (which is gated to the CLI subprocess path) and
    /// so the #151 idempotency fix didn't apply.
    #[test]
    fn session_end_idempotent_swallows_vanished_daemon() {
        // Construct an endpoint that is guaranteed to have no listener —
        // a unique pipe / socket name with no server bound to it.
        let endpoint = crate::ipc::unique_test_endpoint();
        let session_id = "00000000-0000-0000-0000-000000000000";

        let result = session_end_idempotent(&endpoint, session_id);

        assert!(
            matches!(result, Ok(None)),
            "vanished daemon must produce Ok(None) (success no-op), got {result:?}"
        );
    }

    /// Control: non-unreachable errors (the function shouldn't be a
    /// blanket "ignore everything"). We can't easily synthesize a live
    /// daemon error here, but we can at least assert the routing via the
    /// helper used inside the function: connect-time `TimedOut` must NOT
    /// be classified as unreachable, so the function would propagate it
    /// (rather than silently return Ok(None)). This guards against a
    /// regression where someone widens the unreachable set to "any I/O
    /// error".
    #[test]
    fn session_end_idempotent_treats_timeout_as_real_error() {
        let err = crate::ipc::IpcError::Io(std::io::Error::from(std::io::ErrorKind::TimedOut));
        assert!(
            !is_daemon_unreachable_err(&err),
            "TimedOut must NOT be classified as daemon-unreachable; session_end_idempotent \
             would otherwise silently swallow real timeouts"
        );
    }

    /// Control: protocol-layer errors (malformed framing, closed
    /// connection mid-response) must NOT be classified as unreachable.
    #[test]
    fn session_end_idempotent_treats_protocol_errors_as_real() {
        let err = crate::ipc::IpcError::ConnectionClosed;
        assert!(!is_daemon_unreachable_err(&err));
        let err = crate::ipc::IpcError::Endpoint("bogus".into());
        assert!(!is_daemon_unreachable_err(&err));
    }

    /// Issue #150: connect-time errors that mean "daemon process is gone
    /// entirely" must be classified as unreachable so the idempotent
    /// session-end paths (`session_end_idempotent` + the CLI's
    /// `cmd_session_end` wrapper) can fall through to the success path.
    /// The set covers every shape `connect()` actually returns when the
    /// pipe / socket is missing or has no listener.
    #[test]
    fn is_daemon_unreachable_recognizes_not_found() {
        let err = crate::ipc::IpcError::Io(std::io::Error::from(std::io::ErrorKind::NotFound));
        assert!(is_daemon_unreachable_err(&err));
    }

    #[test]
    fn is_daemon_unreachable_recognizes_connection_refused() {
        let err =
            crate::ipc::IpcError::Io(std::io::Error::from(std::io::ErrorKind::ConnectionRefused));
        assert!(is_daemon_unreachable_err(&err));
    }

    #[test]
    fn is_daemon_unreachable_recognizes_broken_pipe() {
        let err = crate::ipc::IpcError::Io(std::io::Error::from(std::io::ErrorKind::BrokenPipe));
        assert!(is_daemon_unreachable_err(&err));
    }

    /// `IpcError::Timeout` is explicitly NOT daemon-unreachable. A
    /// timed-out recv means we connected successfully but the peer did
    /// not respond — that's a hung-daemon fault, not a vanished daemon.
    /// Soldr's at-exit `session_end` path classifies vanished-daemon as
    /// a no-op; if `Timeout` were misclassified here, a stuck daemon
    /// would be silently swallowed and the user would never see it.
    #[test]
    fn is_daemon_unreachable_timeout_is_not_unreachable() {
        let err = crate::ipc::IpcError::Timeout(std::time::Duration::from_secs(5));
        assert!(
            !is_daemon_unreachable_err(&err),
            "Timeout must propagate as a real fault, not be swallowed as daemon-unreachable"
        );
    }

    /// Mapping ENOENT through `from_raw_os_error` must yield the same
    /// classification as constructing from `ErrorKind::NotFound`. This
    /// guards against platform variance (macOS / Linux / Windows could
    /// in principle synthesize a different kind for the same errno).
    #[test]
    fn is_daemon_unreachable_recognizes_raw_enoent() {
        // ENOENT == 2 on every Unix; on Windows ERROR_FILE_NOT_FOUND == 2 too.
        let err = crate::ipc::IpcError::Io(std::io::Error::from_raw_os_error(2));
        assert!(
            is_daemon_unreachable_err(&err),
            "errno 2 must map to a kind in the unreachable set; got kind={:?}",
            match &err {
                crate::ipc::IpcError::Io(io) => io.kind(),
                _ => unreachable!(),
            }
        );
    }

    /// Regression: `client_session_end` is the in-process library entry point
    /// used by Python bindings and external tools (soldr's `rust-plan save`).
    /// It must mirror `session_end_idempotent` — a vanished daemon is a no-op
    /// success, not a hard error. Before this fix, soldr called
    /// `client_session_end`, got `Err("cannot connect to daemon at …")`,
    /// surfaced it as "soldr: zccache session-end … failed: …", and Windows
    /// Test failed teardown even after every workspace test passed.
    #[test]
    fn client_session_end_swallows_vanished_daemon() {
        let endpoint = crate::ipc::unique_test_endpoint();
        let session_id = "00000000-0000-0000-0000-000000000000";

        let result = client_session_end(Some(&endpoint), session_id);

        assert!(
            matches!(result, Ok(None)),
            "vanished daemon must produce Ok(None) (success no-op), got {result:?}"
        );
    }

    /// `gc_log_directory_in` must:
    /// 1. delete every stale file regardless of name (not just
    ///    `daemon-spawn-*.log`), so leftover `daemon-lifecycle.log.1`,
    ///    `daemon.log.<ts>`, `compile_journal.jsonl.<ts>`, and stray
    ///    files from previous versions all get reaped;
    /// 2. preserve the live `daemon-lifecycle.log` even when it's
    ///    older than the cutoff — a long-idle daemon may only touch
    ///    it twice (at `spawn` and `died-*`).
    #[test]
    fn gc_log_directory_sweeps_stale_files_and_preserves_lifecycle_log() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let logs = tmp.path();

        // Fresh files (mtime = now). Must all survive a sweep with a
        // 60-second cutoff regardless of name.
        for name in [
            "daemon-lifecycle.log",
            "daemon-lifecycle-soldr-dev.log",
            "daemon-lifecycle.log.1",
            "daemon-lifecycle-soldr-dev.log.1",
            "daemon-spawn-1234-9999.log",
            "daemon.log",
            "daemon.log.2026-01-01T00-00-00Z",
            "compile_journal.jsonl",
            "compile_journal.jsonl.2026-01-01T00-00-00Z",
            "last-session.log",
            "stray-from-external-tool.txt",
        ] {
            std::fs::write(logs.join(name), b"x").unwrap();
        }

        gc_log_directory_in(logs, std::time::Duration::from_secs(60));

        for name in [
            "daemon-lifecycle.log",
            "daemon-lifecycle-soldr-dev.log",
            "daemon-lifecycle.log.1",
            "daemon-lifecycle-soldr-dev.log.1",
            "daemon-spawn-1234-9999.log",
            "daemon.log",
            "daemon.log.2026-01-01T00-00-00Z",
            "compile_journal.jsonl",
            "compile_journal.jsonl.2026-01-01T00-00-00Z",
            "last-session.log",
            "stray-from-external-tool.txt",
        ] {
            assert!(
                logs.join(name).exists(),
                "{name} must survive when mtime is fresh"
            );
        }

        // Now age every file by overwriting mtime to two days ago.
        // Then sweep with a 24h cutoff. Only `daemon-lifecycle.log`
        // should survive — it's the live writer and may sit idle for
        // an arbitrarily long time between events.
        let two_days_ago =
            std::time::SystemTime::now() - std::time::Duration::from_secs(60 * 60 * 48);
        for name in [
            "daemon-lifecycle.log",
            "daemon-lifecycle-soldr-dev.log",
            "daemon-lifecycle.log.1",
            "daemon-lifecycle-soldr-dev.log.1",
            "daemon-spawn-1234-9999.log",
            "daemon.log",
            "daemon.log.2026-01-01T00-00-00Z",
            "compile_journal.jsonl",
            "compile_journal.jsonl.2026-01-01T00-00-00Z",
            "last-session.log",
            "stray-from-external-tool.txt",
        ] {
            let path = logs.join(name);
            let f = std::fs::File::options().write(true).open(&path).unwrap();
            f.set_modified(two_days_ago).unwrap();
        }

        gc_log_directory_in(logs, std::time::Duration::from_secs(60 * 60 * 24));

        assert!(
            logs.join("daemon-lifecycle.log").exists(),
            "active lifecycle log must be preserved even when stale"
        );
        assert!(
            logs.join("daemon-lifecycle-soldr-dev.log").exists(),
            "active namespaced lifecycle log must be preserved even when stale"
        );
        for name in [
            "daemon-lifecycle.log.1",
            "daemon-lifecycle-soldr-dev.log.1",
            "daemon-spawn-1234-9999.log",
            "daemon.log",
            "daemon.log.2026-01-01T00-00-00Z",
            "compile_journal.jsonl",
            "compile_journal.jsonl.2026-01-01T00-00-00Z",
            "last-session.log",
            "stray-from-external-tool.txt",
        ] {
            assert!(
                !logs.join(name).exists(),
                "{name} should have been swept (older than 24h cutoff)"
            );
        }
    }

    /// Sweeping a nonexistent directory is a silent no-op (called
    /// before every spawn — must never fail on a fresh install).
    #[test]
    fn gc_log_directory_silently_handles_missing_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("does-not-exist");
        gc_log_directory_in(&missing, std::time::Duration::from_secs(60));
        assert!(!missing.exists());
    }
}
