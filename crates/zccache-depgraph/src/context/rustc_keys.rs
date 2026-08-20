//! Rustc artifact and verdict key computation.

use std::path::Path;
use std::sync::Arc;

use zccache_hash::ContentHash;

use super::{normalize_key_path, ArtifactKey, ContextKey};

/// Compute the artifact key for a rustc compilation.
///
/// Like `compute_artifact_key` for C/C++, but also incorporates
/// extern crate content hashes (analogous to header content hashes).
pub fn compute_rustc_artifact_key<P: AsRef<Path> + Ord>(
    context_key: &ContextKey,
    file_hashes: &mut [(P, ContentHash)],
    extern_hashes: &mut [(String, ContentHash)],
) -> ArtifactKey {
    compute_rustc_artifact_key_with_root(context_key, file_hashes, extern_hashes, None)
}

/// Compute the result-verdict key for a rustc artifact.
///
/// Output bytes are keyed only by compiler inputs. Diagnostics and exit
/// status live behind this second key so a Dylint invocation cannot consume
/// a plain rustc verdict (or vice versa) while both may share identical
/// artifact bytes.
#[must_use]
pub fn compute_rustc_verdict_key(
    artifact_key_hex: &str,
    dylint_input_hash: Option<&str>,
) -> ArtifactKey {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"zccache-rustc-verdict-key-v1\0");
    hasher.update(artifact_key_hex.as_bytes());
    hasher.update(b"\0mode\0");
    match dylint_input_hash {
        Some(hash) => {
            hasher.update(b"dylint\0");
            hasher.update(hash.as_bytes());
        }
        None => {
            hasher.update(b"plain");
        }
    };
    ArtifactKey(ContentHash::from_bytes(*hasher.finalize().as_bytes()))
}

/// Compute the rustc artifact key, optionally normalizing project-local files.
///
/// When `key_root` is provided, source and dependency file paths under that
/// root are hashed relative to it. Extern hashes remain keyed by crate name.
pub fn compute_rustc_artifact_key_with_root<P: AsRef<Path> + Ord>(
    context_key: &ContextKey,
    file_hashes: &mut [(P, ContentHash)],
    extern_hashes: &mut [(String, ContentHash)],
    key_root: Option<&Path>,
) -> ArtifactKey {
    compute_rustc_artifact_key_with_root_with(
        context_key,
        file_hashes,
        extern_hashes,
        key_root,
        |path, key_root| normalize_key_path(path, key_root).into(),
    )
}

/// Like [`compute_rustc_artifact_key_with_root`] but the per-header path
/// normalization is delegated to a caller-supplied closure. Used by the
/// daemon's rustc miss/update paths to consult
/// `DepGraph::cached_normalize_key_path` for the per-header allocation
/// amortization (issue #550).
pub fn compute_rustc_artifact_key_with_root_with<P, F>(
    context_key: &ContextKey,
    file_hashes: &mut [(P, ContentHash)],
    extern_hashes: &mut [(String, ContentHash)],
    key_root: Option<&Path>,
    mut normalize: F,
) -> ArtifactKey
where
    P: AsRef<Path> + Ord,
    F: FnMut(&Path, Option<&Path>) -> Arc<str>,
{
    // Issue #571: pre-normalize each path once (O(n) via the cached
    // closure), then sort on the cached Arc<str> keys (O(n log n) byte
    // compares). The previous shape called `normalize` twice per
    // sort-comparison AND once per hash-loop entry — 3 calls per
    // element. With ~600 transitive headers per cpp/rust compile,
    // this collapses ~10k normalize calls into ~600. Hash output is
    // bit-identical: same sort order, same blake3 input bytes.
    let mut indexed: Vec<(Arc<str>, ContentHash)> = file_hashes
        .iter()
        .map(|(p, h)| (normalize(p.as_ref(), key_root), *h))
        .collect();
    indexed.sort_by(|a, b| a.0.cmp(&b.0));
    extern_hashes.sort_by(|a, b| a.0.cmp(&b.0));

    let mut hasher = blake3::Hasher::new();
    hasher.update(b"zccache-rustc-artifact-key-v1\0");
    hasher.update(context_key.0.as_bytes());
    hasher.update(b"\0");

    // Source + dependency file hashes.
    for (path_key, hash) in &indexed {
        hasher.update(path_key.as_bytes());
        hasher.update(b"\0");
        hasher.update(hash.as_bytes());
        hasher.update(b"\0");
    }

    // Extern crate content hashes.
    hasher.update(b"externs\0");
    for (name, hash) in extern_hashes.iter() {
        hasher.update(name.as_bytes());
        hasher.update(b"\0");
        hasher.update(hash.as_bytes());
        hasher.update(b"\0");
    }

    ArtifactKey(ContentHash::from_bytes(*hasher.finalize().as_bytes()))
}

/// Fold rustc env-dep values into an already-computed rustc artifact key
/// (zccache#1021).
///
/// rustc records every `env!()`/`option_env!()` read as a
/// `# env-dep:NAME[=value]` line in its dep-info. The values are compile
/// inputs exactly like extern rlib bytes: `cargo:rustc-env` vars (vergen's
/// `VERGEN_GIT_SHA` and friends) change without any argv/source change,
/// and serving the old artifact ships the stale value inside the rlib.
///
/// Layered on top of the base key rather than folded inside it so that
/// contexts with **no** recorded env-deps (the overwhelmingly common
/// case) keep byte-identical artifact keys with prior releases — no cache
/// invalidation for env-free crates.
///
/// `env_hashes` entries are `(name, value_hash)` where `None` means the
/// variable was **unset** at read time (`option_env!` → `None`) — a
/// distinct variant from every set value. Entries are sorted by name for
/// determinism.
#[must_use]
pub fn fold_rustc_env_deps_into_artifact_key(
    base: ArtifactKey,
    env_hashes: &mut [(String, Option<ContentHash>)],
) -> ArtifactKey {
    if env_hashes.is_empty() {
        return base;
    }
    env_hashes.sort_by(|a, b| a.0.cmp(&b.0));

    let mut hasher = blake3::Hasher::new();
    hasher.update(b"zccache-rustc-env-deps-v1\0");
    hasher.update(base.0.as_bytes());
    hasher.update(b"\0");
    for (name, value_hash) in env_hashes.iter() {
        hasher.update(name.as_bytes());
        hasher.update(b"\0");
        match value_hash {
            Some(h) => hasher.update(h.as_bytes()),
            None => hasher.update(b"unset"),
        };
        hasher.update(b"\0");
    }
    ArtifactKey(ContentHash::from_bytes(*hasher.finalize().as_bytes()))
}
