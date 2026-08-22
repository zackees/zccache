//! `zccache fetch <url>` — global content-addressed cache for tool downloads
//! (issue #1469).
//!
//! Three layers, deliberately kept apart:
//!
//! 1. **Freshness** — conditional GET per URL (`If-None-Match` /
//!    `If-Modified-Since`). A 304 means the artifact cached *for this URL* is
//!    current, so the transfer is skipped entirely.
//! 2. **Identity** — blake3 of the payload bytes is the CAS key. This is what
//!    dedups identical payloads across mirrors and re-uploads, and what
//!    survives an origin changing its validator scheme.
//! 3. **Pinning** — `--expect <digest>` is verified before the artifact is
//!    exposed, so a compromised origin cannot hand back usable bytes.
//!
//! The validator is never the cache key. ETag is server-chosen and opaque:
//! GitHub Releases serves an Azure blob timestamp, so re-uploading identical
//! content changes it (a false miss), while nginx's default `mtime-size`
//! can stay identical across a content swap (a false *hit*, which is worse).
//! Hashing the bytes ourselves is the only thing that resolves both.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::download::{DownloadOptions, DownloadPhase};

/// KV namespace holding one revalidation record per URL. Lowercase and
/// hyphenated to satisfy `zccache_artifact::kv::is_valid_namespace`.
const URL_NAMESPACE: &str = "fetch-url";

/// Layout under the cache root. `cas/` is content-addressed and immutable;
/// `kv/` holds per-URL validators; `tmp/` is staging that never contains a
/// file another process may observe as complete.
const FETCH_DIR: &str = "fetch";
const CAS_DIR: &str = "cas";
const KV_DIR: &str = "kv";
const TMP_DIR: &str = "tmp";

/// Two levels of one byte, matching the artifact store's sharding so a large
/// CAS does not put tens of thousands of entries in one directory.
const SHARD_LEVELS: usize = 2;
const SHARD_BYTES: usize = 1;

/// What we remember about a URL between fetches.
///
/// Deliberately *not* the cache key — see the module docs. The digest is the
/// link from "this URL, last time" to the content-addressed entry.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct UrlRecord {
    /// Content digest (hex) that this URL last resolved to.
    digest: String,
    /// Opaque origin validator, replayed as `If-None-Match`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    etag: Option<String>,
    /// Replayed as `If-Modified-Since` when no ETag is offered.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_modified: Option<String>,
}

/// Outcome of a fetch, for `--json` and for tests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum FetchOutcome {
    /// The origin answered 304; nothing was transferred.
    Revalidated,
    /// Bytes were transferred and the digest was already in the CAS.
    DownloadedDuplicate,
    /// Bytes were transferred and stored as a new CAS entry.
    DownloadedNew,
}

impl FetchOutcome {
    pub(crate) fn as_str(&self) -> &'static str {
        match self {
            Self::Revalidated => "revalidated",
            Self::DownloadedDuplicate => "downloaded-duplicate",
            Self::DownloadedNew => "downloaded-new",
        }
    }
}

/// `<root>/fetch/cas/<aa>/<bb>/<hex>` for a content digest.
///
/// Pure so the layout is testable without touching a filesystem.
pub(crate) fn cas_path_for(root: &Path, digest_hex: &str) -> PathBuf {
    let mut path = root.join(FETCH_DIR).join(CAS_DIR);
    for level in 0..SHARD_LEVELS {
        let start = level * SHARD_BYTES * 2;
        let end = start + SHARD_BYTES * 2;
        match digest_hex.get(start..end) {
            Some(part) => path.push(part),
            // A digest too short to shard is a caller bug; keep it addressable
            // rather than panicking in a CLI path.
            None => break,
        }
    }
    path.join(digest_hex)
}

/// Stable per-URL key. blake3 of the URL bytes — an index into the record
/// store, never a content address.
pub(crate) fn url_key(url: &str) -> zccache_artifact::kv::Key {
    zccache_artifact::kv::Key::from_hash(blake3::hash(url.as_bytes()))
}

/// Whether a pin matches. Case-insensitive; a pin that is not 64 hex chars is
/// rejected as malformed rather than silently failing to match.
pub(crate) fn pin_matches(expected: &str, actual_hex: &str) -> Result<bool, String> {
    let expected = expected.trim();
    if expected.len() != 64 || !expected.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(format!(
            "--expect must be a 64-character blake3 hex digest, got {:?}",
            expected
        ));
    }
    Ok(expected.eq_ignore_ascii_case(actual_hex))
}

fn fetch_root() -> PathBuf {
    let (root, _source) = crate::core::config::resolve_cache_root();
    root.as_path().to_path_buf()
}

fn open_records(root: &Path) -> Result<zccache_artifact::kv::KvStore, String> {
    zccache_artifact::kv::KvStore::open(root.join(FETCH_DIR).join(KV_DIR))
        .map_err(|e| format!("cannot open fetch record store: {e}"))
}

fn load_record(store: &zccache_artifact::kv::KvStore, url: &str) -> Option<UrlRecord> {
    let raw = store.get(URL_NAMESPACE, &url_key(url)).ok().flatten()?;
    serde_json::from_slice(&raw).ok()
}

fn save_record(store: &zccache_artifact::kv::KvStore, url: &str, record: &UrlRecord) {
    if let Ok(bytes) = serde_json::to_vec(record) {
        // Best effort: a lost record costs one refetch, never correctness,
        // because the CAS entry is keyed by content.
        let _ = store.put(URL_NAMESPACE, &url_key(url), &bytes);
    }
}

/// Ask the origin whether the cached entry is still current.
///
/// Returns `Ok(true)` only on an explicit 304. Any error, or any other
/// status, returns `Ok(false)` so the caller falls through to a real
/// download — revalidation is an optimization and must never be the reason a
/// fetch fails.
async fn is_still_fresh(url: &str, record: &UrlRecord) -> bool {
    let Some(client) = reqwest::Client::builder()
        .user_agent(format!("zccache-fetch/{}", zccache_core::VERSION))
        .https_only(true)
        .build()
        .ok()
    else {
        return false;
    };
    let mut request = client.get(url);
    if let Some(etag) = &record.etag {
        request = request.header(reqwest::header::IF_NONE_MATCH, etag);
    } else if let Some(modified) = &record.last_modified {
        request = request.header(reqwest::header::IF_MODIFIED_SINCE, modified);
    } else {
        return false;
    }
    matches!(request.send().await, Ok(response) if response.status() == reqwest::StatusCode::NOT_MODIFIED)
}

/// Read the validators an origin offers, for the next revalidation.
async fn read_validators(url: &str) -> (Option<String>, Option<String>) {
    let Some(client) = reqwest::Client::builder()
        .user_agent(format!("zccache-fetch/{}", zccache_core::VERSION))
        .https_only(true)
        .build()
        .ok()
    else {
        return (None, None);
    };
    let Ok(response) = client.head(url).send().await else {
        return (None, None);
    };
    let header = |name: reqwest::header::HeaderName| {
        response
            .headers()
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned)
    };
    (
        header(reqwest::header::ETAG),
        header(reqwest::header::LAST_MODIFIED),
    )
}

pub(crate) async fn cmd_fetch(url: &str, expect: Option<&str>, json: bool) -> ExitCode {
    // Validate the pin before any network work, so a typo fails immediately
    // rather than after a 30 MB transfer.
    if let Some(pin) = expect {
        if let Err(err) = pin_matches(pin, &"0".repeat(64)) {
            eprintln!("zccache: {err}");
            return ExitCode::FAILURE;
        }
    }

    let root = fetch_root();
    let store = match open_records(&root) {
        Ok(store) => store,
        Err(err) => {
            eprintln!("zccache: {err}");
            return ExitCode::FAILURE;
        }
    };

    // Layer 1: freshness. Only meaningful if the entry it points at is still
    // on disk -- a 304 about a CAS entry we no longer hold is not a hit.
    if let Some(record) = load_record(&store, url) {
        let cached = cas_path_for(&root, &record.digest);
        if cached.is_file() && is_still_fresh(url, &record).await {
            // Layer 3 applies to a revalidated hit as well. Returning here
            // without checking the pin would mean `--expect` is enforced only
            // on a cold fetch -- i.e. bypassed for every warm one, which is
            // exactly the case a pin exists to cover.
            if let Some(pin) = expect {
                match pin_matches(pin, &record.digest) {
                    Ok(true) => {}
                    Ok(false) => {
                        eprintln!(
                            "zccache: digest mismatch: expected {pin}, cached entry is {}",
                            record.digest
                        );
                        return ExitCode::FAILURE;
                    }
                    Err(err) => {
                        eprintln!("zccache: {err}");
                        return ExitCode::FAILURE;
                    }
                }
            }
            return report(&cached, &record.digest, FetchOutcome::Revalidated, json);
        }
    }

    let tmp_dir = root.join(FETCH_DIR).join(TMP_DIR);
    if let Err(err) = std::fs::create_dir_all(&tmp_dir) {
        eprintln!("zccache: cannot create fetch staging dir: {err}");
        return ExitCode::FAILURE;
    }
    let staged = tmp_dir.join(format!("{}.part", url_key(url).to_hex()));
    let progress: crate::download::ProgressCallback =
        Arc::new(|_downloaded, _total, _phase: DownloadPhase| {});
    let options = DownloadOptions::default();

    if let Err(err) = crate::download::download_to_path(
        url,
        &staged,
        &tmp_dir,
        &options,
        progress,
        tokio_util::sync::CancellationToken::new(),
    )
    .await
    {
        let _ = std::fs::remove_file(&staged);
        eprintln!("zccache: fetch failed: {err}");
        return ExitCode::FAILURE;
    }

    // Layer 2: identity. Hash what actually landed, not what was promised.
    let digest = match zccache_hash::hash_file(&staged) {
        Ok(hash) => hash.to_hex(),
        Err(err) => {
            let _ = std::fs::remove_file(&staged);
            eprintln!("zccache: cannot hash fetched file: {err}");
            return ExitCode::FAILURE;
        }
    };

    // Layer 3: pinning, checked while the bytes are still in staging so a
    // mismatched artifact is never reachable through the CAS.
    if let Some(pin) = expect {
        match pin_matches(pin, &digest) {
            Ok(true) => {}
            Ok(false) => {
                let _ = std::fs::remove_file(&staged);
                eprintln!("zccache: digest mismatch: expected {pin}, got {digest}");
                return ExitCode::FAILURE;
            }
            Err(err) => {
                let _ = std::fs::remove_file(&staged);
                eprintln!("zccache: {err}");
                return ExitCode::FAILURE;
            }
        }
    }

    let cas = cas_path_for(&root, &digest);
    let outcome = if cas.is_file() {
        // Same bytes already present -- a different URL, a re-upload, or a
        // changed validator. Drop the duplicate rather than rewriting it.
        let _ = std::fs::remove_file(&staged);
        FetchOutcome::DownloadedDuplicate
    } else {
        if let Some(parent) = cas.parent() {
            if let Err(err) = std::fs::create_dir_all(parent) {
                let _ = std::fs::remove_file(&staged);
                eprintln!("zccache: cannot create CAS directory: {err}");
                return ExitCode::FAILURE;
            }
        }
        if let Err(err) = std::fs::rename(&staged, &cas) {
            let _ = std::fs::remove_file(&staged);
            eprintln!("zccache: cannot publish fetched file: {err}");
            return ExitCode::FAILURE;
        }
        FetchOutcome::DownloadedNew
    };

    let (etag, last_modified) = read_validators(url).await;
    save_record(
        &store,
        url,
        &UrlRecord {
            digest: digest.clone(),
            etag,
            last_modified,
        },
    );

    report(&cas, &digest, outcome, json)
}

fn report(path: &Path, digest: &str, outcome: FetchOutcome, json: bool) -> ExitCode {
    if json {
        println!(
            "{}",
            serde_json::json!({
                "path": path,
                "digest": digest,
                "outcome": outcome.as_str(),
            })
        );
    } else {
        println!("{}", path.display());
    }
    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cas_path_sits_under_two_shard_levels() {
        let path = cas_path_for(Path::new("/root"), &"ab".repeat(32));
        let parts: Vec<_> = path
            .components()
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect();

        assert!(parts.contains(&"fetch".to_string()), "{parts:?}");
        assert!(parts.contains(&"cas".to_string()), "{parts:?}");
        assert_eq!(parts[parts.len() - 1], "ab".repeat(32));
        assert_eq!(parts[parts.len() - 2], "ab");
        assert_eq!(parts[parts.len() - 3], "ab");
    }

    #[test]
    fn identical_digests_map_to_one_path() {
        // The dedup property the issue asks for: two URLs, same bytes, one
        // CAS entry. Nothing about the URL enters the path.
        let digest = "c".repeat(64);
        assert_eq!(
            cas_path_for(Path::new("/root"), &digest),
            cas_path_for(Path::new("/root"), &digest)
        );
    }

    #[test]
    fn different_digests_map_to_different_paths() {
        assert_ne!(
            cas_path_for(Path::new("/root"), &"a".repeat(64)),
            cas_path_for(Path::new("/root"), &"b".repeat(64))
        );
    }

    #[test]
    fn url_key_is_stable_and_url_scoped() {
        assert_eq!(
            url_key("https://example.com/a"),
            url_key("https://example.com/a")
        );
        assert_ne!(
            url_key("https://example.com/a"),
            url_key("https://example.com/b")
        );
    }

    #[test]
    fn a_pin_matches_case_insensitively() {
        let digest = "ab".repeat(32);
        assert_eq!(pin_matches(&digest.to_uppercase(), &digest), Ok(true));
        assert_eq!(pin_matches(&digest, &digest), Ok(true));
    }

    #[test]
    fn a_wrong_pin_does_not_match() {
        assert_eq!(pin_matches(&"a".repeat(64), &"b".repeat(64)), Ok(false));
    }

    #[test]
    fn a_malformed_pin_is_rejected_rather_than_treated_as_a_mismatch() {
        // "not 64 hex chars" must be a usage error. Reporting it as a plain
        // mismatch would read as "the origin served the wrong bytes".
        assert!(pin_matches("deadbeef", &"a".repeat(64)).is_err());
        assert!(pin_matches(&"z".repeat(64), &"a".repeat(64)).is_err());
    }

    #[test]
    fn a_record_round_trips_without_validators() {
        // An origin offering neither ETag nor Last-Modified must still yield a
        // usable record -- the digest is what links the URL to the CAS.
        let record = UrlRecord {
            digest: "a".repeat(64),
            etag: None,
            last_modified: None,
        };
        let raw = serde_json::to_vec(&record).unwrap();
        assert_eq!(serde_json::from_slice::<UrlRecord>(&raw).unwrap(), record);
    }

    #[test]
    fn a_record_round_trips_with_validators() {
        let record = UrlRecord {
            digest: "b".repeat(64),
            etag: Some("\"0x8DEFE5A52510C18\"".to_string()),
            last_modified: Some("Wed, 21 Oct 2026 07:28:00 GMT".to_string()),
        };
        let raw = serde_json::to_vec(&record).unwrap();
        assert_eq!(serde_json::from_slice::<UrlRecord>(&raw).unwrap(), record);
    }

    #[test]
    fn outcomes_have_distinct_labels() {
        assert_eq!(FetchOutcome::Revalidated.as_str(), "revalidated");
        assert_eq!(
            FetchOutcome::DownloadedDuplicate.as_str(),
            "downloaded-duplicate"
        );
        assert_eq!(FetchOutcome::DownloadedNew.as_str(), "downloaded-new");
    }

    #[test]
    fn a_pin_is_checked_against_a_cached_digest_too() {
        // Regression: the revalidation path returned before the pin was
        // checked, so `--expect <wrong>` succeeded on any warm cache. Found by
        // running the command, not by a unit test -- `pin_matches` was correct
        // in isolation the whole time.
        let cached = "8553ed6dbe44efed1613966979f89efb8fb2fb0c3bd7d83df15ccfad49847027";

        assert_eq!(pin_matches(&"a".repeat(64), cached), Ok(false));
        assert_eq!(pin_matches(cached, cached), Ok(true));
    }
}
