//! Artifact fetch pipeline for the download client.
//!
//! Submodules split the pipeline by concern; this `mod.rs` owns the public
//! types and the [`DownloadClient::fetch`] / [`DownloadClient::exists`]
//! orchestration that ties them together. All `pub` items are re-exported
//! here so the public path remains `download_client::artifact::<Name>`.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::download::{DownloadOptions, DownloadPhase};

use super::DownloadClient;

mod archive;
mod hashing;
mod lock;
mod marker;
mod parts;
mod resolve;
mod state;

#[cfg(test)]
mod tests;

use archive::{extract_archive, remove_path_if_exists};
use lock::acquire_fetch_lock;
use marker::{
    expanded_marker_matches, expanded_marker_path, write_artifact_marker, write_expanded_marker,
};
use parts::download_explicit_parts;
use resolve::{resolve_request, resolve_request_no_create};
use state::{cleanup_invalid_fetch_state, exists_resolved, validate_artifact};

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
                        if super::is_terminal(&status) {
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
