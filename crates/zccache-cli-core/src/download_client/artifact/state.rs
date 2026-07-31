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
    match request.expected_sha256.as_ref() {
        Some(expected_sha256) => fingerprint.sha256 == *expected_sha256,
        // #1172: `unwrap_or(true)` meant "no checksum supplied" and "checksum
        // matched" were the same answer, so a caller could not tell an
        // unverified artifact from a verified one. When the caller asked for
        // verification, absence is a failure, not a pass.
        None => !request.require_checksum,
    }
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
    // #1172: refuse before hashing. A required-but-absent checksum is a
    // configuration failure, and reporting it as such is more useful than
    // silently validating nothing.
    if request.require_checksum && request.expected_sha256.is_none() {
        return Err(format!(
            "refusing to accept {}: no expected sha256 was supplied and checksum \
             verification is required (set one on the request, or unset {})",
            request.cache_path.display(),
            super::REQUIRE_CHECKSUM_ENV,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::download_client::artifact::{
        checksum_required_by_policy_with, FetchRequest, REQUIRE_CHECKSUM_ENV,
    };

    fn fingerprint(sha256: &str) -> ArtifactFingerprint {
        ArtifactFingerprint {
            bytes: 1,
            sha256: sha256.to_string(),
        }
    }

    fn resolved(expected: Option<&str>, require: bool) -> ResolvedFetchRequest {
        let mut request = FetchRequest::new("https://example.invalid/a.tar", "/tmp/a.tar");
        request.expected_sha256 = expected.map(str::to_string);
        request.require_checksum = require;
        super::super::resolve::resolve_request_no_create(&request).expect("resolve")
    }

    /// #1172: `unwrap_or(true)` made "no checksum supplied" and "checksum
    /// matched" the same answer, so a caller could not tell an unverified
    /// artifact from a verified one. When verification was asked for, absence
    /// must be a failure.
    #[test]
    fn a_missing_checksum_does_not_pass_as_a_match_when_verification_is_required() {
        let any = fingerprint("aa");
        assert!(
            artifact_matches_request(&resolved(None, false), &any),
            "without the requirement, today's permissive behaviour is preserved"
        );
        assert!(
            !artifact_matches_request(&resolved(None, true), &any),
            "with the requirement, an unverified artifact must not match"
        );
    }

    #[test]
    fn a_supplied_checksum_is_still_compared_normally() {
        assert!(artifact_matches_request(
            &resolved(Some("aa"), true),
            &fingerprint("aa")
        ));
        assert!(!artifact_matches_request(
            &resolved(Some("aa"), true),
            &fingerprint("bb")
        ));
    }

    /// The error has to name the knob, or an operator who turned the policy on
    /// fleet-wide gets a failure they cannot act on.
    #[test]
    fn requiring_a_checksum_without_supplying_one_is_a_named_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.tar");
        std::fs::write(&path, b"payload").unwrap();

        let mut request = FetchRequest::new("https://example.invalid/a.tar", path.clone());
        request.require_checksum = true;
        let resolved = super::super::resolve::resolve_request_no_create(&request).expect("resolve");

        let error = validate_artifact(&resolved).expect_err("must refuse");
        assert!(
            error.contains(REQUIRE_CHECKSUM_ENV),
            "the refusal must name the policy knob: {error}"
        );
    }

    #[test]
    fn the_policy_env_var_is_off_by_default_and_accepts_falsey_values() {
        assert!(!checksum_required_by_policy_with(|_| None));
        assert!(!checksum_required_by_policy_with(|_| Some("0".to_string())));
        assert!(!checksum_required_by_policy_with(|_| Some(
            "false".to_string()
        )));
        assert!(!checksum_required_by_policy_with(|_| Some(
            "  ".to_string()
        )));
        assert!(checksum_required_by_policy_with(|_| Some("1".to_string())));
        assert!(checksum_required_by_policy_with(|_| Some(
            "yes".to_string()
        )));
    }
}
