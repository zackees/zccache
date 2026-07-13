use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::download::stable_download_id;

use super::archive::detect_archive_format;
use super::hashing::ArtifactFingerprint;
use super::resolve::ResolvedFetchRequest;
use super::{ArchiveFormat, DownloadSource};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct ExpandedMarker {
    source: DownloadSource,
    cache_path: String,
    artifact_sha256: String,
    archive_format: ArchiveFormat,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct ArtifactMarker {
    source: DownloadSource,
    cache_path: String,
    sha256: String,
    bytes: u64,
}

pub(super) fn artifact_marker_path(cache_path: &Path) -> PathBuf {
    let hash = stable_download_id(cache_path);
    crate::core::config::daemon_state_dir()
        .join("downloads")
        .join("artifact-state")
        .join(format!("{hash}.json"))
        .into_path_buf()
}

pub(super) fn expanded_marker_path(expanded_path: &Path) -> PathBuf {
    let hash = stable_download_id(expanded_path);
    crate::core::config::daemon_state_dir()
        .join("downloads")
        .join("expanded-state")
        .join(format!("{hash}.json"))
        .into_path_buf()
}

pub(super) fn read_or_compute_artifact_fingerprint(
    request: &ResolvedFetchRequest,
) -> Result<ArtifactFingerprint, String> {
    let fingerprint = super::hashing::compute_artifact_fingerprint(&request.cache_path)
        .map_err(|e| e.to_string())?;
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

pub(super) fn write_artifact_marker(
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

pub(super) fn expanded_marker_matches(
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

pub(super) fn write_expanded_marker(
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
