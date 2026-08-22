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
use crate::download_client::ArchiveFormat;

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
    sharded(root.join(FETCH_DIR).join(CAS_DIR), digest_hex)
}

/// Shard `hex` under `base` as `<aa>/<bb>/<hex>`.
///
/// Shared by the blob CAS and the tree cache so the two layouts cannot drift.
fn sharded(base: PathBuf, hex: &str) -> PathBuf {
    let mut path = base;
    for level in 0..SHARD_LEVELS {
        let start = level * SHARD_BYTES * 2;
        let end = start + SHARD_BYTES * 2;
        match hex.get(start..end) {
            Some(part) => path.push(part),
            // A digest too short to shard is a caller bug; keep it addressable
            // rather than panicking in a CLI path.
            None => break,
        }
    }
    path.join(hex)
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

pub(crate) async fn cmd_fetch(
    url: &str,
    expect: Option<&str>,
    extract: bool,
    json: bool,
) -> ExitCode {
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
            return finish(
                &root,
                url,
                &cached,
                &record.digest,
                FetchOutcome::Revalidated,
                extract,
                json,
            );
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

    finish(&root, url, &cas, &digest, outcome, extract, json)
}

/// Extracted trees live beside the blob CAS, sharded the same way.
const TREES_DIR: &str = "trees";

/// Domain separator for tree keys. Without it a tree key could collide with a
/// blob digest, which is a bare blake3 of file bytes.
const TREE_KEY_DOMAIN: &str = "zccache-fetch-tree-v1";

/// Whether an extraction was served from cache or actually performed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ExtractOutcome {
    Cached,
    Extracted,
}

impl ExtractOutcome {
    pub(crate) fn as_str(&self) -> &'static str {
        match self {
            Self::Cached => "cached",
            Self::Extracted => "extracted",
        }
    }
}

/// Stable tag per archive format, used in the tree key.
///
/// Written out rather than derived from `Debug` so that renaming a variant
/// cannot silently invalidate every cached tree on disk.
pub(crate) fn format_tag(format: ArchiveFormat) -> &'static str {
    match format {
        ArchiveFormat::Auto => "auto",
        ArchiveFormat::None => "none",
        ArchiveFormat::Zst => "zst",
        ArchiveFormat::Zip => "zip",
        ArchiveFormat::Xz => "xz",
        ArchiveFormat::TarGz => "tar.gz",
        ArchiveFormat::TarXz => "tar.xz",
        ArchiveFormat::TarZst => "tar.zst",
        ArchiveFormat::SevenZip => "7z",
    }
}

/// Key for an extracted tree: archive digest + the extraction options that
/// produced it.
///
/// This is the issue's open question answered as "yes" -- an extracted tree is
/// a CAS entry in its own right. Keying on the *archive digest* rather than on
/// the URL means two URLs serving identical bytes share one extraction, and an
/// origin that changes its validator without changing content still hits.
/// Including the format means a future flag that changes extraction output
/// (strip-components, filters) extends this key instead of silently reusing a
/// tree produced under different options.
pub(crate) fn tree_key(archive_digest: &str, format: ArchiveFormat) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(TREE_KEY_DOMAIN.as_bytes());
    hasher.update(&[0]);
    hasher.update(archive_digest.as_bytes());
    hasher.update(&[0]);
    hasher.update(format_tag(format).as_bytes());
    hasher.finalize().to_hex().to_string()
}

/// `<root>/fetch/trees/<aa>/<bb>/<hex>` for an extracted tree.
pub(crate) fn tree_path_for(root: &Path, tree_key_hex: &str) -> PathBuf {
    sharded(root.join(FETCH_DIR).join(TREES_DIR), tree_key_hex)
}

/// The archive file name implied by a URL, used only for format detection.
///
/// The CAS path has no extension -- it is a bare digest -- so the format has
/// to come from the URL. Query and fragment are stripped first, because
/// `...node.tar.gz?token=x` is common and would otherwise detect as `None`.
pub(crate) fn archive_name_from_url(url: &str) -> String {
    let without_fragment = url.split('#').next().unwrap_or(url);
    let without_query = without_fragment
        .split('?')
        .next()
        .unwrap_or(without_fragment);
    without_query
        .rsplit('/')
        .find(|segment| !segment.is_empty())
        .unwrap_or_default()
        .to_string()
}

/// Resolve the extraction format for a URL, or explain why it cannot be.
fn extraction_format(url: &str) -> Result<ArchiveFormat, String> {
    let name = archive_name_from_url(url);
    let format = crate::download_client::artifact::archive::auto_archive_format(Path::new(&name))
        .unwrap_or(ArchiveFormat::None);
    if matches!(format, ArchiveFormat::None | ArchiveFormat::Auto) {
        return Err(format!(
            "--extract: cannot determine an archive format from {name:?} \
             (supported: .zip, .tar.gz, .tar.xz, .tar.zst, .tzst, .7z, .xz, .zst)"
        ));
    }
    Ok(format)
}

/// Extract `archive` into the tree cache, or report that it is already there.
///
/// Publication is a single directory rename. That is what makes "the tree
/// directory exists" mean "the extraction finished": a run interrupted partway
/// leaves its debris in `tmp/`, never at the final path, so the next run
/// cannot mistake a half-extracted tree for a complete one. Checking for the
/// directory and extracting *into* it would have exactly that bug.
fn ensure_extracted(
    root: &Path,
    archive: &Path,
    digest: &str,
    format: ArchiveFormat,
) -> Result<(PathBuf, ExtractOutcome), String> {
    let key = tree_key(digest, format);
    let final_path = tree_path_for(root, &key);
    if final_path.is_dir() {
        return Ok((final_path, ExtractOutcome::Cached));
    }

    let tmp_root = root.join(FETCH_DIR).join(TMP_DIR);
    std::fs::create_dir_all(&tmp_root)
        .map_err(|e| format!("cannot create extraction staging dir: {e}"))?;
    let staging = tmp_root.join(format!("{key}.tree"));
    // Debris from an interrupted earlier run must not be extracted over.
    let _ = std::fs::remove_dir_all(&staging);

    crate::download_client::artifact::archive::extract_archive_at(archive, format, &staging)
        .map_err(|e| {
            let _ = std::fs::remove_dir_all(&staging);
            format!("extraction failed: {e}")
        })?;

    if let Some(parent) = final_path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("cannot create tree cache directory: {e}"))?;
    }
    match std::fs::rename(&staging, &final_path) {
        Ok(()) => Ok((final_path, ExtractOutcome::Extracted)),
        // Another process published the same tree while we were extracting.
        // Its bytes are ours by construction (same key), so ours is redundant.
        Err(_) if final_path.is_dir() => {
            let _ = std::fs::remove_dir_all(&staging);
            Ok((final_path, ExtractOutcome::Cached))
        }
        Err(err) => {
            let _ = std::fs::remove_dir_all(&staging);
            Err(format!("cannot publish extracted tree: {err}"))
        }
    }
}

/// Common tail for every route into a cached blob: optionally extract, then
/// report. Shared so `--extract` cannot be honoured on one path and skipped on
/// another -- the bug `--expect` already had on the revalidated path.
fn finish(
    root: &Path,
    url: &str,
    cas: &Path,
    digest: &str,
    outcome: FetchOutcome,
    extract: bool,
    json: bool,
) -> ExitCode {
    if !extract {
        return report(cas, digest, outcome, None, None, json);
    }
    let format = match extraction_format(url) {
        Ok(format) => format,
        Err(err) => {
            eprintln!("zccache: {err}");
            return ExitCode::FAILURE;
        }
    };
    match ensure_extracted(root, cas, digest, format) {
        Ok((tree, extract_outcome)) => report(
            cas,
            digest,
            outcome,
            Some(&tree),
            Some(extract_outcome),
            json,
        ),
        Err(err) => {
            eprintln!("zccache: {err}");
            ExitCode::FAILURE
        }
    }
}

fn report(
    path: &Path,
    digest: &str,
    outcome: FetchOutcome,
    tree: Option<&Path>,
    extract_outcome: Option<ExtractOutcome>,
    json: bool,
) -> ExitCode {
    if json {
        let mut payload = serde_json::json!({
            "path": path,
            "digest": digest,
            "outcome": outcome.as_str(),
        });
        if let (Some(tree), Some(extract_outcome)) = (tree, extract_outcome) {
            payload["extracted"] = serde_json::json!(tree);
            payload["extraction"] = serde_json::json!(extract_outcome.as_str());
        }
        println!("{payload}");
    } else {
        // With --extract the useful path is the tree, not the archive blob:
        // the caller wants something to run, not something to unpack.
        println!("{}", tree.unwrap_or(path).display());
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

    // --- extraction cache (issue #1469, acceptance criterion 7) ---

    /// Build a real `.tar.gz` so the extraction tests exercise the actual
    /// decoder rather than a stand-in.
    fn write_targz(path: &Path, entries: &[(&str, &[u8])]) {
        let file = std::fs::File::create(path).expect("create archive");
        let encoder = flate2::write::GzEncoder::new(file, flate2::Compression::fast());
        let mut builder = tar::Builder::new(encoder);
        for (name, body) in entries {
            let mut header = tar::Header::new_gnu();
            header.set_size(body.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            builder
                .append_data(&mut header, name, *body)
                .expect("append entry");
        }
        builder
            .into_inner()
            .expect("finish tar")
            .finish()
            .expect("finish gz");
    }

    #[test]
    fn a_tree_key_is_not_the_archive_digest() {
        // Domain separation: a tree key must never collide with a blob digest,
        // since both are 64 hex chars living under the same root.
        let digest = "ab".repeat(32);

        assert_ne!(tree_key(&digest, ArchiveFormat::TarGz), digest);
    }

    #[test]
    fn a_tree_key_depends_on_the_extraction_format() {
        // Same bytes unpacked under different options are different trees.
        let digest = "cd".repeat(32);

        assert_ne!(
            tree_key(&digest, ArchiveFormat::TarGz),
            tree_key(&digest, ArchiveFormat::Zip)
        );
    }

    #[test]
    fn identical_archives_share_one_tree_key() {
        // This is what makes two URLs serving identical bytes -- or an origin
        // that changed only its ETag -- reuse one extraction.
        let digest = "ef".repeat(32);

        assert_eq!(
            tree_key(&digest, ArchiveFormat::TarGz),
            tree_key(&digest, ArchiveFormat::TarGz)
        );
    }

    #[test]
    fn a_url_name_drops_query_and_fragment() {
        // `?token=` on a release URL is common; without stripping it the
        // format would detect as None and --extract would refuse a valid
        // archive.
        assert_eq!(
            archive_name_from_url("https://h/x/node.tar.gz?token=abc"),
            "node.tar.gz"
        );
        assert_eq!(
            archive_name_from_url("https://h/x/node.tar.gz#frag"),
            "node.tar.gz"
        );
        assert_eq!(archive_name_from_url("https://h/x/node.zip"), "node.zip");
    }

    #[test]
    fn an_unrecognized_extension_is_refused_rather_than_guessed() {
        // Silently treating an unknown payload as a single-file copy would
        // produce a "tree" that is really one file at a directory path.
        assert!(extraction_format("https://h/tool.bin").is_err());
        assert!(extraction_format("https://h/tool.tar.gz").is_ok());
    }

    #[test]
    fn an_archive_is_extracted_once_and_reused() {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path();
        let archive = root.join("payload.tar.gz");
        write_targz(&archive, &[("bin/tool", b"payload")]);
        let digest = "11".repeat(32);

        let (first, first_outcome) =
            ensure_extracted(root, &archive, &digest, ArchiveFormat::TarGz).expect("extract");
        let (second, second_outcome) =
            ensure_extracted(root, &archive, &digest, ArchiveFormat::TarGz).expect("reuse");

        assert_eq!(first_outcome, ExtractOutcome::Extracted);
        assert_eq!(
            second_outcome,
            ExtractOutcome::Cached,
            "second call re-extracted"
        );
        assert_eq!(first, second);
        assert_eq!(
            std::fs::read(first.join("bin/tool")).expect("read entry"),
            b"payload"
        );
    }

    #[test]
    fn a_partial_extraction_is_never_served_as_complete() {
        // The failure this guards: check-then-extract-in-place leaves a
        // half-populated directory at the final path when interrupted, and
        // the next run sees "directory exists" and hands it out. Publication
        // by rename means debris can only ever sit in tmp/.
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path();
        let archive = root.join("payload.tar.gz");
        write_targz(&archive, &[("bin/tool", b"payload")]);
        let digest = "22".repeat(32);
        let key = tree_key(&digest, ArchiveFormat::TarGz);

        // Simulate an interrupted run: staging debris, nothing published.
        let staging = root
            .join(FETCH_DIR)
            .join(TMP_DIR)
            .join(format!("{key}.tree"));
        std::fs::create_dir_all(staging.join("bin")).expect("staging");
        std::fs::write(staging.join("bin/truncated"), b"partial").expect("debris");

        let (tree, outcome) =
            ensure_extracted(root, &archive, &digest, ArchiveFormat::TarGz).expect("extract");

        assert_eq!(
            outcome,
            ExtractOutcome::Extracted,
            "debris was served as a hit"
        );
        assert!(tree.join("bin/tool").is_file(), "real entry missing");
        assert!(
            !tree.join("bin/truncated").exists(),
            "debris from the interrupted run leaked into the published tree"
        );
    }

    #[test]
    fn extraction_outcomes_have_distinct_labels() {
        assert_ne!(
            ExtractOutcome::Cached.as_str(),
            ExtractOutcome::Extracted.as_str()
        );
    }

    #[test]
    fn a_tree_path_sits_under_two_shard_levels() {
        let key = "ab".repeat(32);
        let path = tree_path_for(Path::new("/root"), &key);
        let parts: Vec<_> = path
            .components()
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect();

        assert!(parts.contains(&TREES_DIR.to_string()));
        assert_eq!(parts[parts.len() - 1], key);
        assert_eq!(parts[parts.len() - 2], "ab");
        assert_eq!(parts[parts.len() - 3], "ab");
    }
}
