use super::archive::remove_path_if_exists;
use super::hashing::{compute_artifact_fingerprint, ArtifactFingerprint};
use super::marker::{
    artifact_marker_path, expanded_marker_matches, expanded_marker_path,
    read_or_compute_artifact_fingerprint,
};
use super::resolve::ResolvedFetchRequest;
use super::{FetchState, FetchStateKind};

pub(super) fn exists_resolved(request: &ResolvedFetchRequest) -> Result<FetchState, String> {
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

pub(super) fn artifact_matches_request(
    request: &ResolvedFetchRequest,
    fingerprint: &ArtifactFingerprint,
) -> bool {
    request
        .expected_sha256
        .as_ref()
        .map(|expected_sha256| fingerprint.sha256 == *expected_sha256)
        .unwrap_or(true)
}

pub(super) fn validate_artifact(
    request: &ResolvedFetchRequest,
) -> Result<ArtifactFingerprint, String> {
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

pub(super) fn cleanup_invalid_fetch_state(request: &ResolvedFetchRequest) {
    let _ = remove_path_if_exists(&request.cache_path);
    let _ = remove_path_if_exists(&artifact_marker_path(&request.cache_path));
    if let Some(expanded_path) = &request.expanded_path {
        let _ = remove_path_if_exists(expanded_path);
        let _ = remove_path_if_exists(&expanded_marker_path(expanded_path));
    }
}
