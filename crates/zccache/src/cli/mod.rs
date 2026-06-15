#![allow(clippy::missing_errors_doc)]

use std::path::Path;

pub mod client;
pub mod commands;
pub mod defender;
mod runtime;
pub mod snapshot_fp;
pub mod symbols;

/// Default per-call timeout for the `Status` probe used by daemon version
/// checks. Two seconds keeps startup responsive when an existing daemon is
/// alive but IPC-deaf, while still leaving normal Compile/Link roundtrips on
/// the generous global client recv timeout.
const STATUS_PROBE_DEFAULT_SECS: u64 = 2;

/// Returns the per-call timeout for daemon version-probe Status recv calls,
/// honoring `ZCCACHE_STATUS_PROBE_TIMEOUT_SECS` when it parses as `u64`.
pub(crate) fn status_probe_timeout() -> std::time::Duration {
    let secs = std::env::var("ZCCACHE_STATUS_PROBE_TIMEOUT_SECS")
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(STATUS_PROBE_DEFAULT_SECS);
    std::time::Duration::from_secs(secs)
}

/// Default per-call timeout for Compile/Link recv before treating the daemon
/// as wedged and triggering recovery (issue #666). The pre-#666 behavior was
/// the 300 s global `DEFAULT_CLIENT_RECV_TIMEOUT`; under a wedge, every
/// parallel ninja job paid the full window.
///
/// Budget was widened from 90 s → 180 s in issue #726. Under FastLED's
/// `all-with-examples` ninja target (~341 link TUs over a ~150 s wall window),
/// the 90 s ceiling tripped for *healthy* daemons that were simply bottlenecked
/// behind the burst — the client then sent `Request::Shutdown`, killed the
/// daemon mid-work, and triggered a thundering-herd respawn. 180 s preserves
/// wedge detection (real wedges show up as recv stalls measured in minutes,
/// not seconds) while leaving headroom for legitimate burst link loads on a
/// busy daemon. The 300 s pre-#666 behavior is still recoverable via the env
/// var override.
///
/// On timeout the wrapper force-kills the wedged daemon via
/// `stop_stale_daemon`, ensures a fresh one is spawned, and retries the
/// request exactly once on the fresh daemon. Subsequent ninja workers that
/// queue up while the kill is in flight see no daemon at connect time and
/// take the normal spawn path — so only one worker pays the recovery cost,
/// not all 673.
///
/// Overridable via `ZCCACHE_WEDGE_RECV_TIMEOUT_SECS`; set to `0` to disable
/// the wedge detection entirely and keep the historical 300 s behavior.
const WEDGE_RECV_DEFAULT_SECS: u64 = 180;

/// Returns the wedge-detection recv timeout for Compile/Link calls. `None`
/// means "disabled" (the env var was set to `0`). See [`WEDGE_RECV_DEFAULT_SECS`].
pub(crate) fn wedge_recv_timeout() -> Option<std::time::Duration> {
    let secs = std::env::var("ZCCACHE_WEDGE_RECV_TIMEOUT_SECS")
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(WEDGE_RECV_DEFAULT_SECS);
    if secs == 0 {
        None
    } else {
        Some(std::time::Duration::from_secs(secs))
    }
}

/// Default number of retries the link/compile clients apply on a
/// transport-level recv failure (`lost connection to daemon`). Set to
/// 1 — a single retry covers the "daemon crashed mid-request, fresh
/// spawn fixes it" failure FastLED/FastLED#3011 surfaces; two failures
/// in a row almost certainly mean a real bug worth surfacing.
const LINK_RETRY_DEFAULT_BUDGET: u32 = 1;

/// Returns the number of automatic retries to attempt on a transport
/// failure before surfacing it as `ExitCode::FAILURE`. Issue #752.
///
/// Override via `ZCCACHE_DISABLE_LINK_RETRY=1` to opt back into the
/// pre-#752 fail-fast behavior (mirrors the
/// `ZCCACHE_WEDGE_RECV_TIMEOUT_SECS=0` shape).
pub(crate) fn link_retry_budget() -> u32 {
    if std::env::var_os("ZCCACHE_DISABLE_LINK_RETRY").is_some_and(|v| v == "1") {
        0
    } else {
        LINK_RETRY_DEFAULT_BUDGET
    }
}

// Re-export daemon-lifecycle helpers moved to `runtime.rs` (issue #365 wave 6)
// so the public API path is unchanged.
#[allow(deprecated)]
// gc_daemon_spawn_logs is deprecated but still re-exported for the public API.
pub use runtime::{
    connect_client, ensure_daemon, gc_daemon_spawn_logs, gc_log_directory, gc_log_directory_in,
    gc_runtime_binaries, gc_runtime_binaries_in, prepare_daemon_exe, prepare_daemon_exe_in,
    run_async, runtime_binaries_dir, spawn_daemon, wait_for_daemon_ready,
};

pub use crate::download_client::{
    ArchiveFormat, DownloadSource, FetchRequest, FetchResult, FetchState, FetchStateKind,
    FetchStatus, WaitMode,
};
pub use client::{
    client_session_end, client_session_start, client_session_stats, client_start, client_status,
    client_stop, fingerprint_check, fingerprint_invalidate, fingerprint_mark_failure,
    fingerprint_mark_success, is_daemon_unreachable_err, session_end_idempotent,
    FingerprintCheckResponse, SessionStartResponse,
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
