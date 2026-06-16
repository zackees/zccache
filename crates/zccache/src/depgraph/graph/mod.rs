//! Core dependency graph.
//!
//! Two-map design:
//! - `files`: shared file nodes (one per unique path, across all contexts)
//! - `contexts`: per-compilation-context entries with resolved include lists
//!
//! The implementation is split across several files for the LOC guard:
//! - `register` — context registration (`register*`, `register_context_entry`, `is_cold`).
//! - `check` — verdict computation (`check`, `check_diagnostic`, `try_fast_hit`).
//! - `update` — post-compile include-list + artifact-key recording.
//! - `maintenance` — `trim`, `clear`, stats, accessors, `ingest_compile_commands`.

mod check;
mod maintenance;
mod register;
mod update;

use std::path::Path;
use std::sync::atomic::AtomicU64;
use std::sync::Arc;
use std::time::Instant;

use crate::core::NormalizedPath;
use crate::hash::ContentHash;
use dashmap::DashMap;

use super::context::{ArtifactKey, CompileContext, ContextKey};
use super::scanner::IncludeDirective;

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
    /// No include list yet — needs full recursive scan.
    Cold,
    /// Include list populated and believed current.
    Warm,
    /// Something changed — needs partial or full rescan.
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
    /// File hashes from the last update() — used for drift diagnostics.
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
    /// Shared file nodes: path → scanned includes.
    pub(super) files: DashMap<NormalizedPath, FileEntry>,
    /// Per-context entries: context key → include list + state.
    pub(super) contexts: DashMap<ContextKey, ContextEntry>,
    /// Rustc-only extern inputs keyed by context.
    ///
    /// These are tracked outside `ContextEntry::resolved_includes` because
    /// their paths are output-placement state. Their content hashes affect the
    /// rustc artifact key by crate name, but target-dir path prefixes must not.
    pub(super) rustc_externs: DashMap<ContextKey, Vec<(String, NormalizedPath)>>,
    /// Issue #550: cached normalize_key_path results, keyed by
    /// (path, key_root_identity). The `update()` hot loop calls
    /// `normalize_key_path` once per resolved include header (typically
    /// 200-500 entries for a C++ compile pulling `<iostream>`). The
    /// normalization itself allocates a `String` per call; caching the
    /// `Arc<str>` result lets subsequent compiles in the same daemon
    /// session reuse the work — measured at ~2 ms saved per cpp-inline
    /// cold compile after the cache is warm. Capped to bound memory.
    pub(super) path_key_cache: DashMap<PathKeyCacheKey, Arc<str>>,
    /// Stats counters.
    pub(super) checks: AtomicU64,
    pub(super) hits: AtomicU64,
    pub(super) misses: AtomicU64,
}

/// Cache key for `path_key_cache`. `(header_path, key_root_or_none)`.
/// Different `key_root` values produce different normalized output
/// (project-relative vs absolute), so the cache must scope by root.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(super) struct PathKeyCacheKey {
    pub(super) path: NormalizedPath,
    pub(super) key_root: Option<NormalizedPath>,
}

#[derive(Debug, Clone, Copy)]
pub struct ContextRegistration {
    pub key: ContextKey,
    pub rebased_from_equivalent_root: bool,
}

/// Issue #582: cached check for `ZCCACHE_PROFILE_CC_MISS` so
/// `DepGraph::update`'s sub-phase emit doesn't pay an env-lookup
/// syscall on every call. Read once on first access.
pub(super) fn depgraph_update_profile_enabled() -> bool {
    use std::sync::OnceLock;
    static FLAG: OnceLock<bool> = OnceLock::new();
    *FLAG.get_or_init(|| std::env::var_os("ZCCACHE_PROFILE_CC_MISS").is_some())
}

pub(super) fn rebase_project_path(
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

pub(super) fn collect_rustc_extern_hashes<G>(
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
pub(super) fn drifted_paths<'a, I, P>(
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
pub(super) fn format_drift_for_log(drifted: &[NormalizedPath]) -> String {
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
        // Issue #588: the path_key_cache was net-negative. Each lookup
        // allocated 4 owned objects (two NormalizedPaths) to construct
        // the DashMap key, saving only ~1 normalize_for_key allocation.
        // The diagnostic data from #584 confirmed `artifact_key_compute_ns`
        // didn't move when the cache was bypassed (#587 fast path).
        //
        // Inline normalize_key_path with zero cache overhead. The Arc<str>
        // conversion from String reuses the heap allocation — one alloc
        // per call total, vs the prior four.
        //
        // The path_key_cache field is retained for backward-compat with
        // tests and to keep the API surface stable; new calls bypass it.
        crate::depgraph::context::normalize_key_path(path, key_root).into()
    }

    /// Number of cached entries in `path_key_cache`. Test-only.
    #[cfg(test)]
    pub fn path_key_cache_len(&self) -> usize {
        self.path_key_cache.len()
    }

    pub(super) fn rustc_extern_inputs(
        &self,
        key: &ContextKey,
    ) -> Option<Vec<(String, NormalizedPath)>> {
        self.rustc_externs.get(key).map(|externs| externs.clone())
    }

    /// Construct a `DepGraph` from pre-built maps, including rustc extern inputs.
    pub(crate) fn from_maps_with_rustc_externs(
        files: DashMap<NormalizedPath, FileEntry>,
        contexts: DashMap<ContextKey, ContextEntry>,
        rustc_externs: DashMap<ContextKey, Vec<(String, NormalizedPath)>>,
    ) -> Self {
        Self {
            files,
            contexts,
            rustc_externs,
            path_key_cache: DashMap::new(),
            checks: AtomicU64::new(0),
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
        }
    }
}

impl Default for DepGraph {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests;
