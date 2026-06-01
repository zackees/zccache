//! Core dependency graph.
//!
//! Two-map design:
//! - `files`: shared file nodes (one per unique path, across all contexts)
//! - `contexts`: per-compilation-context entries with resolved include lists

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::core::NormalizedPath;
use crate::hash::ContentHash;
use dashmap::DashMap;

use super::context::{
    compute_artifact_key_with, compute_context_key_with, compute_rustc_artifact_key_with_root_with,
    ArtifactKey, CompileContext, ContextKey,
};
use super::scanner::{IncludeDirective, ScanResult};

/// A file node in the graph. Shared across all contexts.
#[derive(Debug, Clone)]
pub struct FileEntry {
    /// Raw `#include` directives found in this file.
    pub includes: Vec<IncludeDirective>,
    /// When this file was last scanned for includes.
    pub scanned_at: Instant,
}

/// State of a compilation context in the graph.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContextState {
    /// No include list yet â€” needs full recursive scan.
    Cold,
    /// Include list populated and believed current.
    Warm,
    /// Something changed â€” needs partial or full rescan.
    Stale,
}

/// A compilation context entry in the graph.
#[derive(Debug, Clone)]
pub struct ContextEntry {
    /// The compilation context (source + flags).
    pub context: CompileContext,
    /// Optional root used to normalize project-local paths in cache keys.
    pub key_root: Option<NormalizedPath>,
    /// Flat list of all transitive resolved headers (absolute paths).
    pub resolved_includes: Vec<NormalizedPath>,
    /// Include names that could not be resolved to any file.
    pub unresolved_includes: Vec<String>,
    /// True if any `#include MACRO` was found during scanning.
    pub has_computed_includes: bool,
    /// Last computed artifact key.
    pub artifact_key: Option<ArtifactKey>,
    /// File hashes from the last update() â€” used for drift diagnostics.
    pub last_file_hashes: Vec<(NormalizedPath, ContentHash)>,
    /// When this entry was last accessed (for trimming).
    pub last_accessed: Instant,
    /// Current state.
    pub state: ContextState,
}

/// Result of checking a context against the file cache.
#[derive(Debug, Clone)]
pub enum CacheVerdict {
    /// All files fresh, artifact key valid. Use cached object.
    Hit { artifact_key: ArtifactKey },
    /// Source changed but headers are fresh. New artifact key computed.
    SourceChanged { artifact_key: ArtifactKey },
    /// One or more headers changed. Rescan needed.
    HeadersChanged { changed: Vec<NormalizedPath> },
    /// No include list yet. Full scan required.
    Cold,
    /// Contains `#include MACRO`. Needs preprocessor fallback.
    NeedsPreprocessor,
}

/// Statistics about the dependency graph.
#[derive(Debug, Clone)]
pub struct DepGraphStats {
    /// Number of unique files tracked.
    pub file_count: usize,
    /// Number of compilation contexts tracked.
    pub context_count: usize,
    /// Number of check() calls.
    pub checks: u64,
    /// Number of cache hits (ultra-fast + fast path).
    pub hits: u64,
    /// Number of cache misses.
    pub misses: u64,
}

/// The core dependency graph.
impl std::fmt::Debug for DepGraph {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DepGraph")
            .field("files", &self.files.len())
            .field("contexts", &self.contexts.len())
            .finish()
    }
}

pub struct DepGraph {
    /// Shared file nodes: path â†’ scanned includes.
    files: DashMap<NormalizedPath, FileEntry>,
    /// Per-context entries: context key â†’ include list + state.
    contexts: DashMap<ContextKey, ContextEntry>,
    /// Rustc-only extern inputs keyed by context.
    ///
    /// These are tracked outside `ContextEntry::resolved_includes` because
    /// their paths are output-placement state. Their content hashes affect the
    /// rustc artifact key by crate name, but target-dir path prefixes must not.
    rustc_externs: DashMap<ContextKey, Vec<(String, NormalizedPath)>>,
    /// Issue #550: cached normalize_key_path results, keyed by
    /// (path, key_root_identity). The `update()` hot loop calls
    /// `normalize_key_path` once per resolved include header (typically
    /// 200-500 entries for a C++ compile pulling `<iostream>`). The
    /// normalization itself allocates a `String` per call; caching the
    /// `Arc<str>` result lets subsequent compiles in the same daemon
    /// session reuse the work — measured at ~2 ms saved per cpp-inline
    /// cold compile after the cache is warm. Capped to bound memory.
    path_key_cache: DashMap<PathKeyCacheKey, Arc<str>>,
    /// Stats counters.
    checks: AtomicU64,
    hits: AtomicU64,
    misses: AtomicU64,
}

/// Cache key for `path_key_cache`. `(header_path, key_root_or_none)`.
/// Different `key_root` values produce different normalized output
/// (project-relative vs absolute), so the cache must scope by root.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct PathKeyCacheKey {
    path: NormalizedPath,
    key_root: Option<NormalizedPath>,
}

/// Cap on `path_key_cache` size. ~150 bytes per entry × 32k = ~5 MB.
/// Beyond this, new entries are silently dropped (still served via
/// uncached recomputation). Cap is reset by [`DepGraph::clear`].
const PATH_KEY_CACHE_MAX_ENTRIES: usize = 32_768;

#[derive(Debug, Clone, Copy)]
pub struct ContextRegistration {
    pub key: ContextKey,
    pub rebased_from_equivalent_root: bool,
}

/// Issue #582: cached check for `ZCCACHE_PROFILE_CC_MISS` so
/// `DepGraph::update`'s sub-phase emit doesn't pay an env-lookup
/// syscall on every call. Read once on first access.
fn depgraph_update_profile_enabled() -> bool {
    use std::sync::OnceLock;
    static FLAG: OnceLock<bool> = OnceLock::new();
    *FLAG.get_or_init(|| std::env::var_os("ZCCACHE_PROFILE_CC_MISS").is_some())
}

fn rebase_project_path(
    path: &NormalizedPath,
    old_root: Option<&NormalizedPath>,
    new_root: Option<&NormalizedPath>,
) -> NormalizedPath {
    match (old_root, new_root) {
        (Some(old_root), Some(new_root)) => path
            .strip_prefix(old_root)
            .map(|relative| new_root.join(relative))
            .unwrap_or_else(|_| path.clone()),
        _ => path.clone(),
    }
}

fn collect_rustc_extern_hashes<G>(
    rustc_externs: &[(String, NormalizedPath)],
    get_hash: &G,
) -> Option<Vec<(String, ContentHash)>>
where
    G: Fn(&Path) -> Option<ContentHash>,
{
    let mut extern_hashes = Vec::with_capacity(rustc_externs.len());
    for (name, path) in rustc_externs {
        extern_hashes.push((name.clone(), get_hash(path)?));
    }
    Some(extern_hashes)
}

/// Files whose content hash has drifted relative to the hashes captured by
/// the most recent `update()` — either a tracked path now reports a
/// different hash, or a path appears in the current set that wasn't in
/// `last_file_hashes` (membership shifted, e.g. a new transitive include).
///
/// Returns an empty `Vec` when `last_file_hashes` is empty (the entry has
/// never been hashed) so callers can distinguish "no drift signal
/// available" from "drift confirmed clean."
///
/// Issue #449: even when the journal misses a watcher event and reports
/// the file as fresh, a hash mismatch here is the ground-truth signal
/// that the source state moved — and that the cached
/// `entry.resolved_includes` set may no longer reflect the file's
/// transitive deps. Returning the drifted paths to the caller lets
/// `check` / `check_diagnostic` downgrade what would otherwise have been
/// a `Hit` with a stale-prediction `artifact_key` into a
/// `HeadersChanged` verdict that forces a fresh scan.
fn drifted_paths<'a, I, P>(
    last_file_hashes: &[(NormalizedPath, ContentHash)],
    current: I,
) -> Vec<NormalizedPath>
where
    I: IntoIterator<Item = (P, &'a ContentHash)>,
    P: AsRef<Path>,
{
    if last_file_hashes.is_empty() {
        return Vec::new();
    }
    let old_map: std::collections::HashMap<&Path, &ContentHash> = last_file_hashes
        .iter()
        .map(|(p, h)| (p.as_path(), h))
        .collect();
    let mut drifted: Vec<NormalizedPath> = Vec::new();
    for (path, new_hash) in current {
        let p = path.as_ref();
        match old_map.get(p) {
            Some(old_hash) if old_hash != &new_hash => {
                drifted.push(p.into());
            }
            None => {
                // Membership drift: a path that wasn't in last_file_hashes
                // is now present. Treat as a changed dependency.
                drifted.push(p.into());
            }
            _ => {}
        }
    }
    drifted
}

/// Format a drift list for the diagnostic session log: at most 5 file
/// names, comma-separated, prefixed with `, drifted=[…]`. Empty input
/// yields an empty string so the caller can splat it into a `format!`
/// without conditional logic.
fn format_drift_for_log(drifted: &[NormalizedPath]) -> String {
    if drifted.is_empty() {
        return String::new();
    }
    let names: Vec<String> = drifted
        .iter()
        .take(5)
        .map(|p| {
            p.file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| p.display().to_string())
        })
        .collect();
    format!(", drifted=[{}]", names.join(","))
}

impl DepGraph {
    /// Create a new empty dependency graph.
    #[must_use]
    pub fn new() -> Self {
        Self {
            files: DashMap::new(),
            contexts: DashMap::new(),
            rustc_externs: DashMap::new(),
            path_key_cache: DashMap::new(),
            checks: AtomicU64::new(0),
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
        }
    }

    /// Cached version of [`crate::depgraph::context::normalize_key_path`].
    ///
    /// Looks up `(path, key_root)` in `path_key_cache`. On hit, returns
    /// the cached `Arc<str>` without re-running the underlying normalization.
    /// On miss, computes via `normalize_key_path` and inserts (subject to
    /// the `PATH_KEY_CACHE_MAX_ENTRIES` cap — past the cap, the result is
    /// returned without caching so memory stays bounded).
    ///
    /// Issue #550 — the `compute_artifact_key` hot loop's per-header
    /// allocation hotspot.
    pub fn cached_normalize_key_path(&self, path: &Path, key_root: Option<&Path>) -> Arc<str> {
        let cache_key = PathKeyCacheKey {
            path: NormalizedPath::new(path),
            key_root: key_root.map(NormalizedPath::new),
        };
        if let Some(cached) = self.path_key_cache.get(&cache_key) {
            return cached.clone();
        }
        let computed: Arc<str> =
            crate::depgraph::context::normalize_key_path(path, key_root).into();
        if self.path_key_cache.len() < PATH_KEY_CACHE_MAX_ENTRIES {
            self.path_key_cache.insert(cache_key, computed.clone());
        }
        computed
    }

    /// Number of cached entries in `path_key_cache`. Test-only.
    #[cfg(test)]
    pub fn path_key_cache_len(&self) -> usize {
        self.path_key_cache.len()
    }

    /// Register a compilation context. Returns the context key.
    /// If the context already exists, returns the existing key.
    pub fn register(&self, ctx: CompileContext) -> ContextKey {
        self.register_with_root(ctx, None)
    }

    /// Register a compilation context with an optional key root used to
    /// normalize project-local paths across workspace renames.
    /// Variant of [`Self::register_with_root`] that folds an optional
    /// `worktree_salt` into the context key (issue #474). Used by the
    /// multi-file compile path when `keys::requires_worktree_in_key` is
    /// true for the unit. Returns only the resulting [`ContextKey`].
    pub fn register_with_root_and_salt(
        &self,
        ctx: CompileContext,
        key_root: Option<NormalizedPath>,
        worktree_salt: Option<&Path>,
    ) -> ContextKey {
        self.register_with_root_and_salt_result(ctx, key_root, worktree_salt)
            .key
    }

    pub fn register_with_root(
        &self,
        ctx: CompileContext,
        key_root: Option<NormalizedPath>,
    ) -> ContextKey {
        self.register_with_root_result(ctx, key_root).key
    }

    pub fn register_with_root_result(
        &self,
        ctx: CompileContext,
        key_root: Option<NormalizedPath>,
    ) -> ContextRegistration {
        self.register_with_root_and_salt_result(ctx, key_root, None)
    }

    /// Issue #474: variant of [`Self::register_with_root_result`] that folds an
    /// optional `worktree_salt` into the context key. Used by the C/C++
    /// compile pipeline when `keys::requires_worktree_in_key` returns true
    /// (PCH builds + MSVC), so the resulting cache entry is scoped to one
    /// worktree and can't be served to a sibling clone whose embedded
    /// paths would diverge from the artifact's.
    pub fn register_with_root_and_salt_result(
        &self,
        ctx: CompileContext,
        key_root: Option<NormalizedPath>,
        worktree_salt: Option<&Path>,
    ) -> ContextRegistration {
        let key =
            compute_context_key_with(&ctx, key_root.as_deref(), worktree_salt, |path, root| {
                self.cached_normalize_key_path(path, root)
            });
        self.register_with_key_and_root_result(key, ctx, key_root)
    }

    /// Register a compilation context with a precomputed key.
    ///
    /// Used for Rustc compilations where the context key is computed from
    /// `RustcCompileContext` (different domain tag) but the dep_graph stores
    /// a `CompileContext` with the source file path for freshness checks.
    pub fn register_with_key(&self, key: ContextKey, ctx: CompileContext) -> ContextKey {
        self.register_with_key_and_root(key, ctx, None)
    }

    pub fn register_with_key_and_root(
        &self,
        key: ContextKey,
        ctx: CompileContext,
        key_root: Option<NormalizedPath>,
    ) -> ContextKey {
        self.register_with_key_and_root_result(key, ctx, key_root)
            .key
    }

    pub fn register_with_key_and_root_result(
        &self,
        key: ContextKey,
        ctx: CompileContext,
        key_root: Option<NormalizedPath>,
    ) -> ContextRegistration {
        let registration = self.register_context_entry(key, ctx, key_root);
        self.rustc_externs.remove(&registration.key);
        registration
    }

    /// Register a rustc context with its current `--extern` file inputs.
    ///
    /// Rustc context keys already reduce extern path prefixes to filename
    /// identity. The dependency graph keeps the actual extern paths here only
    /// for hashing/freshness; artifact keys incorporate them by crate name.
    pub fn register_rustc_with_key_and_root_result(
        &self,
        key: ContextKey,
        ctx: CompileContext,
        key_root: Option<NormalizedPath>,
        externs: Vec<(String, NormalizedPath)>,
    ) -> ContextRegistration {
        let registration = self.register_context_entry(key, ctx, key_root);
        self.rustc_externs.insert(registration.key, externs);
        registration
    }

    fn register_context_entry(
        &self,
        key: ContextKey,
        ctx: CompileContext,
        key_root: Option<NormalizedPath>,
    ) -> ContextRegistration {
        let mut rebased_from_equivalent_root = false;
        self.contexts
            .entry(key)
            .and_modify(|entry| {
                if entry.context.source_file != ctx.source_file || entry.key_root != key_root {
                    let old_root = entry.key_root.clone();
                    rebased_from_equivalent_root =
                        old_root.is_some() && key_root.is_some() && old_root != key_root;
                    entry.resolved_includes = entry
                        .resolved_includes
                        .iter()
                        .map(|path| rebase_project_path(path, old_root.as_ref(), key_root.as_ref()))
                        .collect();
                    entry.last_file_hashes = entry
                        .last_file_hashes
                        .iter()
                        .map(|(path, hash)| {
                            (
                                rebase_project_path(path, old_root.as_ref(), key_root.as_ref()),
                                *hash,
                            )
                        })
                        .collect();
                    entry.context = ctx.clone();
                    entry.key_root = key_root.clone();
                }
                entry.last_accessed = Instant::now();
            })
            .or_insert_with(|| ContextEntry {
                context: ctx,
                key_root,
                resolved_includes: Vec::new(),
                unresolved_includes: Vec::new(),
                has_computed_includes: false,
                artifact_key: None,
                last_file_hashes: Vec::new(),
                last_accessed: Instant::now(),
                state: ContextState::Cold,
            });

        ContextRegistration {
            key,
            rebased_from_equivalent_root,
        }
    }

    fn rustc_extern_inputs(&self, key: &ContextKey) -> Option<Vec<(String, NormalizedPath)>> {
        self.rustc_externs.get(key).map(|externs| externs.clone())
    }

    /// Returns `true` if the context has never been updated (no artifact key).
    /// Used by the server to skip pre-compile hashing on cold contexts where
    /// `check_diagnostic` would return `Cold` without examining any hashes.
    #[must_use]
    pub fn is_cold(&self, key: &ContextKey) -> bool {
        match self.contexts.get(key) {
            Some(entry) => entry.state == ContextState::Cold,
            None => true,
        }
    }

    /// Check if a compilation can use cached output.
    ///
    /// `is_fresh` is called for each file path. It should query Layer 1
    /// (fscache) and return `true` if the file has not changed since last
    /// known state.
    ///
    /// `get_hash` retrieves the content hash for a file from Layer 1.
    pub fn check<F, G>(&self, key: &ContextKey, is_fresh: F, get_hash: G) -> CacheVerdict
    where
        F: Fn(&Path) -> bool,
        G: Fn(&Path) -> Option<ContentHash>,
    {
        self.checks.fetch_add(1, Ordering::Relaxed);

        let rustc_externs = self.rustc_extern_inputs(key);
        let mut entry = match self.contexts.get_mut(key) {
            Some(e) => e,
            None => {
                self.misses.fetch_add(1, Ordering::Relaxed);
                return CacheVerdict::Cold;
            }
        };

        entry.last_accessed = Instant::now();

        if entry.state == ContextState::Cold {
            self.misses.fetch_add(1, Ordering::Relaxed);
            return CacheVerdict::Cold;
        }

        if entry.has_computed_includes {
            self.misses.fetch_add(1, Ordering::Relaxed);
            return CacheVerdict::NeedsPreprocessor;
        }

        // Helper: a file is fresh if the journal hasn't seen it change
        // since `since` OR — when the journal has no opinion (post-restart
        // cold journal, the watcher dropped events, etc.) — if its current
        // content hash matches the hash we stored at last `update()`.
        // The journal is in-memory and starts empty after every daemon
        // restart; without this fallback, every cached header reports
        // "changed" and every Warm context degrades to HeadersChanged.
        let fresh_or_hash_match = |path: &NormalizedPath| -> bool {
            if is_fresh(path) {
                return true;
            }
            let current = match get_hash(path) {
                Some(h) => h,
                None => return false,
            };
            entry
                .last_file_hashes
                .iter()
                .any(|(p, h)| p == path && *h == current)
        };

        // Check source file freshness.
        let source_fresh = fresh_or_hash_match(&entry.context.source_file);

        // Check all headers.
        let mut changed_headers = Vec::new();
        for header in &entry.resolved_includes {
            if !fresh_or_hash_match(header) {
                changed_headers.push(header.clone());
            }
        }
        // Also check force-included files (PCH, -include).
        for fi in &entry.context.force_includes {
            if !fresh_or_hash_match(fi) {
                changed_headers.push(fi.clone());
            }
        }

        if !changed_headers.is_empty() {
            self.misses.fetch_add(1, Ordering::Relaxed);
            entry.state = ContextState::Stale;
            return CacheVerdict::HeadersChanged {
                changed: changed_headers,
            };
        }

        // All headers fresh. Compute artifact key (using &Path to avoid NormalizedPath clones).
        // Issue #578: pre-size to avoid the ~10 reallocations that grow-from-zero
        // triggers for a typical 600-header cpp compile.
        let mut file_hashes: Vec<(&Path, ContentHash)> = Vec::with_capacity(
            1 + entry.resolved_includes.len() + entry.context.force_includes.len(),
        );

        if let Some(h) = get_hash(&entry.context.source_file) {
            file_hashes.push((&entry.context.source_file, h));
        } else {
            self.misses.fetch_add(1, Ordering::Relaxed);
            return CacheVerdict::Cold;
        }

        for header in &entry.resolved_includes {
            if let Some(h) = get_hash(header) {
                file_hashes.push((header, h));
            } else {
                self.misses.fetch_add(1, Ordering::Relaxed);
                return CacheVerdict::Cold;
            }
        }
        // Hash force-included files (PCH content must affect artifact key).
        for fi in &entry.context.force_includes {
            if let Some(h) = get_hash(fi) {
                file_hashes.push((fi, h));
            } else {
                self.misses.fetch_add(1, Ordering::Relaxed);
                return CacheVerdict::Cold;
            }
        }

        let artifact_key = if let Some(externs) = rustc_externs.as_deref() {
            let Some(mut extern_hashes) = collect_rustc_extern_hashes(externs, &get_hash) else {
                self.misses.fetch_add(1, Ordering::Relaxed);
                return CacheVerdict::Cold;
            };
            compute_rustc_artifact_key_with_root_with(
                key,
                &mut file_hashes,
                &mut extern_hashes,
                entry.key_root.as_deref(),
                |path, key_root| self.cached_normalize_key_path(path, key_root),
            )
        } else {
            compute_artifact_key_with(
                key,
                &mut file_hashes,
                entry.key_root.as_deref(),
                |path, key_root| self.cached_normalize_key_path(path, key_root),
            )
        };

        if source_fresh {
            // Ultra-fast path: nothing changed at all.
            if entry.artifact_key == Some(artifact_key) {
                self.hits.fetch_add(1, Ordering::Relaxed);
                return CacheVerdict::Hit { artifact_key };
            }

            // Source is fresh-by-journal but the recomputed artifact key
            // doesn't match the one stored at last `update()`. Either a
            // file's hash drifted (watcher missed the event but the
            // content actually changed) or the `entry.artifact_key` was
            // unset to start with (`warm_context_with_no_artifact`).
            //
            // Issue #449: when a header file has drifted, the include set
            // it transitively pulls in may have shifted too — but
            // `entry.resolved_includes` still reflects the OLD set from
            // the previous `update()`. The read-side `artifact_key`
            // derived from that stale set is not a trustworthy lookup key
            // for the *current* source state, even though by blake3 a
            // matching stored artifact would have been compiled from the
            // same byte-stream set. Force a recompile + re-scan so the
            // depgraph refreshes `resolved_includes` and the artifact
            // gets stored under the write-side key derived from the
            // post-compile dependency set.
            let drifted = drifted_paths(
                &entry.last_file_hashes,
                file_hashes.iter().map(|(p, h)| (*p, h)),
            );
            if !drifted.is_empty() {
                self.misses.fetch_add(1, Ordering::Relaxed);
                entry.state = ContextState::Stale;
                return CacheVerdict::HeadersChanged { changed: drifted };
            }

            // No drift detected (e.g., first check after a warm context
            // with no stored artifact_key, or last_file_hashes empty):
            // record the new key and hit.
            entry.artifact_key = Some(artifact_key);
            self.hits.fetch_add(1, Ordering::Relaxed);
            CacheVerdict::Hit { artifact_key }
        } else {
            // Fast path: only source changed, headers all fresh.
            entry.artifact_key = Some(artifact_key);
            self.hits.fetch_add(1, Ordering::Relaxed);
            CacheVerdict::SourceChanged { artifact_key }
        }
    }

    /// Check if a compilation can use cached output, with diagnostic reason.
    ///
    /// Same logic as [`check()`](Self::check) but returns a reason string
    /// explaining why the verdict was reached (useful for session logs).
    pub fn check_diagnostic<F, G>(
        &self,
        key: &ContextKey,
        is_fresh: F,
        get_hash: G,
    ) -> (CacheVerdict, String)
    where
        F: Fn(&Path) -> bool,
        G: Fn(&Path) -> Option<ContentHash>,
    {
        self.checks.fetch_add(1, Ordering::Relaxed);

        let rustc_externs = self.rustc_extern_inputs(key);
        let mut entry = match self.contexts.get_mut(key) {
            Some(e) => e,
            None => {
                self.misses.fetch_add(1, Ordering::Relaxed);
                return (CacheVerdict::Cold, "context_key not registered".to_string());
            }
        };

        entry.last_accessed = Instant::now();

        if entry.state == ContextState::Cold {
            self.misses.fetch_add(1, Ordering::Relaxed);
            return (
                CacheVerdict::Cold,
                "context never updated (state=Cold)".to_string(),
            );
        }

        if entry.has_computed_includes {
            self.misses.fetch_add(1, Ordering::Relaxed);
            return (
                CacheVerdict::NeedsPreprocessor,
                "has computed includes, needs preprocessor".to_string(),
            );
        }

        // See `check()` above for the rationale — content-hash fallback
        // catches the post-restart empty-journal case where every header
        // would otherwise look "changed".
        let fresh_or_hash_match = |path: &NormalizedPath| -> bool {
            if is_fresh(path) {
                return true;
            }
            let current = match get_hash(path) {
                Some(h) => h,
                None => return false,
            };
            entry
                .last_file_hashes
                .iter()
                .any(|(p, h)| p == path && *h == current)
        };

        // Check source file freshness.
        let source_fresh = fresh_or_hash_match(&entry.context.source_file);

        // Check all headers.
        let mut changed_headers = Vec::new();
        for header in &entry.resolved_includes {
            if !fresh_or_hash_match(header) {
                changed_headers.push(header.clone());
            }
        }
        // Also check force-included files (PCH, -include).
        for fi in &entry.context.force_includes {
            if !fresh_or_hash_match(fi) {
                changed_headers.push(fi.clone());
            }
        }

        if !changed_headers.is_empty() {
            self.misses.fetch_add(1, Ordering::Relaxed);
            entry.state = ContextState::Stale;
            let names: Vec<String> = changed_headers
                .iter()
                .map(|p| p.display().to_string())
                .collect();
            return (
                CacheVerdict::HeadersChanged {
                    changed: changed_headers,
                },
                format!("headers changed: [{}]", names.join(", ")),
            );
        }

        // All headers fresh. Compute artifact key.
        // Issue #578: pre-size to avoid grow-from-zero reallocations.
        let mut file_hashes = Vec::with_capacity(
            1 + entry.resolved_includes.len() + entry.context.force_includes.len(),
        );

        if let Some(h) = get_hash(&entry.context.source_file) {
            file_hashes.push((entry.context.source_file.clone(), h));
        } else {
            self.misses.fetch_add(1, Ordering::Relaxed);
            return (
                CacheVerdict::Cold,
                format!(
                    "source hash missing: {}",
                    entry.context.source_file.display()
                ),
            );
        }

        for header in &entry.resolved_includes {
            if let Some(h) = get_hash(header) {
                file_hashes.push((header.clone(), h));
            } else {
                self.misses.fetch_add(1, Ordering::Relaxed);
                return (
                    CacheVerdict::Cold,
                    format!("header hash missing: {}", header.display()),
                );
            }
        }
        // Hash force-included files (PCH content must affect artifact key).
        for fi in &entry.context.force_includes {
            if let Some(h) = get_hash(fi) {
                file_hashes.push((fi.clone(), h));
            } else {
                self.misses.fetch_add(1, Ordering::Relaxed);
                return (
                    CacheVerdict::Cold,
                    format!("force-include hash missing: {}", fi.display()),
                );
            }
        }

        let artifact_key = if let Some(externs) = rustc_externs.as_deref() {
            let Some(mut extern_hashes) = collect_rustc_extern_hashes(externs, &get_hash) else {
                self.misses.fetch_add(1, Ordering::Relaxed);
                return (CacheVerdict::Cold, "rustc extern hash missing".to_string());
            };
            compute_rustc_artifact_key_with_root_with(
                key,
                &mut file_hashes,
                &mut extern_hashes,
                entry.key_root.as_deref(),
                |path, key_root| self.cached_normalize_key_path(path, key_root),
            )
        } else {
            compute_artifact_key_with(
                key,
                &mut file_hashes,
                entry.key_root.as_deref(),
                |path, key_root| self.cached_normalize_key_path(path, key_root),
            )
        };

        if source_fresh {
            if entry.artifact_key == Some(artifact_key) {
                self.hits.fetch_add(1, Ordering::Relaxed);
                let hex = &artifact_key.hash().to_hex()[..8];
                return (
                    CacheVerdict::Hit { artifact_key },
                    format!("hit: artifact_key={hex}"),
                );
            }
            // Source is "fresh" by watcher but artifact key differs.
            let old_hex = entry
                .artifact_key
                .as_ref()
                .map(|k| k.hash().to_hex()[..8].to_string())
                .unwrap_or_else(|| "none".to_string());

            let drifted = drifted_paths(
                &entry.last_file_hashes,
                file_hashes.iter().map(|(p, h)| (p, h)),
            );
            let file_count = file_hashes.len();

            // Issue #449: header (or source) content drifted under a
            // journal that still claims everything is fresh. The read-side
            // `artifact_key` is derived from `entry.resolved_includes`
            // which captures the *previous* compile's transitive include
            // set — that set may no longer match the current source state
            // (the changed header could have gained or lost an
            // `#include`). Returning `Hit` here would point the pipeline
            // at a predicted key that, when it coincidentally matches a
            // stored artifact, can serve a `.obj` whose compile didn't
            // actually use the current source state. Force the pipeline
            // through the cold-miss path: recompile, re-scan, re-store
            // under the write-side key derived from the post-compile
            // dependency set.
            if !drifted.is_empty() {
                self.misses.fetch_add(1, Ordering::Relaxed);
                entry.state = ContextState::Stale;
                let drift_info = format_drift_for_log(&drifted);
                return (
                    CacheVerdict::HeadersChanged { changed: drifted },
                    format!(
                        "drift: was={old_hex}, files={file_count}{drift_info} \
                         (journal reported fresh; recompile forced to refresh \
                         resolved_includes and store under the write-side key)"
                    ),
                );
            }

            // No drift detected (e.g., warm context with no previously
            // stored artifact_key, or `last_file_hashes` empty). Adopt
            // the new artifact_key and return Hit so a subsequent check
            // takes the ultra-fast path.
            entry.artifact_key = Some(artifact_key);
            self.hits.fetch_add(1, Ordering::Relaxed);
            let hex = &artifact_key.hash().to_hex()[..8];
            entry.last_file_hashes = file_hashes;
            (
                CacheVerdict::Hit { artifact_key },
                format!(
                    "hit: artifact_key={hex} (first check after update, was={old_hex}, files={file_count})",
                ),
            )
        } else {
            entry.artifact_key = Some(artifact_key);
            self.hits.fetch_add(1, Ordering::Relaxed);
            (
                CacheVerdict::SourceChanged { artifact_key },
                "source content changed".to_string(),
            )
        }
    }

    /// Fast-path artifact key check: recompute the key from caller-provided
    /// hashes and compare against the stored key.  Returns `Some(key)` when
    /// they match (common cache-hit case), `None` otherwise.
    ///
    /// Compared to `check_diagnostic`, this method:
    /// - Uses a **shared** DashMap read (no write lock)
    /// - Skips redundant per-file journal freshness checks (caller already
    ///   stat-verified every file during the hash phase)
    /// - Avoids `NormalizedPath` clones by working with references into the entry
    ///
    /// Call this *after* hashing and *before* `check_diagnostic`.  On `None`,
    /// fall back to the full `check_diagnostic` for miss-reason diagnostics.
    pub fn try_fast_hit<G>(&self, key: &ContextKey, get_hash: G) -> Option<ArtifactKey>
    where
        G: Fn(&Path) -> Option<ContentHash>,
    {
        let rustc_externs = self.rustc_extern_inputs(key);
        let entry = self.contexts.get(key)?;

        if entry.state == ContextState::Cold || entry.has_computed_includes {
            return None;
        }

        let stored_key = entry.artifact_key.as_ref()?;

        // Build file_hashes using references â€” zero NormalizedPath clones.
        let cap = 1 + entry.resolved_includes.len() + entry.context.force_includes.len();
        let mut file_hashes: Vec<(&Path, ContentHash)> = Vec::with_capacity(cap);

        file_hashes.push((
            &entry.context.source_file,
            get_hash(&entry.context.source_file)?,
        ));
        for header in &entry.resolved_includes {
            file_hashes.push((header.as_path(), get_hash(header)?));
        }
        for fi in &entry.context.force_includes {
            file_hashes.push((fi.as_path(), get_hash(fi)?));
        }

        let computed = if let Some(externs) = rustc_externs.as_deref() {
            let mut extern_hashes = collect_rustc_extern_hashes(externs, &get_hash)?;
            compute_rustc_artifact_key_with_root_with(
                key,
                &mut file_hashes,
                &mut extern_hashes,
                entry.key_root.as_deref(),
                |path, key_root| self.cached_normalize_key_path(path, key_root),
            )
        } else {
            compute_artifact_key_with(
                key,
                &mut file_hashes,
                entry.key_root.as_deref(),
                |path, key_root| self.cached_normalize_key_path(path, key_root),
            )
        };

        if computed == *stored_key {
            self.hits.fetch_add(1, Ordering::Relaxed);
            Some(computed)
        } else {
            None
        }
    }

    /// After a compile (or on cold path), record the full include list.
    ///
    /// `get_hash` retrieves the content hash for a file from Layer 1.
    pub fn update<G>(
        &self,
        key: &ContextKey,
        scan_result: ScanResult,
        get_hash: G,
    ) -> Option<ArtifactKey>
    where
        G: Fn(&Path) -> Option<ContentHash>,
    {
        // Issue #582: emit a `zccache_depgraph_update_breakdown` line when
        // `ZCCACHE_PROFILE_CC_MISS` is set so the next perf iteration has
        // sub-phase data for the remaining ~2.4 ms mean `depgraph_update_ns`.
        // Cached env-check to avoid per-call syscall.
        let profile_enabled = depgraph_update_profile_enabled();
        let t_total = profile_enabled.then(Instant::now);

        let t_entry = profile_enabled.then(Instant::now);
        let rustc_externs = self.rustc_extern_inputs(key);
        let mut entry = self.contexts.get_mut(key)?;
        let entry_get_ns = t_entry.map(|t| t.elapsed().as_nanos() as u64).unwrap_or(0);

        // Always update include lists (useful for diagnostics even if hashing fails).
        entry.resolved_includes = scan_result.resolved;
        entry.unresolved_includes = scan_result.unresolved;
        entry.has_computed_includes = scan_result.has_computed;
        entry.last_accessed = Instant::now();
        // DO NOT set state=Warm here â€” wait until all hashes succeed.

        // Compute artifact key â€” if any file is missing a hash, leave state
        // unchanged (Cold stays Cold) so check() doesn't see a Warm context
        // with no artifact key.
        //
        // Issue #578: pre-size to avoid the grow-from-zero reallocations
        // (~10 for a typical 600-header cpp compile).
        let t_file_hashes = profile_enabled.then(Instant::now);
        let mut file_hashes = Vec::with_capacity(
            1 + entry.resolved_includes.len() + entry.context.force_includes.len(),
        );
        let source_hash = get_hash(&entry.context.source_file)?;
        file_hashes.push((entry.context.source_file.clone(), source_hash));

        for header in &entry.resolved_includes {
            match get_hash(header) {
                Some(h) => file_hashes.push((header.clone(), h)),
                None => return None, // Incomplete hashes â†’ state stays unchanged
            }
        }
        // Hash force-included files (PCH content must affect artifact key).
        for fi in &entry.context.force_includes {
            match get_hash(fi) {
                Some(h) => file_hashes.push((fi.clone(), h)),
                None => return None,
            }
        }
        let file_hashes_build_ns = t_file_hashes
            .map(|t| t.elapsed().as_nanos() as u64)
            .unwrap_or(0);

        let t_artifact_key = profile_enabled.then(Instant::now);
        let artifact_key = if let Some(externs) = rustc_externs.as_deref() {
            let mut extern_hashes = collect_rustc_extern_hashes(externs, &get_hash)?;
            compute_rustc_artifact_key_with_root_with(
                key,
                &mut file_hashes,
                &mut extern_hashes,
                entry.key_root.as_deref(),
                |path, key_root| self.cached_normalize_key_path(path, key_root),
            )
        } else if entry.key_root.is_none() {
            // Issue #585: fast path — when there's no key_root, the
            // path-key bytes ARE NormalizedPath::key (populated since #576).
            // Skip the cached_normalize_key_path indirection (which
            // allocates 4 owned objects per lookup just to build the
            // DashMap key) and use the in-struct cache directly.
            crate::depgraph::context::compute_artifact_key_normalized_inplace(key, &mut file_hashes)
        } else {
            compute_artifact_key_with(
                key,
                &mut file_hashes,
                entry.key_root.as_deref(),
                |path, key_root| self.cached_normalize_key_path(path, key_root),
            )
        };
        let artifact_key_compute_ns = t_artifact_key
            .map(|t| t.elapsed().as_nanos() as u64)
            .unwrap_or(0);

        let t_finalize = profile_enabled.then(Instant::now);
        // SUCCESS: all hashes computed â€” transition to Warm atomically with artifact key.
        entry.state = ContextState::Warm;
        entry.artifact_key = Some(artifact_key);
        entry.last_file_hashes = file_hashes;
        let finalize_ns = t_finalize
            .map(|t| t.elapsed().as_nanos() as u64)
            .unwrap_or(0);

        if let Some(t) = t_total {
            let total_ns = t.elapsed().as_nanos() as u64;
            let resolved = entry.resolved_includes.len();
            let force = entry.context.force_includes.len();
            // Drop the entry guard before printing to avoid holding the
            // DashMap write-lock across stderr I/O.
            drop(entry);
            eprintln!(
                "zccache_depgraph_update_breakdown total_ns={total_ns} \
                 entry_get_ns={entry_get_ns} file_hashes_build_ns={file_hashes_build_ns} \
                 artifact_key_compute_ns={artifact_key_compute_ns} \
                 finalize_ns={finalize_ns} resolved_count={resolved} \
                 force_includes_count={force}"
            );
        }

        Some(artifact_key)
    }

    /// Trim entries not accessed within the given duration.
    /// Returns the number of entries removed.
    pub fn trim(&self, max_age: Duration) -> usize {
        let now = Instant::now();
        let mut removed = 0;

        self.contexts.retain(|_, entry| {
            // Use saturating_duration_since to avoid panic if Instant is
            // non-monotonic (documented edge case on some platforms/VMs).
            if now.saturating_duration_since(entry.last_accessed) > max_age {
                removed += 1;
                false
            } else {
                true
            }
        });
        self.rustc_externs
            .retain(|key, _| self.contexts.contains_key(key));

        // Also trim file entries not referenced by any context.
        let referenced: std::collections::HashSet<NormalizedPath> = self
            .contexts
            .iter()
            .flat_map(
                |entry: dashmap::mapref::multiple::RefMulti<'_, ContextKey, ContextEntry>| {
                    let mut paths = entry.value().resolved_includes.clone();
                    paths.push(entry.value().context.source_file.clone());
                    for fi in &entry.value().context.force_includes {
                        paths.push(fi.clone());
                    }
                    paths
                },
            )
            .collect();

        self.files.retain(|path, _| referenced.contains(path));

        removed
    }

    /// Clear all graph state: files, contexts, and stats counters.
    pub fn clear(&self) {
        self.files.clear();
        self.contexts.clear();
        self.rustc_externs.clear();
        self.path_key_cache.clear();
        self.checks.store(0, Ordering::Relaxed);
        self.hits.store(0, Ordering::Relaxed);
        self.misses.store(0, Ordering::Relaxed);
    }

    /// Get statistics about the graph.
    #[must_use]
    pub fn stats(&self) -> DepGraphStats {
        DepGraphStats {
            file_count: self.files.len(),
            context_count: self.contexts.len(),
            checks: self.checks.load(Ordering::Relaxed),
            hits: self.hits.load(Ordering::Relaxed),
            misses: self.misses.load(Ordering::Relaxed),
        }
    }

    /// Get the state of a context entry.
    #[must_use]
    pub fn get_state(&self, key: &ContextKey) -> Option<ContextState> {
        self.contexts.get(key).map(|e| e.state)
    }

    /// Count contexts by state. Returned as `(cold, warm, stale)`.
    ///
    /// Used by the daemon's depgraph save / load logging to diagnose
    /// post-save / post-load state distribution — specifically to find
    /// out whether contexts are getting persisted as Warm (so `is_cold`
    /// returns `false` after restore, enabling the cache lookup path)
    /// or as Cold (so every warm-side compile takes the `cold_skip`
    /// branch and misses regardless of artifact-store state).
    #[must_use]
    pub fn state_breakdown(&self) -> (usize, usize, usize) {
        let mut cold = 0usize;
        let mut warm = 0usize;
        let mut stale = 0usize;
        for entry in self.contexts.iter() {
            match entry.value().state {
                ContextState::Cold => cold += 1,
                ContextState::Warm => warm += 1,
                ContextState::Stale => stale += 1,
            }
        }
        (cold, warm, stale)
    }

    /// Number of contexts whose `artifact_key` is set. Combined with
    /// `state_breakdown()` this distinguishes contexts that have a
    /// computed key (a successful prior compile) from contexts that
    /// were registered but never reached a Warm state.
    #[must_use]
    pub fn contexts_with_artifact_key(&self) -> usize {
        self.contexts
            .iter()
            .filter(|e| e.value().artifact_key.is_some())
            .count()
    }

    /// Get the resolved includes for a context.
    #[must_use]
    pub fn get_includes(&self, key: &ContextKey) -> Option<Vec<NormalizedPath>> {
        self.contexts.get(key).map(|e| e.resolved_includes.clone())
    }

    /// Get rustc extern input paths for a context.
    #[must_use]
    pub fn get_rustc_externs(&self, key: &ContextKey) -> Option<Vec<(String, NormalizedPath)>> {
        self.rustc_extern_inputs(key)
    }

    /// Store scanned includes for a file (shared file node).
    pub fn store_file_includes(&self, path: NormalizedPath, includes: Vec<IncludeDirective>) {
        self.files.insert(
            path,
            FileEntry {
                includes,
                scanned_at: Instant::now(),
            },
        );
    }

    /// Get scanned includes for a file.
    #[must_use]
    pub fn get_file_includes(&self, path: &NormalizedPath) -> Option<Vec<IncludeDirective>> {
        self.files.get(path).map(|e| e.includes.clone())
    }

    /// Iterate over all context entries.
    pub(crate) fn contexts_iter(&self) -> dashmap::iter::Iter<'_, ContextKey, ContextEntry> {
        self.contexts.iter()
    }

    /// Iterate over all file entries.
    pub(crate) fn files_iter(&self) -> dashmap::iter::Iter<'_, NormalizedPath, FileEntry> {
        self.files.iter()
    }

    /// Construct a `DepGraph` from pre-built maps (for deserialization).
    pub(crate) fn from_maps(
        files: DashMap<NormalizedPath, FileEntry>,
        contexts: DashMap<ContextKey, ContextEntry>,
    ) -> Self {
        Self {
            files,
            contexts,
            rustc_externs: DashMap::new(),
            path_key_cache: DashMap::new(),
            checks: AtomicU64::new(0),
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
        }
    }

    /// Mark a context as stale, requiring rescan on next check.
    /// Returns `true` if the context existed and was marked stale.
    pub fn mark_stale(&self, key: &ContextKey) -> bool {
        if let Some(mut entry) = self.contexts.get_mut(key) {
            entry.state = ContextState::Stale;
            true
        } else {
            false
        }
    }

    /// Bulk-populate contexts from parsed compile commands.
    ///
    /// For each command, parses the arguments, builds a `CompileContext`
    /// (merging in the provided system include paths), and registers it.
    /// Returns the context keys for all successfully registered entries.
    pub fn ingest_compile_commands(
        &self,
        commands: &[super::compile_commands::CompileCommand],
        system_includes: &[NormalizedPath],
    ) -> Vec<ContextKey> {
        commands
            .iter()
            .map(|cmd| {
                let parsed = cmd.parse();
                let mut ctx = CompileContext::from_parsed_args(parsed);

                // Merge system includes into the context's search paths.
                // These go into the `system` field, appended after any
                // explicit -isystem paths.
                for path in system_includes {
                    if !ctx.include_search.system.contains(path) {
                        ctx.include_search.system.push(path.clone());
                    }
                }

                self.register(ctx)
            })
            .collect()
    }
}

impl Default for DepGraph {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests;
