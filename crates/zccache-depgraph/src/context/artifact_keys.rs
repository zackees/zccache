//! Generic C/C++ artifact-key computation.

use std::path::Path;
use std::sync::Arc;

use zccache_hash::ContentHash;

use super::{normalize_key_path, ArtifactKey, ContextKey};

/// Compute the artifact key from a context key and file content hashes.
///
/// The artifact key uniquely identifies a specific compilation output.
/// `file_hashes` should contain `(path, content_hash)` pairs for the
/// source file and all resolved headers, sorted by path.
pub fn compute_artifact_key<P: AsRef<Path> + Ord>(
    context_key: &ContextKey,
    file_hashes: &mut [(P, ContentHash)],
    key_root: Option<&Path>,
) -> ArtifactKey {
    compute_artifact_key_with(context_key, file_hashes, key_root, |path, key_root| {
        normalize_key_path(path, key_root).into()
    })
}

/// Like [`compute_artifact_key`] but the per-header path normalization
/// is delegated to a caller-supplied closure. The daemon plumbs in a
/// closure that consults `DepGraph::cached_normalize_key_path` so the
/// per-compile allocations amortize across the daemon's lifetime
/// (issue #550). The closure is `FnMut` only because the impl forwards
/// it through the iterator; in practice it's a `Fn`-shaped lookup.
/// Fast-path variant of [`compute_artifact_key_with`] for the common case
/// where the caller already holds owned [`zccache_core::NormalizedPath`] values and has
/// no `key_root` (the cc/cpp compile path without `-ffile-prefix-map`).
///
/// Issue #585: post-#576 each [`zccache_core::NormalizedPath`] caches its
/// `normalize_for_key` result in the struct's `key` field. With no
/// `key_root`, that cached key IS the bytes we want to hash — no
/// allocation, no DashMap lookup, no closure call. The previous shape
/// went through `cached_normalize_key_path` which allocated 4 owned
/// objects per lookup just to construct the `DashMap` key.
///
/// Output is bit-identical to `compute_artifact_key_with` when called
/// with `key_root: None` and a closure that returns
/// `normalize_for_key(path).into()`.
#[must_use]
pub fn compute_artifact_key_normalized_inplace(
    context_key: &ContextKey,
    file_hashes: &mut [(zccache_core::NormalizedPath, ContentHash)],
) -> ArtifactKey {
    compute_artifact_key_normalized_with_root(context_key, file_hashes, None)
}

/// Issue #591: extension of [`compute_artifact_key_normalized_inplace`]
/// that also handles `key_root: Some`. For paths NOT under `key_root`
/// (the common case for system headers), the path-key bytes are just
/// `NormalizedPath::case_key()` — no allocation. For paths under
/// `key_root`, we fall back to `normalize_key_path(path, Some(root))`.
///
/// Replaces the closure-based slow path through
/// `compute_artifact_key_with` + `cached_normalize_key_path` which
/// allocated 1 String per entry even after #590's cache bypass.
#[must_use]
pub fn compute_artifact_key_normalized_with_root(
    context_key: &ContextKey,
    file_hashes: &[(zccache_core::NormalizedPath, ContentHash)],
    key_root: Option<&Path>,
) -> ArtifactKey {
    use std::borrow::Cow;

    // Materialize path-keys: borrow from NormalizedPath::key for paths
    // not under key_root (zero alloc); compute fresh for project-local.
    let mut indexed: Vec<(Cow<'_, str>, ContentHash)> = file_hashes
        .iter()
        .map(|(np, h)| {
            let path_key: Cow<'_, str> = match key_root {
                Some(root) if np.as_path().starts_with(root) => {
                    Cow::Owned(normalize_key_path(np.as_path(), Some(root)))
                }
                _ => {
                    #[expect(
                        clippy::expect_used,
                        reason = "NormalizedPath::case_key() returns Option only as a forward-compat marker; post-#576 it is always Some. Returning a fallback would silently corrupt cache keys, which is catastrophic per CLAUDE.md correctness model."
                    )]
                    let key = np
                        .case_key()
                        .expect("NormalizedPath::key is always populated post-#576");
                    Cow::Borrowed(key)
                }
            };
            (path_key, *h)
        })
        .collect();
    indexed.sort_by(|a, b| a.0.cmp(&b.0));

    let mut hasher = blake3::Hasher::new();
    hasher.update(b"zccache-artifact-key-v1\0");
    hasher.update(context_key.0.as_bytes());
    hasher.update(b"\0");

    for (path_key, hash) in &indexed {
        hasher.update(path_key.as_bytes());
        hasher.update(b"\0");
        hasher.update(hash.as_bytes());
        hasher.update(b"\0");
    }

    ArtifactKey(ContentHash::from_bytes(*hasher.finalize().as_bytes()))
}

pub fn compute_artifact_key_with<P, F>(
    context_key: &ContextKey,
    file_hashes: &mut [(P, ContentHash)],
    key_root: Option<&Path>,
    mut normalize: F,
) -> ArtifactKey
where
    P: AsRef<Path> + Ord,
    F: FnMut(&Path, Option<&Path>) -> Arc<str>,
{
    // Issue #571: pre-normalize each path once (O(n) via the cached
    // closure), then sort by the cheap Arc<str> key (O(n log n) byte
    // compares). The prior path called `NormalizedPath::cmp` inside
    // `sort_by`, which invoked `normalize_for_key` on BOTH operands of
    // every comparison — O(n log n) normalizations bypassed the
    // #553 cache entirely. With ~600 transitive headers per cpp/rust
    // compile, that was ~10k normalize_for_key calls per miss; this
    // collapses to ~600 calls (most hit the cache after the first
    // compile in a session) plus cheap byte compares.
    //
    // Hash output is bit-identical to the prior path: the sort order
    // is determined by the same normalized path-keys, and the blake3
    // input bytes (path-key, separator, content-hash, separator) are
    // emitted in the same order.
    let mut indexed: Vec<(Arc<str>, ContentHash)> = file_hashes
        .iter()
        .map(|(p, h)| (normalize(p.as_ref(), key_root), *h))
        .collect();
    indexed.sort_by(|a, b| a.0.cmp(&b.0));

    let mut hasher = blake3::Hasher::new();
    hasher.update(b"zccache-artifact-key-v1\0");
    hasher.update(context_key.0.as_bytes());
    hasher.update(b"\0");

    for (path_key, hash) in &indexed {
        hasher.update(path_key.as_bytes());
        hasher.update(b"\0");
        hasher.update(hash.as_bytes());
        hasher.update(b"\0");
    }

    ArtifactKey(ContentHash::from_bytes(*hasher.finalize().as_bytes()))
}
