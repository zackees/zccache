#![allow(clippy::missing_errors_doc)]

use std::path::Path;
use zccache_core::NormalizedPath;

#[cfg(feature = "python")]
mod python;

pub use zccache_download_client::{
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
    let input_hash = zccache_hash::hash_file(input)?;
    let mut hasher = zccache_hash::StreamHasher::new();
    hasher.update(b"zccache-ino-convert-v1");
    hasher.update(input_hash.as_bytes());
    hasher.update(input.as_os_str().to_string_lossy().as_bytes());
    hasher.update(if options.inject_arduino_include {
        b"include-arduino-h"
    } else {
        b"no-arduino-h"
    });
    if let Some(libclang_hash) = zccache_compiler::arduino::libclang_hash() {
        hasher.update(libclang_hash.as_bytes());
    }
    for arg in &options.clang_args {
        hasher.update(arg.as_bytes());
        hasher.update(b"\0");
    }
    let cache_key = hasher.finalize().to_hex();

    let cache_dir = zccache_core::config::default_cache_dir().join("ino");
    std::fs::create_dir_all(&cache_dir)?;
    let cached_cpp = cache_dir.join(format!("{cache_key}.ino.cpp"));

    if cached_cpp.exists() {
        return restore_cached_ino_output(&cached_cpp, output);
    }

    let generated = zccache_compiler::arduino::generate_ino_cpp(
        input,
        &zccache_compiler::arduino::ArduinoConversionOptions {
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
        let output_hash = zccache_hash::hash_file(output)?;
        let cached_hash = zccache_hash::hash_file(cached_cpp)?;
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

fn resolve_endpoint(explicit: Option<&str>) -> String {
    if let Some(ep) = explicit {
        return ep.to_string();
    }
    if let Ok(ep) = std::env::var("ZCCACHE_ENDPOINT") {
        return ep;
    }
    zccache_ipc::default_endpoint()
}

pub fn infer_download_archive_path(
    source: &DownloadSource,
    archive_format: ArchiveFormat,
) -> std::path::PathBuf {
    let file_name = infer_download_file_name(source, archive_format);
    zccache_core::config::default_cache_dir()
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
    let client = zccache_download_client::DownloadClient::new(endpoint.map(ToOwned::to_owned));
    client.fetch(request)
}

pub fn client_download_exists(
    endpoint: Option<&str>,
    params: DownloadParams,
) -> Result<FetchState, String> {
    let request = build_download_request(params);
    let client = zccache_download_client::DownloadClient::new(endpoint.map(ToOwned::to_owned));
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

fn run_async<T>(future: impl std::future::Future<Output = Result<T, String>>) -> Result<T, String> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("failed to create tokio runtime: {e}"))?
        .block_on(future)
}

#[derive(Debug)]
enum VersionCheck {
    Ok,
    Unreachable,
    DaemonOlder { daemon_ver: String },
    DaemonNewer,
    CommError,
}

#[cfg(unix)]
async fn connect_client(
    endpoint: &str,
) -> Result<zccache_ipc::IpcConnection, zccache_ipc::IpcError> {
    zccache_ipc::connect(endpoint).await
}

#[cfg(windows)]
async fn connect_client(
    endpoint: &str,
) -> Result<zccache_ipc::IpcClientConnection, zccache_ipc::IpcError> {
    zccache_ipc::connect(endpoint).await
}

async fn check_daemon_version(endpoint: &str) -> VersionCheck {
    let mut conn = match connect_client(endpoint).await {
        Ok(c) => c,
        Err(_) => return VersionCheck::Unreachable,
    };
    if conn.send(&zccache_protocol::Request::Status).await.is_err() {
        return VersionCheck::CommError;
    }
    match conn.recv::<zccache_protocol::Response>().await {
        Ok(Some(zccache_protocol::Response::Status(s))) => {
            if s.version == zccache_core::VERSION {
                return VersionCheck::Ok;
            }
            let client_ver = zccache_core::version::current();
            match zccache_core::version::Version::parse(&s.version) {
                Some(daemon_ver) => match daemon_ver.cmp(&client_ver) {
                    std::cmp::Ordering::Equal => VersionCheck::Ok,
                    std::cmp::Ordering::Greater => VersionCheck::DaemonNewer,
                    std::cmp::Ordering::Less => VersionCheck::DaemonOlder {
                        daemon_ver: s.version,
                    },
                },
                None => VersionCheck::DaemonOlder {
                    daemon_ver: s.version,
                },
            }
        }
        _ => VersionCheck::CommError,
    }
}

async fn spawn_and_wait(endpoint: &str) -> Result<(), String> {
    let daemon_bin = find_daemon_binary().ok_or("cannot find zccache-daemon binary")?;
    spawn_daemon(&daemon_bin, endpoint)?;

    for _ in 0..100 {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        if connect_client(endpoint).await.is_ok() {
            return Ok(());
        }
    }
    Err("daemon started but not accepting connections after 10s".to_string())
}

/// Stop a stale daemon that is unreachable or version-incompatible.
async fn stop_stale_daemon(endpoint: &str) {
    if let Ok(mut conn) = connect_client(endpoint).await {
        let _ = conn.send(&zccache_protocol::Request::Shutdown).await;
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }

    if let Some(pid) = zccache_ipc::check_running_daemon() {
        if zccache_ipc::force_kill_process(pid).is_ok() {
            for _ in 0..50 {
                if !zccache_ipc::is_process_alive(pid) {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        }
        zccache_ipc::remove_lock_file();
    }

    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
}

async fn ensure_daemon(endpoint: &str) -> Result<(), String> {
    match check_daemon_version(endpoint).await {
        VersionCheck::Ok | VersionCheck::DaemonNewer => return Ok(()),
        VersionCheck::DaemonOlder { daemon_ver } => {
            tracing::info!(
                daemon_ver,
                client_ver = zccache_core::VERSION,
                "daemon is older than client, auto-recovering"
            );
            stop_stale_daemon(endpoint).await;
            return spawn_and_wait(endpoint).await;
        }
        VersionCheck::CommError => {
            tracing::info!("cannot communicate with daemon, auto-recovering");
            stop_stale_daemon(endpoint).await;
            return spawn_and_wait(endpoint).await;
        }
        VersionCheck::Unreachable => {}
    }

    if let Some(pid) = zccache_ipc::check_running_daemon() {
        let mut backoff = std::time::Duration::from_millis(100);
        for _ in 0..20 {
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(std::time::Duration::from_millis(500));
            match check_daemon_version(endpoint).await {
                VersionCheck::Ok | VersionCheck::DaemonNewer => return Ok(()),
                VersionCheck::DaemonOlder { daemon_ver } => {
                    tracing::info!(
                        daemon_ver,
                        client_ver = zccache_core::VERSION,
                        "daemon is older than client during startup, auto-recovering"
                    );
                    stop_stale_daemon(endpoint).await;
                    return spawn_and_wait(endpoint).await;
                }
                VersionCheck::CommError => {
                    stop_stale_daemon(endpoint).await;
                    return spawn_and_wait(endpoint).await;
                }
                VersionCheck::Unreachable => continue,
            }
        }
        return Err(format!(
            "daemon process {pid} exists but not accepting connections after retrying"
        ));
    }

    spawn_and_wait(endpoint).await
}

fn find_daemon_binary() -> Option<NormalizedPath> {
    let name = if cfg!(windows) {
        "zccache-daemon.exe"
    } else {
        "zccache-daemon"
    };

    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let candidate = dir.join(name);
            if candidate.exists() {
                return Some(candidate.into());
            }
        }
    }

    which_on_path(name)
}

fn which_on_path(name: &str) -> Option<NormalizedPath> {
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate.into());
        }
        #[cfg(windows)]
        if Path::new(name).extension().is_none() {
            let with_exe = dir.join(format!("{name}.exe"));
            if with_exe.is_file() {
                return Some(with_exe.into());
            }
        }
    }
    None
}

/// Initialize spawn-lineage env vars on a command the CLI is about to spawn.
///
/// Mirrors the daemon-side propagation in `zccache_daemon::lineage` so that
/// any process attribution (orphan tracking, running-process scanners) sees
/// a consistent chain across CLI -> daemon -> compiler hops. The chain is
/// initialized with the CLI's PID, and the originator marker (used by
/// running-process for crash-resilient orphan discovery) is set to
/// `zccache-cli:<pid>` unless an outer tool has already claimed it.
fn apply_cli_spawn_lineage(cmd: &mut std::process::Command) {
    const ENV_ORIGINATOR: &str = "RUNNING_PROCESS_ORIGINATOR";
    const ENV_LINEAGE: &str = "ZCCACHE_LINEAGE";
    const ENV_PARENT_PID: &str = "ZCCACHE_PARENT_PID";
    const ENV_CLIENT_PID: &str = "ZCCACHE_CLIENT_PID";

    let cli_pid = std::process::id();

    // Preserve any outer originator (e.g. the build tool was already wrapped
    // by running-process). Otherwise, claim the originator slot ourselves.
    if std::env::var(ENV_ORIGINATOR).is_err() {
        cmd.env(ENV_ORIGINATOR, format!("zccache-cli:{cli_pid}"));
    }

    // Extend or initialize the chain with our PID.
    let chain = match std::env::var(ENV_LINEAGE) {
        Ok(existing)
            if existing
                .rsplit_once('>')
                .map_or(existing.as_str(), |(_, last)| last)
                != cli_pid.to_string() =>
        {
            format!("{existing}>{cli_pid}")
        }
        Ok(existing) => existing,
        Err(_) => cli_pid.to_string(),
    };
    cmd.env(ENV_LINEAGE, chain);
    cmd.env(ENV_PARENT_PID, cli_pid.to_string());
    cmd.env(ENV_CLIENT_PID, cli_pid.to_string());
}

fn spawn_daemon(bin: &Path, endpoint: &str) -> Result<(), String> {
    let mut cmd = std::process::Command::new(bin);
    cmd.args(["--foreground", "--endpoint", endpoint]);
    cmd.stdin(std::process::Stdio::null());
    cmd.stdout(std::process::Stdio::null());
    cmd.stderr(std::process::Stdio::null());

    apply_cli_spawn_lineage(&mut cmd);

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
        disable_handle_inheritance();
    }

    cmd.spawn()
        .map_err(|e| format!("failed to spawn daemon: {e}"))?;

    #[cfg(windows)]
    restore_handle_inheritance();

    Ok(())
}

#[cfg(windows)]
fn disable_handle_inheritance() {
    use std::os::windows::io::AsRawHandle;

    extern "system" {
        fn SetHandleInformation(handle: *mut std::ffi::c_void, mask: u32, flags: u32) -> i32;
    }
    const HANDLE_FLAG_INHERIT: u32 = 1;

    unsafe {
        let stdout = std::io::stdout().as_raw_handle();
        let stderr = std::io::stderr().as_raw_handle();
        let _ = SetHandleInformation(stdout.cast(), HANDLE_FLAG_INHERIT, 0);
        let _ = SetHandleInformation(stderr.cast(), HANDLE_FLAG_INHERIT, 0);
    }
}

#[cfg(windows)]
fn restore_handle_inheritance() {
    use std::os::windows::io::AsRawHandle;

    extern "system" {
        fn SetHandleInformation(handle: *mut std::ffi::c_void, mask: u32, flags: u32) -> i32;
    }
    const HANDLE_FLAG_INHERIT: u32 = 1;

    unsafe {
        let stdout = std::io::stdout().as_raw_handle();
        let stderr = std::io::stderr().as_raw_handle();
        let _ = SetHandleInformation(stdout.cast(), HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT);
        let _ = SetHandleInformation(stderr.cast(), HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT);
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
        conn.send(&zccache_protocol::Request::Shutdown)
            .await
            .map_err(|e| format!("failed to send to daemon: {e}"))?;
        match conn.recv::<zccache_protocol::Response>().await {
            Ok(Some(zccache_protocol::Response::ShuttingDown)) => Ok(true),
            Ok(Some(zccache_protocol::Response::Error { message })) => Err(message),
            Ok(None) => Err("lost connection to daemon (no response received)".to_string()),
            Ok(Some(other)) => Err(format!("unexpected response from daemon: {other:?}")),
            Err(e) => Err(format!("broken connection to daemon: {e}")),
        }
    })
}

pub fn client_status(endpoint: Option<&str>) -> Result<zccache_protocol::DaemonStatus, String> {
    let endpoint = resolve_endpoint(endpoint);
    run_async(async move {
        let mut conn = connect_client(&endpoint)
            .await
            .map_err(|e| format!("daemon not running at {endpoint}: {e}"))?;
        conn.send(&zccache_protocol::Request::Status)
            .await
            .map_err(|e| format!("failed to send to daemon: {e}"))?;
        match conn.recv::<zccache_protocol::Response>().await {
            Ok(Some(zccache_protocol::Response::Status(status))) => Ok(status),
            Ok(Some(zccache_protocol::Response::Error { message })) => Err(message),
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
        conn.send(&zccache_protocol::Request::SessionStart {
            client_pid: std::process::id(),
            working_dir: cwd.into(),
            log_file,
            track_stats,
            journal_path,
        })
        .await
        .map_err(|e| format!("failed to send to daemon: {e}"))?;

        match conn.recv::<zccache_protocol::Response>().await {
            Ok(Some(zccache_protocol::Response::SessionStarted {
                session_id,
                journal_path,
            })) => Ok(SessionStartResponse {
                session_id,
                journal_path: journal_path.map(|p| p.display().to_string()),
            }),
            Ok(Some(zccache_protocol::Response::Error { message })) => Err(message),
            Ok(None) => Err("lost connection to daemon (no response received)".to_string()),
            Ok(Some(other)) => Err(format!("unexpected response from daemon: {other:?}")),
            Err(e) => Err(format!("broken connection to daemon: {e}")),
        }
    })
}

pub fn client_session_end(
    endpoint: Option<&str>,
    session_id: &str,
) -> Result<Option<zccache_protocol::SessionStats>, String> {
    let endpoint = resolve_endpoint(endpoint);
    let session_id = session_id.to_string();
    run_async(async move {
        let mut conn = connect_client(&endpoint)
            .await
            .map_err(|e| format!("cannot connect to daemon at {endpoint}: {e}"))?;
        conn.send(&zccache_protocol::Request::SessionEnd {
            session_id: session_id.clone(),
        })
        .await
        .map_err(|e| format!("failed to send to daemon: {e}"))?;

        match conn.recv::<zccache_protocol::Response>().await {
            Ok(Some(zccache_protocol::Response::SessionEnded { stats })) => Ok(stats),
            Ok(Some(zccache_protocol::Response::Error { message })) => Err(message),
            Ok(None) => Err("lost connection to daemon (no response received)".to_string()),
            Ok(Some(other)) => Err(format!("unexpected response from daemon: {other:?}")),
            Err(e) => Err(format!("broken connection to daemon: {e}")),
        }
    })
}

pub fn client_session_stats(
    endpoint: Option<&str>,
    session_id: &str,
) -> Result<Option<zccache_protocol::SessionStats>, String> {
    let endpoint = resolve_endpoint(endpoint);
    let session_id = session_id.to_string();
    run_async(async move {
        let mut conn = connect_client(&endpoint)
            .await
            .map_err(|e| format!("cannot connect to daemon at {endpoint}: {e}"))?;
        conn.send(&zccache_protocol::Request::SessionStats {
            session_id: session_id.clone(),
        })
        .await
        .map_err(|e| format!("failed to send to daemon: {e}"))?;

        match conn.recv::<zccache_protocol::Response>().await {
            Ok(Some(zccache_protocol::Response::SessionStatsResult { stats })) => Ok(stats),
            Ok(Some(zccache_protocol::Response::Error { message })) => Err(message),
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

        conn.send(&zccache_protocol::Request::FingerprintCheck {
            cache_file: cache_file.into(),
            cache_type,
            root: root.into(),
            extensions,
            include_globs,
            exclude,
        })
        .await
        .map_err(|e| format!("failed to send to daemon: {e}"))?;

        match conn.recv::<zccache_protocol::Response>().await {
            Ok(Some(zccache_protocol::Response::FingerprintCheckResult {
                decision,
                reason,
                changed_files,
            })) => Ok(FingerprintCheckResponse {
                decision,
                reason,
                changed_files,
            }),
            Ok(Some(zccache_protocol::Response::Error { message })) => Err(message),
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
            zccache_protocol::Request::FingerprintMarkSuccess {
                cache_file: cache_file.into(),
            }
        } else {
            zccache_protocol::Request::FingerprintMarkFailure {
                cache_file: cache_file.into(),
            }
        };
        conn.send(&request)
            .await
            .map_err(|e| format!("failed to send to daemon: {e}"))?;
        match conn.recv::<zccache_protocol::Response>().await {
            Ok(Some(zccache_protocol::Response::FingerprintAck)) => Ok(()),
            Ok(Some(zccache_protocol::Response::Error { message })) => Err(message),
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
        conn.send(&zccache_protocol::Request::FingerprintInvalidate {
            cache_file: cache_file.into(),
        })
        .await
        .map_err(|e| format!("failed to send to daemon: {e}"))?;
        match conn.recv::<zccache_protocol::Response>().await {
            Ok(Some(zccache_protocol::Response::FingerprintAck)) => Ok(()),
            Ok(Some(zccache_protocol::Response::Error { message })) => Err(message),
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
}
