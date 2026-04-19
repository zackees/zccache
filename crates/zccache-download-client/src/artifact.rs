use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};

use reqwest::header::ACCEPT_ENCODING;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;
use zccache_download::{canonical_destination, stable_download_id, DownloadOptions, DownloadPhase};

use crate::DownloadClient;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum WaitMode {
    Block,
    NoWait,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ArchiveFormat {
    Auto,
    None,
    Zst,
    Zip,
    Xz,
    TarGz,
    TarXz,
    TarZst,
    SevenZip,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FetchStatus {
    Downloaded,
    AlreadyPresent,
    Expanded,
    AlreadyExpanded,
    Ready,
    Locked,
    DryRun,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FetchStateKind {
    Missing,
    ArtifactReady,
    ExpandedReady,
    Invalid,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum DownloadSource {
    Url(String),
    MultipartUrls(Vec<String>),
}

impl DownloadSource {
    #[must_use]
    pub fn primary_url(&self) -> &str {
        match self {
            Self::Url(url) => url,
            Self::MultipartUrls(urls) => urls.first().map(String::as_str).unwrap_or(""),
        }
    }
}

impl From<String> for DownloadSource {
    fn from(value: String) -> Self {
        Self::Url(value)
    }
}

impl From<&str> for DownloadSource {
    fn from(value: &str) -> Self {
        Self::Url(value.to_string())
    }
}

impl From<Vec<String>> for DownloadSource {
    fn from(value: Vec<String>) -> Self {
        Self::MultipartUrls(value)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FetchRequest {
    pub source: DownloadSource,
    pub destination_path: PathBuf,
    pub destination_path_expanded: Option<PathBuf>,
    pub expected_sha256: Option<String>,
    pub archive_format: ArchiveFormat,
    pub wait_mode: WaitMode,
    pub dry_run: bool,
    pub force: bool,
    pub download_options: DownloadOptions,
}

impl FetchRequest {
    #[must_use]
    pub fn new(source: impl Into<DownloadSource>, destination_path: impl Into<PathBuf>) -> Self {
        Self {
            source: source.into(),
            destination_path: destination_path.into(),
            destination_path_expanded: None,
            expected_sha256: None,
            archive_format: ArchiveFormat::Auto,
            wait_mode: WaitMode::Block,
            dry_run: false,
            force: false,
            download_options: DownloadOptions::default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FetchResult {
    pub status: FetchStatus,
    pub cache_path: PathBuf,
    pub expanded_path: Option<PathBuf>,
    pub bytes: Option<u64>,
    pub sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FetchState {
    pub kind: FetchStateKind,
    pub cache_path: PathBuf,
    pub expanded_path: Option<PathBuf>,
    pub bytes: Option<u64>,
    pub sha256: Option<String>,
    pub reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ExpandedMarker {
    source: DownloadSource,
    cache_path: String,
    artifact_sha256: String,
    archive_format: ArchiveFormat,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ArtifactMarker {
    source: DownloadSource,
    cache_path: String,
    sha256: String,
    bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ArtifactFingerprint {
    sha256: String,
    bytes: u64,
}

#[derive(Debug, Clone)]
struct ResolvedFetchRequest {
    source: DownloadSource,
    cache_path: PathBuf,
    expanded_path: Option<PathBuf>,
    expected_sha256: Option<String>,
    archive_format: ArchiveFormat,
    wait_mode: WaitMode,
    dry_run: bool,
    force: bool,
    download_options: DownloadOptions,
}

impl DownloadClient {
    pub fn fetch(&self, request: FetchRequest) -> Result<FetchResult, String> {
        let resolved = resolve_request(&request)?;
        let initial = exists_resolved(&resolved)?;
        if resolved.force && initial.kind != FetchStateKind::Missing {
            return Err(format!(
                "artifact state already exists at {}; purge it before forcing replacement",
                resolved.cache_path.display()
            ));
        }
        if !resolved.force {
            match initial.kind {
                FetchStateKind::ExpandedReady => {
                    return Ok(FetchResult {
                        status: FetchStatus::AlreadyExpanded,
                        cache_path: resolved.cache_path,
                        expanded_path: resolved.expanded_path,
                        bytes: initial.bytes,
                        sha256: initial
                            .sha256
                            .ok_or_else(|| "missing artifact sha256 fingerprint".to_string())?,
                    });
                }
                FetchStateKind::ArtifactReady if resolved.expanded_path.is_none() => {
                    return Ok(FetchResult {
                        status: FetchStatus::AlreadyPresent,
                        cache_path: resolved.cache_path,
                        expanded_path: None,
                        bytes: initial.bytes,
                        sha256: initial
                            .sha256
                            .ok_or_else(|| "missing artifact sha256 fingerprint".to_string())?,
                    });
                }
                _ => {}
            }
        }

        if resolved.dry_run {
            return Ok(FetchResult {
                status: FetchStatus::DryRun,
                cache_path: resolved.cache_path,
                expanded_path: resolved.expanded_path,
                bytes: initial.bytes,
                sha256: initial.sha256.unwrap_or_default(),
            });
        }

        let _lock = match acquire_fetch_lock(&resolved) {
            Ok(lock) => lock,
            Err(message) if message == "locked" => {
                return Ok(FetchResult {
                    status: FetchStatus::Locked,
                    cache_path: resolved.cache_path,
                    expanded_path: resolved.expanded_path,
                    bytes: initial.bytes,
                    sha256: initial.sha256.unwrap_or_default(),
                });
            }
            Err(message) => return Err(message),
        };
        let current = exists_resolved(&resolved)?;
        if current.kind == FetchStateKind::Invalid {
            return Err(format!(
                "{}; purge the artifact state before retrying",
                current
                    .reason
                    .clone()
                    .unwrap_or_else(|| "artifact exists but failed validation".to_string())
            ));
        }
        if !resolved.force {
            match current.kind {
                FetchStateKind::ExpandedReady => {
                    return Ok(FetchResult {
                        status: FetchStatus::AlreadyExpanded,
                        cache_path: resolved.cache_path,
                        expanded_path: resolved.expanded_path,
                        bytes: current.bytes,
                        sha256: current
                            .sha256
                            .ok_or_else(|| "missing artifact sha256 fingerprint".to_string())?,
                    });
                }
                FetchStateKind::ArtifactReady if resolved.expanded_path.is_none() => {
                    return Ok(FetchResult {
                        status: FetchStatus::AlreadyPresent,
                        cache_path: resolved.cache_path,
                        expanded_path: None,
                        bytes: current.bytes,
                        sha256: current
                            .sha256
                            .ok_or_else(|| "missing artifact sha256 fingerprint".to_string())?,
                    });
                }
                _ => {}
            }
        }

        let mut downloaded_now = false;
        if resolved.force || current.kind != FetchStateKind::ArtifactReady {
            match &resolved.source {
                DownloadSource::Url(url) => {
                    let mut handle = self.download(
                        url,
                        &resolved.cache_path,
                        resolved.download_options.clone(),
                    )?;
                    let status = loop {
                        let status = handle.wait(None)?;
                        if crate::is_terminal(&status) {
                            break status;
                        }
                    };
                    if status.phase != DownloadPhase::Completed {
                        return Err(status.error.unwrap_or_else(|| {
                            format!("download finished in unexpected phase {:?}", status.phase)
                        }));
                    }
                    handle.close()?;
                }
                DownloadSource::MultipartUrls(urls) => {
                    download_explicit_parts(urls, &resolved.cache_path)?;
                }
            }
            downloaded_now = true;
        }

        let fingerprint = match validate_artifact(&resolved) {
            Ok(fingerprint) => fingerprint,
            Err(err) => {
                cleanup_invalid_fetch_state(&resolved);
                return Err(err);
            }
        };
        write_artifact_marker(&resolved, &fingerprint)?;

        if let Some(expanded_path) = &resolved.expanded_path {
            let expanded_ready = expanded_marker_matches(&resolved, &fingerprint)?;
            if !resolved.force && expanded_ready {
                return Ok(FetchResult {
                    status: if downloaded_now {
                        FetchStatus::Ready
                    } else {
                        FetchStatus::AlreadyExpanded
                    },
                    cache_path: resolved.cache_path.clone(),
                    expanded_path: Some(expanded_path.clone()),
                    bytes: Some(fingerprint.bytes),
                    sha256: fingerprint.sha256.clone(),
                });
            }

            if expanded_path.exists() {
                return Err(format!(
                    "expanded destination {} already exists but is not validated; purge it before retrying",
                    expanded_path.display()
                ));
            }

            remove_path_if_exists(&expanded_marker_path(expanded_path))?;
            extract_archive(&resolved, expanded_path)?;
            write_expanded_marker(&resolved, &fingerprint)?;
            return Ok(FetchResult {
                status: FetchStatus::Expanded,
                cache_path: resolved.cache_path.clone(),
                expanded_path: Some(expanded_path.clone()),
                bytes: Some(fingerprint.bytes),
                sha256: fingerprint.sha256,
            });
        }

        Ok(FetchResult {
            status: if downloaded_now {
                FetchStatus::Downloaded
            } else {
                FetchStatus::AlreadyPresent
            },
            cache_path: resolved.cache_path.clone(),
            expanded_path: None,
            bytes: Some(fingerprint.bytes),
            sha256: fingerprint.sha256,
        })
    }

    pub fn exists(&self, request: &FetchRequest) -> Result<FetchState, String> {
        let resolved = resolve_request_no_create(request)?;
        exists_resolved(&resolved)
    }
}

fn resolve_request(request: &FetchRequest) -> Result<ResolvedFetchRequest, String> {
    Ok(ResolvedFetchRequest {
        source: normalize_source(request.source.clone())?,
        cache_path: canonical_destination(&request.destination_path)
            .map_err(|e| e.to_string())?
            .into_path_buf(),
        expanded_path: request
            .destination_path_expanded
            .as_ref()
            .map(|p| normalize_target(p, true))
            .transpose()?,
        expected_sha256: request.expected_sha256.clone().map(normalize_sha256),
        archive_format: request.archive_format,
        wait_mode: request.wait_mode,
        dry_run: request.dry_run,
        force: request.force,
        download_options: request.download_options.clone(),
    })
}

fn resolve_request_no_create(request: &FetchRequest) -> Result<ResolvedFetchRequest, String> {
    Ok(ResolvedFetchRequest {
        source: normalize_source(request.source.clone())?,
        cache_path: normalize_target(&request.destination_path, false)?,
        expanded_path: request
            .destination_path_expanded
            .as_ref()
            .map(|p| normalize_target(p, false))
            .transpose()?,
        expected_sha256: request.expected_sha256.clone().map(normalize_sha256),
        archive_format: request.archive_format,
        wait_mode: request.wait_mode,
        dry_run: request.dry_run,
        force: request.force,
        download_options: request.download_options.clone(),
    })
}

fn normalize_target(path: &Path, create_parent: bool) -> Result<PathBuf, String> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|e| e.to_string())?
            .join(path)
    };
    let file_name = absolute
        .file_name()
        .map(ToOwned::to_owned)
        .ok_or_else(|| "path must include a terminal file or directory name".to_string())?;
    let parent = absolute.parent().unwrap_or_else(|| Path::new("."));
    let canonical_parent = if parent.exists() {
        std::fs::canonicalize(parent).map_err(|e| e.to_string())?
    } else if create_parent {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        std::fs::canonicalize(parent).map_err(|e| e.to_string())?
    } else {
        zccache_core::NormalizedPath::new(parent).into_path_buf()
    };
    Ok(canonical_parent.join(file_name))
}

fn normalize_sha256(value: String) -> String {
    value.trim().to_ascii_lowercase()
}

fn normalize_source(source: DownloadSource) -> Result<DownloadSource, String> {
    match source {
        DownloadSource::Url(url) => {
            if url.trim().is_empty() {
                Err("download source URL must not be empty".to_string())
            } else {
                Ok(DownloadSource::Url(url))
            }
        }
        DownloadSource::MultipartUrls(urls) => {
            if urls.is_empty() {
                return Err("multipart download source must include at least one URL".to_string());
            }
            if urls.iter().any(|url| url.trim().is_empty()) {
                return Err("multipart download source contains an empty URL".to_string());
            }
            Ok(DownloadSource::MultipartUrls(urls))
        }
    }
}

fn exists_resolved(request: &ResolvedFetchRequest) -> Result<FetchState, String> {
    let cache_exists = request.cache_path.exists();
    let fingerprint = if cache_exists {
        Some(read_or_compute_artifact_fingerprint(request)?)
    } else {
        None
    };
    let cache_valid = fingerprint
        .as_ref()
        .map(|fingerprint| artifact_matches_request(request, fingerprint))
        .unwrap_or(false);
    let bytes = fingerprint.as_ref().map(|fingerprint| fingerprint.bytes);
    let sha256 = fingerprint
        .as_ref()
        .map(|fingerprint| fingerprint.sha256.clone());

    if let Some(expanded_path) = &request.expanded_path {
        if cache_valid
            && expanded_marker_matches(
                request,
                fingerprint
                    .as_ref()
                    .ok_or_else(|| "missing artifact fingerprint".to_string())?,
            )?
            && expanded_path.exists()
        {
            return Ok(FetchState {
                kind: FetchStateKind::ExpandedReady,
                cache_path: request.cache_path.clone(),
                expanded_path: Some(expanded_path.clone()),
                bytes,
                sha256,
                reason: None,
            });
        }

        if cache_valid {
            return Ok(FetchState {
                kind: FetchStateKind::ArtifactReady,
                cache_path: request.cache_path.clone(),
                expanded_path: Some(expanded_path.clone()),
                bytes,
                sha256,
                reason: Some("expanded destination not ready".to_string()),
            });
        }
    } else if cache_valid {
        return Ok(FetchState {
            kind: FetchStateKind::ArtifactReady,
            cache_path: request.cache_path.clone(),
            expanded_path: None,
            bytes,
            sha256,
            reason: None,
        });
    }

    if cache_exists {
        return Ok(FetchState {
            kind: FetchStateKind::Invalid,
            cache_path: request.cache_path.clone(),
            expanded_path: request.expanded_path.clone(),
            bytes,
            sha256,
            reason: Some("artifact exists but failed validation".to_string()),
        });
    }

    Ok(FetchState {
        kind: FetchStateKind::Missing,
        cache_path: request.cache_path.clone(),
        expanded_path: request.expanded_path.clone(),
        bytes: None,
        sha256: None,
        reason: None,
    })
}

fn artifact_matches_request(
    request: &ResolvedFetchRequest,
    fingerprint: &ArtifactFingerprint,
) -> bool {
    request
        .expected_sha256
        .as_ref()
        .map(|expected_sha256| fingerprint.sha256 == *expected_sha256)
        .unwrap_or(true)
}

fn validate_artifact(request: &ResolvedFetchRequest) -> Result<ArtifactFingerprint, String> {
    if !request.cache_path.exists() {
        return Err(format!(
            "downloaded artifact missing at {}",
            request.cache_path.display()
        ));
    }
    let fingerprint =
        compute_artifact_fingerprint(&request.cache_path).map_err(|e| e.to_string())?;
    if let Some(expected_sha256) = &request.expected_sha256 {
        if fingerprint.sha256 != *expected_sha256 {
            return Err(format!(
                "sha256 mismatch for {}: expected {}, got {}",
                request.cache_path.display(),
                expected_sha256,
                fingerprint.sha256
            ));
        }
    }
    Ok(fingerprint)
}

fn cleanup_invalid_fetch_state(request: &ResolvedFetchRequest) {
    let _ = remove_path_if_exists(&request.cache_path);
    let _ = remove_path_if_exists(&artifact_marker_path(&request.cache_path));
    if let Some(expanded_path) = &request.expanded_path {
        let _ = remove_path_if_exists(expanded_path);
        let _ = remove_path_if_exists(&expanded_marker_path(expanded_path));
    }
}

fn download_explicit_parts(part_urls: &[String], destination: &Path) -> Result<(), String> {
    let temp_path = temp_download_path(destination);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("failed to create tokio runtime: {e}"))?;
    runtime.block_on(async move {
        let client = reqwest::Client::builder()
            .user_agent(format!("zccache-download/{}", zccache_core::VERSION))
            .build()
            .map_err(|e| e.to_string())?;

        if let Some(parent) = destination.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| e.to_string())?;
        }

        let _ = tokio::fs::remove_file(&temp_path).await;

        let result = async {
            let mut output = tokio::fs::File::create(&temp_path)
                .await
                .map_err(|e| e.to_string())?;
            for url in part_urls {
                let mut response = client
                    .get(url)
                    .header(ACCEPT_ENCODING, "identity")
                    .send()
                    .await
                    .map_err(|e| e.to_string())?;
                if !response.status().is_success() {
                    return Err(format!("unexpected status {} for {url}", response.status()));
                }
                while let Some(chunk) = response.chunk().await.map_err(|e| e.to_string())? {
                    output.write_all(&chunk).await.map_err(|e| e.to_string())?;
                }
            }
            output.flush().await.map_err(|e| e.to_string())?;
            drop(output);
            if destination.exists() {
                let _ = tokio::fs::remove_file(destination).await;
            }
            tokio::fs::rename(&temp_path, destination)
                .await
                .map_err(|e| e.to_string())
        }
        .await;

        if result.is_err() {
            let _ = tokio::fs::remove_file(&temp_path).await;
        }
        result
    })
}

fn sha256_file(path: &Path) -> io::Result<String> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn compute_artifact_fingerprint(path: &Path) -> io::Result<ArtifactFingerprint> {
    let sha256 = sha256_file(path)?;
    let bytes = fs::metadata(path)?.len();
    Ok(ArtifactFingerprint { sha256, bytes })
}

fn temp_download_path(destination: &Path) -> PathBuf {
    destination.with_extension(format!(
        "{}part",
        destination
            .extension()
            .map(|ext| format!("{}.", ext.to_string_lossy()))
            .unwrap_or_default()
    ))
}

struct FetchLock {
    _file: File,
}

fn acquire_fetch_lock(request: &ResolvedFetchRequest) -> Result<FetchLock, String> {
    let lock_path = fetch_lock_path(request);
    if let Some(parent) = lock_path.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .map_err(|e| e.to_string())?;
    match request.wait_mode {
        WaitMode::Block => fs2::FileExt::lock_exclusive(&file).map_err(|e| e.to_string())?,
        WaitMode::NoWait => {
            if fs2::FileExt::try_lock_exclusive(&file).is_err() {
                return Err("locked".to_string());
            }
        }
    }
    Ok(FetchLock { _file: file })
}

fn fetch_lock_path(request: &ResolvedFetchRequest) -> PathBuf {
    let mut key = zccache_core::normalize_for_key(&request.cache_path);
    if let Some(expanded_path) = &request.expanded_path {
        key.push('\n');
        key.push_str(&zccache_core::normalize_for_key(expanded_path));
    }
    let hash = stable_download_id(Path::new(&key));
    zccache_core::config::default_cache_dir()
        .join("downloads")
        .join("locks")
        .join(format!("{hash}.lock"))
        .into_path_buf()
}

fn artifact_marker_path(cache_path: &Path) -> PathBuf {
    let hash = stable_download_id(cache_path);
    zccache_core::config::default_cache_dir()
        .join("downloads")
        .join("artifact-state")
        .join(format!("{hash}.json"))
        .into_path_buf()
}

fn expanded_marker_path(expanded_path: &Path) -> PathBuf {
    let hash = stable_download_id(expanded_path);
    zccache_core::config::default_cache_dir()
        .join("downloads")
        .join("expanded-state")
        .join(format!("{hash}.json"))
        .into_path_buf()
}

fn read_or_compute_artifact_fingerprint(
    request: &ResolvedFetchRequest,
) -> Result<ArtifactFingerprint, String> {
    let fingerprint =
        compute_artifact_fingerprint(&request.cache_path).map_err(|e| e.to_string())?;
    if let Ok(content) = fs::read_to_string(artifact_marker_path(&request.cache_path)) {
        let marker: ArtifactMarker = serde_json::from_str(&content).map_err(|e| e.to_string())?;
        if marker.source != request.source
            || marker.cache_path != request.cache_path.to_string_lossy()
            || marker.sha256 != fingerprint.sha256
            || marker.bytes != fingerprint.bytes
        {
            return Err(format!(
                "artifact marker for {} does not match the on-disk payload",
                request.cache_path.display()
            ));
        }
    }
    Ok(fingerprint)
}

fn write_artifact_marker(
    request: &ResolvedFetchRequest,
    fingerprint: &ArtifactFingerprint,
) -> Result<(), String> {
    let marker_path = artifact_marker_path(&request.cache_path);
    if let Some(parent) = marker_path.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let marker = ArtifactMarker {
        source: request.source.clone(),
        cache_path: request.cache_path.to_string_lossy().into_owned(),
        sha256: fingerprint.sha256.clone(),
        bytes: fingerprint.bytes,
    };
    let json = serde_json::to_string(&marker).map_err(|e| e.to_string())?;
    fs::write(marker_path, json).map_err(|e| e.to_string())
}

fn expanded_marker_matches(
    request: &ResolvedFetchRequest,
    fingerprint: &ArtifactFingerprint,
) -> Result<bool, String> {
    let Some(expanded_path) = &request.expanded_path else {
        return Ok(false);
    };
    let marker_path = expanded_marker_path(expanded_path);
    let marker: ExpandedMarker = match fs::read_to_string(&marker_path) {
        Ok(content) => serde_json::from_str(&content).map_err(|e| e.to_string())?,
        Err(_) => return Ok(false),
    };
    if marker.source != request.source {
        return Ok(false);
    }
    if marker.cache_path != request.cache_path.to_string_lossy() {
        return Ok(false);
    }
    if marker.artifact_sha256 != fingerprint.sha256 {
        return Ok(false);
    }
    if marker.archive_format != detect_archive_format(request)? {
        return Ok(false);
    }
    Ok(expanded_path.exists())
}

fn write_expanded_marker(
    request: &ResolvedFetchRequest,
    fingerprint: &ArtifactFingerprint,
) -> Result<(), String> {
    let Some(expanded_path) = &request.expanded_path else {
        return Ok(());
    };
    let marker_path = expanded_marker_path(expanded_path);
    if let Some(parent) = marker_path.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let marker = ExpandedMarker {
        source: request.source.clone(),
        cache_path: request.cache_path.to_string_lossy().into_owned(),
        artifact_sha256: fingerprint.sha256.clone(),
        archive_format: detect_archive_format(request)?,
    };
    let json = serde_json::to_string(&marker).map_err(|e| e.to_string())?;
    fs::write(marker_path, json).map_err(|e| e.to_string())
}

fn detect_archive_format(request: &ResolvedFetchRequest) -> Result<ArchiveFormat, String> {
    match request.archive_format {
        ArchiveFormat::Auto => auto_archive_format(&request.cache_path),
        other => Ok(other),
    }
}

fn auto_archive_format(path: &Path) -> Result<ArchiveFormat, String> {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    if name.ends_with(".tar.gz") {
        Ok(ArchiveFormat::TarGz)
    } else if name.ends_with(".tar.xz") {
        Ok(ArchiveFormat::TarXz)
    } else if name.ends_with(".tar.zst") || name.ends_with(".tzst") {
        Ok(ArchiveFormat::TarZst)
    } else if name.ends_with(".zip") {
        Ok(ArchiveFormat::Zip)
    } else if name.ends_with(".zst") {
        Ok(ArchiveFormat::Zst)
    } else if name.ends_with(".xz") {
        Ok(ArchiveFormat::Xz)
    } else if name.ends_with(".7z") {
        Ok(ArchiveFormat::SevenZip)
    } else {
        Ok(ArchiveFormat::None)
    }
}

fn extract_archive(request: &ResolvedFetchRequest, expanded_path: &Path) -> Result<(), String> {
    match detect_archive_format(request)? {
        ArchiveFormat::None => {
            copy_file(&request.cache_path, expanded_path).map_err(|e| e.to_string())
        }
        ArchiveFormat::Zst => {
            let input = File::open(&request.cache_path).map_err(|e| e.to_string())?;
            let mut decoder = ruzstd::StreamingDecoder::new(input).map_err(|e| e.to_string())?;
            write_decoded_to_file(&mut decoder, expanded_path).map_err(|e| e.to_string())
        }
        ArchiveFormat::Xz => {
            let input = File::open(&request.cache_path).map_err(|e| e.to_string())?;
            if let Some(parent) = expanded_path.parent() {
                fs::create_dir_all(parent).map_err(|e| e.to_string())?;
            }
            let mut output = File::create(expanded_path).map_err(|e| e.to_string())?;
            let mut input = io::BufReader::new(input);
            lzma_rs::xz_decompress(&mut input, &mut output).map_err(|e| e.to_string())
        }
        ArchiveFormat::Zip => extract_zip(&request.cache_path, expanded_path),
        ArchiveFormat::TarGz => {
            let input = File::open(&request.cache_path).map_err(|e| e.to_string())?;
            let decoder = flate2::read::GzDecoder::new(input);
            extract_tar(decoder, expanded_path)
        }
        ArchiveFormat::TarXz => {
            let input = File::open(&request.cache_path).map_err(|e| e.to_string())?;
            let mut decoded = Vec::new();
            let mut input = io::BufReader::new(input);
            lzma_rs::xz_decompress(&mut input, &mut decoded).map_err(|e| e.to_string())?;
            extract_tar(io::Cursor::new(decoded), expanded_path)
        }
        ArchiveFormat::TarZst => {
            let input = File::open(&request.cache_path).map_err(|e| e.to_string())?;
            let decoder = ruzstd::StreamingDecoder::new(input).map_err(|e| e.to_string())?;
            extract_tar(decoder, expanded_path)
        }
        ArchiveFormat::SevenZip => extract_7z(&request.cache_path, expanded_path),
        ArchiveFormat::Auto => Err("archive format auto-detection failed".to_string()),
    }
}

fn extract_7z(archive_path: &Path, destination: &Path) -> Result<(), String> {
    fs::create_dir_all(destination).map_err(|e| e.to_string())?;
    let base = destination.to_path_buf();
    sevenz_rust::decompress_file_with_extract_fn(
        archive_path,
        destination,
        move |entry, reader, _default_dest| {
            let relative = Path::new(entry.name());
            let out_path = safe_join(&base, relative).map_err(std::io::Error::other)?;
            if entry.is_directory() {
                fs::create_dir_all(&out_path)?;
                return Ok(true);
            }
            if let Some(parent) = out_path.parent() {
                fs::create_dir_all(parent)?;
            }
            let mut output = File::create(&out_path)?;
            io::copy(reader, &mut output)?;
            output.flush()?;
            Ok(true)
        },
    )
    .map_err(|e| e.to_string())
}

fn write_decoded_to_file(reader: &mut dyn Read, destination: &Path) -> io::Result<()> {
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut output = File::create(destination)?;
    io::copy(reader, &mut output)?;
    output.flush()?;
    Ok(())
}

fn copy_file(source: &Path, destination: &Path) -> io::Result<()> {
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::copy(source, destination)?;
    Ok(())
}

fn extract_zip(archive_path: &Path, destination: &Path) -> Result<(), String> {
    fs::create_dir_all(destination).map_err(|e| e.to_string())?;
    let file = File::open(archive_path).map_err(|e| e.to_string())?;
    let mut zip = zip::ZipArchive::new(file).map_err(|e| e.to_string())?;
    for i in 0..zip.len() {
        let mut entry = zip.by_index(i).map_err(|e| e.to_string())?;
        let name = entry
            .enclosed_name()
            .map(|p| p.to_path_buf())
            .ok_or_else(|| format!("unsafe zip entry: {}", entry.name()))?;
        let out_path = safe_join(destination, &name)?;
        if entry.is_dir() {
            fs::create_dir_all(&out_path).map_err(|e| e.to_string())?;
            continue;
        }
        if let Some(mode) = entry.unix_mode() {
            if (mode & 0o170000) == 0o120000 {
                return Err(format!(
                    "zip symlink entries are not allowed: {}",
                    entry.name()
                ));
            }
        }
        if let Some(parent) = out_path.parent() {
            fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let mut out = File::create(&out_path).map_err(|e| e.to_string())?;
        io::copy(&mut entry, &mut out).map_err(|e| e.to_string())?;
    }
    Ok(())
}

fn extract_tar<R: Read>(reader: R, destination: &Path) -> Result<(), String> {
    fs::create_dir_all(destination).map_err(|e| e.to_string())?;
    let mut archive = tar::Archive::new(reader);
    let entries = archive.entries().map_err(|e| e.to_string())?;
    for item in entries {
        let mut entry = item.map_err(|e| e.to_string())?;
        let path = entry.path().map_err(|e| e.to_string())?;
        let out_path = safe_join(destination, &path)?;
        let entry_type = entry.header().entry_type();
        if entry_type.is_symlink() || entry_type.is_hard_link() {
            return Err(format!(
                "tar link entries are not allowed: {}",
                path.display()
            ));
        }
        if entry_type.is_dir() {
            fs::create_dir_all(&out_path).map_err(|e| e.to_string())?;
            continue;
        }
        if let Some(parent) = out_path.parent() {
            fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let mut out = File::create(&out_path).map_err(|e| e.to_string())?;
        io::copy(&mut entry, &mut out).map_err(|e| e.to_string())?;
    }
    Ok(())
}

fn safe_join(base: &Path, entry: &Path) -> Result<PathBuf, String> {
    if entry.is_absolute() {
        return Err(format!(
            "absolute archive entry is not allowed: {}",
            entry.display()
        ));
    }
    let mut clean = PathBuf::new();
    for component in entry.components() {
        match component {
            Component::Normal(part) => clean.push(part),
            Component::CurDir => {}
            _ => return Err(format!("unsafe archive entry: {}", entry.display())),
        }
    }
    Ok(base.join(clean))
}

fn remove_path_if_exists(path: &Path) -> Result<(), String> {
    if !path.exists() {
        return Ok(());
    }
    if path.is_dir() {
        fs::remove_dir_all(path).map_err(|e| e.to_string())
    } else {
        fs::remove_file(path).map_err(|e| e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::net::{TcpListener, TcpStream};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::thread;
    use std::time::{Duration, Instant};

    use flate2::write::GzEncoder;
    use flate2::Compression;
    use zccache_download_daemon::DownloadDaemon;

    #[derive(Clone)]
    struct TestHttpConfig {
        body: Arc<Vec<u8>>,
        accept_ranges: bool,
        send_content_length: bool,
        chunk_size: usize,
        chunk_delay: Duration,
        path: String,
        request_started: Option<Arc<AtomicBool>>,
        release_response: Option<Arc<AtomicBool>>,
    }

    struct TestHttpServer {
        url: String,
        request_count: Arc<AtomicUsize>,
        range_request_count: Arc<AtomicUsize>,
        shutdown: Arc<AtomicBool>,
        thread: Option<thread::JoinHandle<()>>,
    }

    impl TestHttpServer {
        fn start(config: TestHttpConfig) -> Self {
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

        fn request_count(&self) -> usize {
            self.request_count.load(Ordering::Relaxed)
        }

        fn range_request_count(&self) -> usize {
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

    fn wait_for_test_condition(
        timeout: Duration,
        description: &str,
        mut predicate: impl FnMut() -> bool,
    ) {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if predicate() {
                return;
            }
            thread::sleep(Duration::from_millis(10));
        }
        panic!("timed out waiting for {description}");
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

    fn fetch_with_retry(http: &DownloadClient, req: FetchRequest) -> Result<FetchResult, String> {
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

    struct TestDaemon {
        endpoint: String,
        shutdown: Arc<tokio::sync::Notify>,
        thread: Option<thread::JoinHandle<()>>,
    }

    impl TestDaemon {
        fn start() -> Self {
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

    fn sha256_hex(data: &[u8]) -> String {
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

    #[test]
    fn fetch_cache_miss_then_hit_and_exists_stay_local() {
        let daemon = TestDaemon::start();
        let body = b"artifact payload".to_vec();
        let server = TestHttpServer::start(TestHttpConfig {
            body: Arc::new(body.clone()),
            accept_ranges: false,
            send_content_length: true,
            chunk_size: 0,
            chunk_delay: Duration::ZERO,
            path: "artifact.bin".to_string(),
            request_started: None,
            release_response: None,
        });
        let client = DownloadClient::new(Some(daemon.endpoint.clone()));
        let dir = tempfile::tempdir().unwrap();
        let mut request = FetchRequest::new(server.url.clone(), dir.path().join("artifact.bin"));
        request.expected_sha256 = Some(sha256_hex(&body));

        let first = fetch_with_retry(&client, request.clone()).unwrap();
        assert_eq!(first.status, FetchStatus::Downloaded);
        assert_eq!(first.sha256, sha256_hex(&body));
        let requests_after_first = server.request_count();
        assert!(requests_after_first > 0);

        let second = fetch_with_retry(&client, request.clone()).unwrap();
        assert_eq!(second.status, FetchStatus::AlreadyPresent);
        assert_eq!(server.request_count(), requests_after_first);

        let state = client.exists(&request).unwrap();
        assert_eq!(state.kind, FetchStateKind::ArtifactReady);
        assert_eq!(state.sha256.as_deref(), Some(first.sha256.as_str()));
        assert_eq!(server.request_count(), requests_after_first);
    }

    #[test]
    fn fetch_checksum_mismatch_cleans_up_invalid_artifact() {
        let daemon = TestDaemon::start();
        let body = b"wrong checksum body".to_vec();
        let server = TestHttpServer::start(TestHttpConfig {
            body: Arc::new(body),
            accept_ranges: false,
            send_content_length: true,
            chunk_size: 0,
            chunk_delay: Duration::ZERO,
            path: "bad.bin".to_string(),
            request_started: None,
            release_response: None,
        });
        let client = DownloadClient::new(Some(daemon.endpoint.clone()));
        let dir = tempfile::tempdir().unwrap();
        let destination = dir.path().join("bad.bin");
        let mut request = FetchRequest::new(server.url.clone(), &destination);
        request.expected_sha256 = Some("00".repeat(32));

        let err = fetch_with_retry(&client, request.clone()).unwrap_err();
        assert!(err.contains("sha256 mismatch"));
        assert!(!destination.exists());

        let state = client.exists(&request).unwrap();
        assert_eq!(state.kind, FetchStateKind::Missing);
    }

    #[test]
    fn fetch_single_url_max_connections_uses_range_requests() {
        let daemon = TestDaemon::start();
        let body: Vec<u8> = (0..128 * 1024).map(|i| (i % 251) as u8).collect();
        let server = TestHttpServer::start(TestHttpConfig {
            body: Arc::new(body.clone()),
            accept_ranges: true,
            send_content_length: true,
            chunk_size: 4096,
            chunk_delay: Duration::ZERO,
            path: "multipart.bin".to_string(),
            request_started: None,
            release_response: None,
        });
        let client = DownloadClient::new(Some(daemon.endpoint.clone()));
        let dir = tempfile::tempdir().unwrap();
        let mut request = FetchRequest::new(server.url.clone(), dir.path().join("multipart.bin"));
        request.download_options.max_connections = Some(4);
        request.download_options.min_segment_size = Some(1024);
        request.expected_sha256 = Some(sha256_hex(&body));

        let result = fetch_with_retry(&client, request).unwrap();
        assert_eq!(result.status, FetchStatus::Downloaded);
        assert_eq!(result.sha256, sha256_hex(&body));
        assert!(server.range_request_count() >= 2);
    }

    #[test]
    fn fetch_explicit_multipart_urls_concatenates_and_stays_local() {
        let daemon = TestDaemon::start();
        let part_a = b"hello ".to_vec();
        let part_b = b"multipart ".to_vec();
        let part_c = b"world".to_vec();
        let mut full = Vec::new();
        full.extend_from_slice(&part_a);
        full.extend_from_slice(&part_b);
        full.extend_from_slice(&part_c);

        let server_a = TestHttpServer::start(TestHttpConfig {
            body: Arc::new(part_a),
            accept_ranges: false,
            send_content_length: true,
            chunk_size: 0,
            chunk_delay: Duration::ZERO,
            path: "artifact.part-aa".to_string(),
            request_started: None,
            release_response: None,
        });
        let server_b = TestHttpServer::start(TestHttpConfig {
            body: Arc::new(part_b),
            accept_ranges: false,
            send_content_length: true,
            chunk_size: 0,
            chunk_delay: Duration::ZERO,
            path: "artifact.part-ab".to_string(),
            request_started: None,
            release_response: None,
        });
        let server_c = TestHttpServer::start(TestHttpConfig {
            body: Arc::new(part_c),
            accept_ranges: false,
            send_content_length: true,
            chunk_size: 0,
            chunk_delay: Duration::ZERO,
            path: "artifact.part-ac".to_string(),
            request_started: None,
            release_response: None,
        });

        let client = DownloadClient::new(Some(daemon.endpoint.clone()));
        let dir = tempfile::tempdir().unwrap();
        let destination = dir.path().join("artifact.bin");
        let mut request = FetchRequest::new(
            vec![
                server_a.url.clone(),
                server_b.url.clone(),
                server_c.url.clone(),
            ],
            &destination,
        );
        request.expected_sha256 = Some(sha256_hex(&full));

        let first = fetch_with_retry(&client, request.clone()).unwrap();
        assert_eq!(first.status, FetchStatus::Downloaded);
        assert_eq!(first.sha256, sha256_hex(&full));
        assert_eq!(fs::read(&destination).unwrap(), full);
        let request_counts = (
            server_a.request_count(),
            server_b.request_count(),
            server_c.request_count(),
        );

        let second = fetch_with_retry(&client, request.clone()).unwrap();
        assert_eq!(second.status, FetchStatus::AlreadyPresent);
        assert_eq!(
            (
                server_a.request_count(),
                server_b.request_count(),
                server_c.request_count()
            ),
            request_counts
        );

        let state = client.exists(&request).unwrap();
        assert_eq!(state.kind, FetchStateKind::ArtifactReady);
        assert_eq!(state.sha256.as_deref(), Some(first.sha256.as_str()));
    }

    #[test]
    fn fetch_no_wait_returns_locked_while_other_client_is_downloading() {
        let daemon = TestDaemon::start();
        let request_started = Arc::new(AtomicBool::new(false));
        let release_response = Arc::new(AtomicBool::new(false));
        let body: Vec<u8> = (0..512 * 1024).map(|i| (i % 251) as u8).collect();
        let server = TestHttpServer::start(TestHttpConfig {
            body: Arc::new(body),
            accept_ranges: false,
            send_content_length: true,
            chunk_size: 4096,
            chunk_delay: Duration::from_millis(2),
            path: "slow.bin".to_string(),
            request_started: Some(Arc::clone(&request_started)),
            release_response: Some(Arc::clone(&release_response)),
        });
        let dest_dir = tempfile::tempdir().unwrap();
        let destination = dest_dir.path().join("slow.bin");

        let endpoint = daemon.endpoint.clone();
        let url = server.url.clone();
        let destination_for_thread = destination.clone();
        let download_thread = thread::spawn(move || {
            let client = DownloadClient::new(Some(endpoint));
            let request = FetchRequest::new(url, &destination_for_thread);
            fetch_with_retry(&client, request)
        });

        wait_for_test_condition(Duration::from_secs(5), "initial download request", || {
            request_started.load(Ordering::Acquire)
        });

        let client = DownloadClient::new(Some(daemon.endpoint.clone()));
        let mut no_wait = FetchRequest::new(server.url.clone(), &destination);
        no_wait.wait_mode = WaitMode::NoWait;
        let locked = fetch_with_retry(&client, no_wait).unwrap();
        assert_eq!(locked.status, FetchStatus::Locked);

        release_response.store(true, Ordering::Release);
        let completed = download_thread.join().unwrap().unwrap();
        assert_eq!(completed.status, FetchStatus::Downloaded);
    }

    #[test]
    fn fetch_multipart_no_wait_returns_locked_while_other_client_is_downloading() {
        let daemon = TestDaemon::start();
        let request_started = Arc::new(AtomicBool::new(false));
        let release_response = Arc::new(AtomicBool::new(false));
        let slow_server = TestHttpServer::start(TestHttpConfig {
            body: Arc::new((0..512 * 1024).map(|i| (i % 251) as u8).collect()),
            accept_ranges: false,
            send_content_length: true,
            chunk_size: 4096,
            chunk_delay: Duration::from_millis(2),
            path: "slow.part-aa".to_string(),
            request_started: Some(Arc::clone(&request_started)),
            release_response: Some(Arc::clone(&release_response)),
        });
        let fast_server = TestHttpServer::start(TestHttpConfig {
            body: Arc::new(b"tail".to_vec()),
            accept_ranges: false,
            send_content_length: true,
            chunk_size: 0,
            chunk_delay: Duration::ZERO,
            path: "slow.part-ab".to_string(),
            request_started: None,
            release_response: None,
        });
        let dest_dir = tempfile::tempdir().unwrap();
        let destination = dest_dir.path().join("slow.bin");

        let endpoint = daemon.endpoint.clone();
        let source = vec![slow_server.url.clone(), fast_server.url.clone()];
        let destination_for_thread = destination.clone();
        let download_thread = thread::spawn(move || {
            let client = DownloadClient::new(Some(endpoint));
            let request = FetchRequest::new(source, &destination_for_thread);
            fetch_with_retry(&client, request)
        });

        wait_for_test_condition(
            Duration::from_secs(5),
            "initial multipart download request",
            || request_started.load(Ordering::Acquire),
        );

        let client = DownloadClient::new(Some(daemon.endpoint.clone()));
        let mut no_wait = FetchRequest::new(
            vec![slow_server.url.clone(), fast_server.url.clone()],
            &destination,
        );
        no_wait.wait_mode = WaitMode::NoWait;
        let locked = fetch_with_retry(&client, no_wait).unwrap();
        assert_eq!(locked.status, FetchStatus::Locked);

        release_response.store(true, Ordering::Release);
        let completed = download_thread.join().unwrap().unwrap();
        assert_eq!(completed.status, FetchStatus::Downloaded);
    }

    #[test]
    fn fetch_dry_run_avoids_network_and_filesystem_mutation() {
        let daemon = TestDaemon::start();
        let server = TestHttpServer::start(TestHttpConfig {
            body: Arc::new(b"dry-run".to_vec()),
            accept_ranges: false,
            send_content_length: true,
            chunk_size: 0,
            chunk_delay: Duration::ZERO,
            path: "dry.bin".to_string(),
            request_started: None,
            release_response: None,
        });
        let client = DownloadClient::new(Some(daemon.endpoint.clone()));
        let dir = tempfile::tempdir().unwrap();
        let destination = dir.path().join("dry.bin");
        let mut request = FetchRequest::new(server.url.clone(), &destination);
        request.dry_run = true;

        let result = fetch_with_retry(&client, request).unwrap();
        assert_eq!(result.status, FetchStatus::DryRun);
        assert_eq!(server.request_count(), 0);
        assert!(!destination.exists());
    }

    #[test]
    fn fetch_expands_7z_and_exists_reports_expanded_ready() {
        let daemon = TestDaemon::start();
        let dir = tempfile::tempdir().unwrap();
        let source_dir = dir.path().join("source");
        fs::create_dir_all(source_dir.join("bin")).unwrap();
        fs::write(source_dir.join("bin").join("tool.txt"), b"tool data").unwrap();
        let archive_path = dir.path().join("toolchain.7z");
        sevenz_rust::compress_to_path(&source_dir, &archive_path).unwrap();
        let archive_bytes = fs::read(&archive_path).unwrap();

        let server = TestHttpServer::start(TestHttpConfig {
            body: Arc::new(archive_bytes.clone()),
            accept_ranges: false,
            send_content_length: true,
            chunk_size: 0,
            chunk_delay: Duration::ZERO,
            path: "toolchain.7z".to_string(),
            request_started: None,
            release_response: None,
        });
        let client = DownloadClient::new(Some(daemon.endpoint.clone()));
        let cache_path = dir.path().join("cache").join("toolchain.7z");
        let expanded_path = dir.path().join("expanded");
        let mut request = FetchRequest::new(server.url.clone(), &cache_path);
        request.destination_path_expanded = Some(expanded_path.clone());
        request.expected_sha256 = Some(sha256_hex(&archive_bytes));

        let first = fetch_with_retry(&client, request.clone()).unwrap();
        assert_eq!(first.status, FetchStatus::Expanded);
        assert_eq!(first.sha256, sha256_hex(&archive_bytes));
        let extracted = [
            expanded_path.join("source").join("bin").join("tool.txt"),
            expanded_path.join("bin").join("tool.txt"),
            expanded_path.join("tool.txt"),
        ]
        .into_iter()
        .find(|path| path.exists())
        .expect("expected extracted file in expanded directory");
        assert_eq!(fs::read(extracted).unwrap(), b"tool data");

        let state = client.exists(&request).unwrap();
        assert_eq!(state.kind, FetchStateKind::ExpandedReady);
        assert_eq!(state.sha256.as_deref(), Some(first.sha256.as_str()));

        let second = fetch_with_retry(&client, request).unwrap();
        assert_eq!(second.status, FetchStatus::AlreadyExpanded);
        assert_eq!(second.sha256, first.sha256);
    }

    #[test]
    fn fetch_without_expected_sha_then_validate_later_uses_stored_fingerprint() {
        let daemon = TestDaemon::start();
        let body = b"artifact with delayed hash".to_vec();
        let server = TestHttpServer::start(TestHttpConfig {
            body: Arc::new(body.clone()),
            accept_ranges: false,
            send_content_length: true,
            chunk_size: 0,
            chunk_delay: Duration::ZERO,
            path: "delayed.bin".to_string(),
            request_started: None,
            release_response: None,
        });
        let client = DownloadClient::new(Some(daemon.endpoint.clone()));
        let dir = tempfile::tempdir().unwrap();
        let destination = dir.path().join("delayed.bin");

        let first =
            fetch_with_retry(&client, FetchRequest::new(server.url.clone(), &destination)).unwrap();
        assert_eq!(first.status, FetchStatus::Downloaded);
        assert_eq!(first.sha256, sha256_hex(&body));

        let mut later = FetchRequest::new(server.url.clone(), &destination);
        later.expected_sha256 = Some(first.sha256.clone());
        let second = fetch_with_retry(&client, later.clone()).unwrap();
        assert_eq!(second.status, FetchStatus::AlreadyPresent);
        assert_eq!(second.sha256, first.sha256);

        let state = client.exists(&later).unwrap();
        assert_eq!(state.kind, FetchStateKind::ArtifactReady);
        assert_eq!(state.sha256.as_deref(), Some(second.sha256.as_str()));
    }

    #[test]
    fn expanded_state_remains_valid_when_expected_sha_is_added_later() {
        let daemon = TestDaemon::start();
        let dir = tempfile::tempdir().unwrap();
        let archive_path = dir.path().join("bundle.zip");
        {
            let file = File::create(&archive_path).unwrap();
            let mut zip = zip::ZipWriter::new(file);
            let options = zip::write::SimpleFileOptions::default();
            zip.start_file("hello.txt", options).unwrap();
            zip.write_all(b"hello").unwrap();
            zip.finish().unwrap();
        }
        let archive_bytes = fs::read(&archive_path).unwrap();
        let server = TestHttpServer::start(TestHttpConfig {
            body: Arc::new(archive_bytes.clone()),
            accept_ranges: false,
            send_content_length: true,
            chunk_size: 0,
            chunk_delay: Duration::ZERO,
            path: "bundle.zip".to_string(),
            request_started: None,
            release_response: None,
        });
        let client = DownloadClient::new(Some(daemon.endpoint.clone()));
        let cache_path = dir.path().join("cache").join("bundle.zip");
        let expanded_path = dir.path().join("expanded");

        let mut initial = FetchRequest::new(server.url.clone(), &cache_path);
        initial.destination_path_expanded = Some(expanded_path.clone());
        let first = fetch_with_retry(&client, initial).unwrap();
        assert_eq!(first.status, FetchStatus::Expanded);

        let mut later = FetchRequest::new(server.url.clone(), &cache_path);
        later.destination_path_expanded = Some(expanded_path.clone());
        later.expected_sha256 = Some(first.sha256.clone());
        let second = fetch_with_retry(&client, later.clone()).unwrap();
        assert_eq!(second.status, FetchStatus::AlreadyExpanded);
        assert_eq!(second.sha256, first.sha256);

        let state = client.exists(&later).unwrap();
        assert_eq!(state.kind, FetchStateKind::ExpandedReady);
        assert_eq!(state.sha256.as_deref(), Some(second.sha256.as_str()));
    }

    #[test]
    fn force_is_rejected_for_existing_artifact_state() {
        let daemon = TestDaemon::start();
        let body = b"immutable".to_vec();
        let server = TestHttpServer::start(TestHttpConfig {
            body: Arc::new(body),
            accept_ranges: false,
            send_content_length: true,
            chunk_size: 0,
            chunk_delay: Duration::ZERO,
            path: "immutable.bin".to_string(),
            request_started: None,
            release_response: None,
        });
        let client = DownloadClient::new(Some(daemon.endpoint.clone()));
        let dir = tempfile::tempdir().unwrap();
        let destination = dir.path().join("immutable.bin");

        let _ =
            fetch_with_retry(&client, FetchRequest::new(server.url.clone(), &destination)).unwrap();

        let mut force = FetchRequest::new(server.url.clone(), &destination);
        force.force = true;
        let err = fetch_with_retry(&client, force).unwrap_err();
        assert!(err.contains("purge"));
    }

    #[test]
    fn fetch_rejects_unsafe_zip_entries_end_to_end() {
        let daemon = TestDaemon::start();
        let dir = tempfile::tempdir().unwrap();
        let archive_path = dir.path().join("unsafe.zip");
        {
            let file = File::create(&archive_path).unwrap();
            let mut zip = zip::ZipWriter::new(file);
            let options = zip::write::SimpleFileOptions::default();
            zip.start_file("../evil.txt", options).unwrap();
            zip.write_all(b"bad").unwrap();
            zip.finish().unwrap();
        }
        let archive_bytes = fs::read(&archive_path).unwrap();
        let server = TestHttpServer::start(TestHttpConfig {
            body: Arc::new(archive_bytes),
            accept_ranges: false,
            send_content_length: true,
            chunk_size: 0,
            chunk_delay: Duration::ZERO,
            path: "unsafe.zip".to_string(),
            request_started: None,
            release_response: None,
        });
        let client = DownloadClient::new(Some(daemon.endpoint.clone()));
        let cache_path = dir.path().join("cache").join("unsafe.zip");
        let expanded_path = dir.path().join("expanded");
        let mut request = FetchRequest::new(server.url.clone(), &cache_path);
        request.destination_path_expanded = Some(expanded_path.clone());

        let err = fetch_with_retry(&client, request).unwrap_err();
        assert!(err.contains("unsafe zip entry"));
        assert!(!dir.path().join("evil.txt").exists());
    }
}
