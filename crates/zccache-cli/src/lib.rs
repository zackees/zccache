#![cfg(feature = "python")]

use std::path::PathBuf;

use pyo3::exceptions::{PyOSError, PyRuntimeError};
use pyo3::prelude::*;
use pyo3::types::PyAny;

use zccache::cli::{
    build_download_request, client_download, client_download_exists, client_session_end,
    client_session_start, client_session_stats, client_start, client_status, client_stop,
    fingerprint_check, fingerprint_invalidate, fingerprint_mark_failure, fingerprint_mark_success,
    run_ino_convert_cached, DownloadParams, DownloadSource, InoConvertOptions, WaitMode,
};

fn runtime_to_py_err(message: String) -> PyErr {
    PyErr::new::<PyRuntimeError, _>(message)
}

fn parse_download_source(source: &Bound<'_, PyAny>) -> PyResult<DownloadSource> {
    if let Ok(url) = source.extract::<String>() {
        return Ok(DownloadSource::Url(url));
    }
    if let Ok(urls) = source.extract::<Vec<String>>() {
        return Ok(DownloadSource::MultipartUrls(urls));
    }
    Err(PyErr::new::<PyRuntimeError, _>(
        "source must be a URL string or a list of URL strings",
    ))
}

#[pyclass(module = "zccache._native")]
#[derive(Clone)]
pub struct NativeDaemonStatus {
    #[pyo3(get)]
    version: String,
    #[pyo3(get)]
    artifact_count: u64,
    #[pyo3(get)]
    cache_size_bytes: u64,
    #[pyo3(get)]
    metadata_entries: u64,
    #[pyo3(get)]
    uptime_secs: u64,
    #[pyo3(get)]
    cache_hits: u64,
    #[pyo3(get)]
    cache_misses: u64,
    #[pyo3(get)]
    total_compilations: u64,
    #[pyo3(get)]
    non_cacheable: u64,
    #[pyo3(get)]
    compile_errors: u64,
    #[pyo3(get)]
    time_saved_ms: u64,
    #[pyo3(get)]
    total_links: u64,
    #[pyo3(get)]
    link_hits: u64,
    #[pyo3(get)]
    link_misses: u64,
    #[pyo3(get)]
    link_non_cacheable: u64,
    #[pyo3(get)]
    dep_graph_contexts: u64,
    #[pyo3(get)]
    dep_graph_files: u64,
    #[pyo3(get)]
    sessions_total: u64,
    #[pyo3(get)]
    sessions_active: u64,
    #[pyo3(get)]
    cache_dir: String,
    #[pyo3(get)]
    dep_graph_version: u32,
    #[pyo3(get)]
    dep_graph_disk_size: u64,
    #[pyo3(get)]
    dep_graph_persisted: bool,
}

impl From<zccache::protocol::DaemonStatus> for NativeDaemonStatus {
    fn from(value: zccache::protocol::DaemonStatus) -> Self {
        Self {
            version: value.version,
            artifact_count: value.artifact_count,
            cache_size_bytes: value.cache_size_bytes,
            metadata_entries: value.metadata_entries,
            uptime_secs: value.uptime_secs,
            cache_hits: value.cache_hits,
            cache_misses: value.cache_misses,
            total_compilations: value.total_compilations,
            non_cacheable: value.non_cacheable,
            compile_errors: value.compile_errors,
            time_saved_ms: value.time_saved_ms,
            total_links: value.total_links,
            link_hits: value.link_hits,
            link_misses: value.link_misses,
            link_non_cacheable: value.link_non_cacheable,
            dep_graph_contexts: value.dep_graph_contexts,
            dep_graph_files: value.dep_graph_files,
            sessions_total: value.sessions_total,
            sessions_active: value.sessions_active,
            cache_dir: value.cache_dir.display().to_string(),
            dep_graph_version: value.dep_graph_version,
            dep_graph_disk_size: value.dep_graph_disk_size,
            dep_graph_persisted: value.dep_graph_persisted,
        }
    }
}

#[pyclass(module = "zccache._native")]
#[derive(Clone)]
pub struct NativeSessionStart {
    #[pyo3(get)]
    session_id: String,
    #[pyo3(get)]
    journal_path: Option<String>,
}

#[pyclass(module = "zccache._native")]
#[derive(Clone)]
pub struct NativeSessionStats {
    #[pyo3(get)]
    duration_ms: u64,
    #[pyo3(get)]
    compilations: u64,
    #[pyo3(get)]
    hits: u64,
    #[pyo3(get)]
    misses: u64,
    #[pyo3(get)]
    non_cacheable: u64,
    #[pyo3(get)]
    errors: u64,
    #[pyo3(get)]
    time_saved_ms: u64,
    #[pyo3(get)]
    unique_sources: u64,
    #[pyo3(get)]
    bytes_read: u64,
    #[pyo3(get)]
    bytes_written: u64,
}

impl From<zccache::protocol::SessionStats> for NativeSessionStats {
    fn from(value: zccache::protocol::SessionStats) -> Self {
        Self {
            duration_ms: value.duration_ms,
            compilations: value.compilations,
            hits: value.hits,
            misses: value.misses,
            non_cacheable: value.non_cacheable,
            errors: value.errors,
            time_saved_ms: value.time_saved_ms,
            unique_sources: value.unique_sources,
            bytes_read: value.bytes_read,
            bytes_written: value.bytes_written,
        }
    }
}

#[pyclass(module = "zccache._native")]
#[derive(Clone)]
pub struct NativeFingerprintCheck {
    #[pyo3(get)]
    decision: String,
    #[pyo3(get)]
    reason: Option<String>,
    #[pyo3(get)]
    changed_files: Vec<String>,
}

#[pyclass(module = "zccache._native")]
#[derive(Clone)]
pub struct NativeInoConvertResult {
    #[pyo3(get)]
    cache_hit: bool,
    #[pyo3(get)]
    skipped_write: bool,
}

#[pyclass(module = "zccache._native")]
#[derive(Clone)]
pub struct NativeDownloadStatus {
    #[pyo3(get)]
    phase: String,
    #[pyo3(get)]
    total_bytes: Option<u64>,
    #[pyo3(get)]
    downloaded_bytes: u64,
    #[pyo3(get)]
    percentage: Option<f32>,
    #[pyo3(get)]
    active_clients: u32,
    #[pyo3(get)]
    destination: String,
    #[pyo3(get)]
    source_url: String,
    #[pyo3(get)]
    error: Option<String>,
}

impl From<zccache::download::DownloadStatus> for NativeDownloadStatus {
    fn from(value: zccache::download::DownloadStatus) -> Self {
        Self {
            phase: format!("{:?}", value.phase).to_lowercase(),
            total_bytes: value.total_bytes,
            downloaded_bytes: value.downloaded_bytes,
            percentage: value.percentage,
            active_clients: value.active_clients,
            destination: value.destination.display().to_string(),
            source_url: value.source_url,
            error: value.error,
        }
    }
}

#[pyclass(module = "zccache._native")]
#[derive(Clone)]
pub struct NativeDownloadDaemonStatus {
    #[pyo3(get)]
    version: String,
    #[pyo3(get)]
    active_downloads: u64,
    #[pyo3(get)]
    connected_clients: u64,
    #[pyo3(get)]
    uptime_secs: u64,
    #[pyo3(get)]
    endpoint: String,
}

impl From<zccache::download::DownloadDaemonStatus> for NativeDownloadDaemonStatus {
    fn from(value: zccache::download::DownloadDaemonStatus) -> Self {
        Self {
            version: value.version,
            active_downloads: value.active_downloads,
            connected_clients: value.connected_clients,
            uptime_secs: value.uptime_secs,
            endpoint: value.endpoint,
        }
    }
}

fn parse_archive_format(value: &str) -> zccache::download_client::ArchiveFormat {
    match value.to_ascii_lowercase().as_str() {
        "none" => zccache::download_client::ArchiveFormat::None,
        "zst" => zccache::download_client::ArchiveFormat::Zst,
        "zip" => zccache::download_client::ArchiveFormat::Zip,
        "xz" => zccache::download_client::ArchiveFormat::Xz,
        "tar.gz" | "targz" => zccache::download_client::ArchiveFormat::TarGz,
        "tar.xz" | "tarxz" => zccache::download_client::ArchiveFormat::TarXz,
        "tar.zst" | "tarzst" => zccache::download_client::ArchiveFormat::TarZst,
        "7z" | "sevenz" => zccache::download_client::ArchiveFormat::SevenZip,
        _ => zccache::download_client::ArchiveFormat::Auto,
    }
}

#[pyclass(module = "zccache._native")]
#[derive(Clone)]
pub struct NativeFetchResult {
    #[pyo3(get)]
    status: String,
    #[pyo3(get)]
    cache_path: String,
    #[pyo3(get)]
    expanded_path: Option<String>,
    #[pyo3(get)]
    bytes: Option<u64>,
    #[pyo3(get)]
    sha256: String,
}

impl From<zccache::download_client::FetchResult> for NativeFetchResult {
    fn from(value: zccache::download_client::FetchResult) -> Self {
        Self {
            status: format!("{:?}", value.status).to_lowercase(),
            cache_path: value.cache_path.display().to_string(),
            expanded_path: value.expanded_path.map(|path| path.display().to_string()),
            bytes: value.bytes,
            sha256: value.sha256,
        }
    }
}

#[pyclass(module = "zccache._native")]
#[derive(Clone)]
pub struct NativeFetchState {
    #[pyo3(get)]
    kind: String,
    #[pyo3(get)]
    cache_path: String,
    #[pyo3(get)]
    expanded_path: Option<String>,
    #[pyo3(get)]
    bytes: Option<u64>,
    #[pyo3(get)]
    sha256: Option<String>,
    #[pyo3(get)]
    reason: Option<String>,
}

impl From<zccache::download_client::FetchState> for NativeFetchState {
    fn from(value: zccache::download_client::FetchState) -> Self {
        Self {
            kind: format!("{:?}", value.kind).to_lowercase(),
            cache_path: value.cache_path.display().to_string(),
            expanded_path: value.expanded_path.map(|path| path.display().to_string()),
            bytes: value.bytes,
            sha256: value.sha256,
            reason: value.reason,
        }
    }
}

#[pyclass(module = "zccache._native")]
pub struct NativeDownloadHandle {
    handle: Option<zccache::download_client::DownloadHandle>,
    initiator: bool,
    download_id: String,
}

#[pymethods]
impl NativeDownloadHandle {
    #[getter]
    fn initiator(&self) -> bool {
        self.initiator
    }

    #[getter]
    fn download_id(&self) -> String {
        self.download_id.clone()
    }

    fn status(&mut self) -> PyResult<NativeDownloadStatus> {
        let handle = self
            .handle
            .as_mut()
            .ok_or_else(|| runtime_to_py_err("download handle is closed".to_string()))?;
        handle
            .status()
            .map(NativeDownloadStatus::from)
            .map_err(runtime_to_py_err)
    }

    #[pyo3(signature = (timeout_ms=None))]
    fn wait(&mut self, timeout_ms: Option<u64>) -> PyResult<NativeDownloadStatus> {
        let handle = self
            .handle
            .as_mut()
            .ok_or_else(|| runtime_to_py_err("download handle is closed".to_string()))?;
        handle
            .wait(timeout_ms)
            .map(NativeDownloadStatus::from)
            .map_err(runtime_to_py_err)
    }

    fn cancel(&mut self) -> PyResult<NativeDownloadStatus> {
        let handle = self
            .handle
            .as_mut()
            .ok_or_else(|| runtime_to_py_err("download handle is closed".to_string()))?;
        handle
            .cancel()
            .map(NativeDownloadStatus::from)
            .map_err(runtime_to_py_err)
    }

    fn close(&mut self) -> PyResult<()> {
        if let Some(handle) = self.handle.take() {
            handle.close().map_err(runtime_to_py_err)?;
        }
        Ok(())
    }
}

#[pyclass(module = "zccache._native")]
pub struct NativeDownloadApi {
    client: zccache::download_client::DownloadClient,
}

#[pymethods]
impl NativeDownloadApi {
    #[new]
    #[pyo3(signature = (endpoint=None))]
    fn new(endpoint: Option<String>) -> Self {
        let client = zccache::download_client::DownloadClient::new(endpoint.clone());
        Self { client }
    }

    fn start(&self) -> PyResult<()> {
        self.client.start_daemon().map_err(runtime_to_py_err)
    }

    fn stop(&self) -> PyResult<bool> {
        self.client.stop_daemon().map_err(runtime_to_py_err)
    }

    fn daemon_status(&self) -> PyResult<NativeDownloadDaemonStatus> {
        self.client
            .daemon_status()
            .map(NativeDownloadDaemonStatus::from)
            .map_err(runtime_to_py_err)
    }

    #[pyo3(signature = (
        source_url,
        destination,
        force=false,
        max_connections=None,
        min_segment_size=None
    ))]
    fn download(
        &self,
        source_url: String,
        destination: String,
        force: bool,
        max_connections: Option<usize>,
        min_segment_size: Option<u64>,
    ) -> PyResult<NativeDownloadHandle> {
        let options = zccache::download::DownloadOptions {
            force,
            max_connections,
            min_segment_size,
        };
        let handle = self
            .client
            .download(&source_url, PathBuf::from(destination).as_path(), options)
            .map_err(runtime_to_py_err)?;
        let initiator = handle.initiator();
        let download_id = handle.download_id().to_string();
        Ok(NativeDownloadHandle {
            handle: Some(handle),
            initiator,
            download_id,
        })
    }

    #[pyo3(signature = (
        source,
        destination=None,
        expanded=None,
        expected_sha256=None,
        archive_format="auto".to_string(),
        max_connections=None,
        min_segment_size=None,
        blocking=true,
        dry_run=false,
        force=false
    ))]
    fn fetch(
        &self,
        source: &Bound<'_, PyAny>,
        destination: Option<String>,
        expanded: Option<String>,
        expected_sha256: Option<String>,
        archive_format: String,
        max_connections: Option<usize>,
        min_segment_size: Option<u64>,
        blocking: bool,
        dry_run: bool,
        force: bool,
    ) -> PyResult<NativeFetchResult> {
        let source = parse_download_source(source)?;
        let request = build_download_request(DownloadParams {
            source,
            archive_path: destination.map(PathBuf::from),
            unarchive_path: expanded.map(PathBuf::from),
            expected_sha256,
            archive_format: parse_archive_format(&archive_format),
            max_connections,
            min_segment_size,
            wait_mode: if blocking {
                WaitMode::Block
            } else {
                WaitMode::NoWait
            },
            dry_run,
            force,
        });
        self.client
            .fetch(request)
            .map(NativeFetchResult::from)
            .map_err(runtime_to_py_err)
    }

    #[pyo3(signature = (
        source,
        destination=None,
        expanded=None,
        expected_sha256=None,
        archive_format="auto".to_string()
    ))]
    fn exists(
        &self,
        source: &Bound<'_, PyAny>,
        destination: Option<String>,
        expanded: Option<String>,
        expected_sha256: Option<String>,
        archive_format: String,
    ) -> PyResult<NativeFetchState> {
        let source = parse_download_source(source)?;
        let request = build_download_request(DownloadParams {
            source,
            archive_path: destination.map(PathBuf::from),
            unarchive_path: expanded.map(PathBuf::from),
            expected_sha256,
            archive_format: parse_archive_format(&archive_format),
            max_connections: None,
            min_segment_size: None,
            wait_mode: WaitMode::Block,
            dry_run: false,
            force: false,
        });
        self.client
            .exists(&request)
            .map(NativeFetchState::from)
            .map_err(runtime_to_py_err)
    }
}

#[pyclass(module = "zccache._native")]
pub struct NativeClient {
    endpoint: Option<String>,
}

#[pymethods]
impl NativeClient {
    #[new]
    #[pyo3(signature = (endpoint=None))]
    fn new(endpoint: Option<String>) -> Self {
        Self { endpoint }
    }

    fn start(&self) -> PyResult<()> {
        client_start(self.endpoint.as_deref()).map_err(runtime_to_py_err)
    }

    fn stop(&self) -> PyResult<bool> {
        client_stop(self.endpoint.as_deref()).map_err(runtime_to_py_err)
    }

    fn status(&self) -> PyResult<NativeDaemonStatus> {
        client_status(self.endpoint.as_deref())
            .map(NativeDaemonStatus::from)
            .map_err(runtime_to_py_err)
    }

    #[pyo3(signature = (
        source,
        destination=None,
        expanded=None,
        expected_sha256=None,
        max_connections=None,
        min_segment_size=None,
        blocking=true,
        dry_run=false,
        force=false
    ))]
    fn download(
        &self,
        source: &Bound<'_, PyAny>,
        destination: Option<String>,
        expanded: Option<String>,
        expected_sha256: Option<String>,
        max_connections: Option<usize>,
        min_segment_size: Option<u64>,
        blocking: bool,
        dry_run: bool,
        force: bool,
    ) -> PyResult<NativeFetchResult> {
        let source = parse_download_source(source)?;
        client_download(
            self.endpoint.as_deref(),
            DownloadParams {
                source,
                archive_path: destination.map(PathBuf::from),
                unarchive_path: expanded.map(PathBuf::from),
                expected_sha256,
                archive_format: zccache::download_client::ArchiveFormat::Auto,
                max_connections,
                min_segment_size,
                wait_mode: if blocking {
                    WaitMode::Block
                } else {
                    WaitMode::NoWait
                },
                dry_run,
                force,
            },
        )
        .map(NativeFetchResult::from)
        .map_err(runtime_to_py_err)
    }

    #[pyo3(signature = (
        source,
        destination=None,
        expanded=None,
        expected_sha256=None
    ))]
    fn download_exists(
        &self,
        source: &Bound<'_, PyAny>,
        destination: Option<String>,
        expanded: Option<String>,
        expected_sha256: Option<String>,
    ) -> PyResult<NativeFetchState> {
        let source = parse_download_source(source)?;
        client_download_exists(
            self.endpoint.as_deref(),
            DownloadParams {
                source,
                archive_path: destination.map(PathBuf::from),
                unarchive_path: expanded.map(PathBuf::from),
                expected_sha256,
                archive_format: zccache::download_client::ArchiveFormat::Auto,
                max_connections: None,
                min_segment_size: None,
                wait_mode: WaitMode::Block,
                dry_run: false,
                force: false,
            },
        )
        .map(NativeFetchState::from)
        .map_err(runtime_to_py_err)
    }

    #[pyo3(signature = (cwd, log_file=None, track_stats=false, journal_path=None))]
    fn session_start(
        &self,
        cwd: String,
        log_file: Option<String>,
        track_stats: bool,
        journal_path: Option<String>,
    ) -> PyResult<NativeSessionStart> {
        let cwd = PathBuf::from(cwd);
        let log_file = log_file.map(PathBuf::from);
        let journal_path = journal_path.map(PathBuf::from);
        client_session_start(
            self.endpoint.as_deref(),
            cwd.as_path(),
            log_file.as_deref(),
            track_stats,
            journal_path.as_deref(),
        )
        .map(|result| NativeSessionStart {
            session_id: result.session_id,
            journal_path: result.journal_path,
        })
        .map_err(runtime_to_py_err)
    }

    fn session_end(&self, session_id: String) -> PyResult<Option<NativeSessionStats>> {
        client_session_end(self.endpoint.as_deref(), &session_id)
            .map(|stats| stats.map(NativeSessionStats::from))
            .map_err(runtime_to_py_err)
    }

    fn session_stats(&self, session_id: String) -> PyResult<Option<NativeSessionStats>> {
        client_session_stats(self.endpoint.as_deref(), &session_id)
            .map(|stats| stats.map(NativeSessionStats::from))
            .map_err(runtime_to_py_err)
    }

    #[pyo3(signature = (
        cache_file,
        cache_type="two-layer".to_string(),
        root=".".to_string(),
        extensions=vec![],
        include_globs=vec![],
        exclude=vec![]
    ))]
    fn fingerprint_check(
        &self,
        cache_file: String,
        cache_type: String,
        root: String,
        extensions: Vec<String>,
        include_globs: Vec<String>,
        exclude: Vec<String>,
    ) -> PyResult<NativeFingerprintCheck> {
        fingerprint_check(
            self.endpoint.as_deref(),
            PathBuf::from(cache_file).as_path(),
            &cache_type,
            PathBuf::from(root).as_path(),
            &extensions,
            &include_globs,
            &exclude,
        )
        .map(|result| NativeFingerprintCheck {
            decision: result.decision,
            reason: result.reason,
            changed_files: result.changed_files,
        })
        .map_err(runtime_to_py_err)
    }

    fn fingerprint_mark_success(&self, cache_file: String) -> PyResult<()> {
        fingerprint_mark_success(
            self.endpoint.as_deref(),
            PathBuf::from(cache_file).as_path(),
        )
        .map_err(runtime_to_py_err)
    }

    fn fingerprint_mark_failure(&self, cache_file: String) -> PyResult<()> {
        fingerprint_mark_failure(
            self.endpoint.as_deref(),
            PathBuf::from(cache_file).as_path(),
        )
        .map_err(runtime_to_py_err)
    }

    fn fingerprint_invalidate(&self, cache_file: String) -> PyResult<()> {
        fingerprint_invalidate(
            self.endpoint.as_deref(),
            PathBuf::from(cache_file).as_path(),
        )
        .map_err(runtime_to_py_err)
    }
}

#[pyfunction]
#[pyo3(signature = (
    input,
    output,
    clang_args=vec![],
    inject_arduino_include=true
))]
fn convert_ino(
    input: String,
    output: String,
    clang_args: Vec<String>,
    inject_arduino_include: bool,
) -> PyResult<NativeInoConvertResult> {
    run_ino_convert_cached(
        PathBuf::from(input).as_path(),
        PathBuf::from(output).as_path(),
        &InoConvertOptions {
            clang_args,
            inject_arduino_include,
        },
    )
    .map(|result| NativeInoConvertResult {
        cache_hit: result.cache_hit,
        skipped_write: result.skipped_write,
    })
    .map_err(|e| PyErr::new::<PyOSError, _>(e.to_string()))
}

#[pyfunction]
fn default_endpoint() -> String {
    zccache::cli::resolve_endpoint(None)
}

#[pyfunction]
fn check_running_daemon() -> Option<u32> {
    zccache::ipc::check_running_daemon()
}

#[pymodule]
fn _native(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<NativeClient>()?;
    m.add_class::<NativeDaemonStatus>()?;
    m.add_class::<NativeSessionStart>()?;
    m.add_class::<NativeSessionStats>()?;
    m.add_class::<NativeFingerprintCheck>()?;
    m.add_class::<NativeInoConvertResult>()?;
    m.add_class::<NativeDownloadStatus>()?;
    m.add_class::<NativeDownloadDaemonStatus>()?;
    m.add_class::<NativeFetchResult>()?;
    m.add_class::<NativeFetchState>()?;
    m.add_class::<NativeDownloadHandle>()?;
    m.add_class::<NativeDownloadApi>()?;
    m.add_function(wrap_pyfunction!(convert_ino, m)?)?;
    m.add_function(wrap_pyfunction!(default_endpoint, m)?)?;
    m.add_function(wrap_pyfunction!(check_running_daemon, m)?)?;
    Ok(())
}
