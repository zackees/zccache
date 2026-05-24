//! Core dependency graph.
//!
//! Two-map design:
//! - `files`: shared file nodes (one per unique path, across all contexts)
//! - `contexts`: per-compilation-context entries with resolved include lists

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crate::core::NormalizedPath;
use crate::hash::ContentHash;
use dashmap::DashMap;

use super::context::{
    compute_artifact_key, compute_context_key, ArtifactKey, CompileContext, ContextKey,
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
    /// Stats counters.
    checks: AtomicU64,
    hits: AtomicU64,
    misses: AtomicU64,
}

#[derive(Debug, Clone, Copy)]
pub struct ContextRegistration {
    pub key: ContextKey,
    pub rebased_from_equivalent_root: bool,
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

impl DepGraph {
    /// Create a new empty dependency graph.
    #[must_use]
    pub fn new() -> Self {
        Self {
            files: DashMap::new(),
            contexts: DashMap::new(),
            checks: AtomicU64::new(0),
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
        }
    }

    /// Register a compilation context. Returns the context key.
    /// If the context already exists, returns the existing key.
    pub fn register(&self, ctx: CompileContext) -> ContextKey {
        self.register_with_root(ctx, None)
    }

    /// Register a compilation context with an optional key root used to
    /// normalize project-local paths across workspace renames.
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
        let key = compute_context_key(&ctx, key_root.as_deref());
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
        let mut file_hashes: Vec<(&Path, ContentHash)> = Vec::new();

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

        let artifact_key = compute_artifact_key(key, &mut file_hashes, entry.key_root.as_deref());

        if source_fresh {
            // Ultra-fast path: nothing changed at all.
            if entry.artifact_key == Some(artifact_key) {
                self.hits.fetch_add(1, Ordering::Relaxed);
                return CacheVerdict::Hit { artifact_key };
            }
            // Source is "fresh" by watcher but artifact key differs
            // (could be first check after update).
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
        let mut file_hashes = Vec::new();

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

        let artifact_key = compute_artifact_key(key, &mut file_hashes, entry.key_root.as_deref());

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

            // Find which files have different hashes vs last update().
            let mut drifted: Vec<String> = Vec::new();
            if !entry.last_file_hashes.is_empty() {
                let old_map: std::collections::HashMap<&Path, &ContentHash> = entry
                    .last_file_hashes
                    .iter()
                    .map(|(p, h)| (p.as_path(), h))
                    .collect();
                for (path, new_hash) in &file_hashes {
                    match old_map.get(path.as_path()) {
                        Some(old_hash) if *old_hash != new_hash => {
                            let fname = path
                                .file_name()
                                .map(|n| n.to_string_lossy().to_string())
                                .unwrap_or_else(|| path.display().to_string());
                            drifted.push(fname);
                        }
                        None => {
                            let fname = path
                                .file_name()
                                .map(|n| n.to_string_lossy().to_string())
                                .unwrap_or_else(|| path.display().to_string());
                            drifted.push(format!("{fname}(new)"));
                        }
                        _ => {} // Same hash, no drift
                    }
                }
            }

            entry.artifact_key = Some(artifact_key);
            self.hits.fetch_add(1, Ordering::Relaxed);
            let hex = &artifact_key.hash().to_hex()[..8];
            let file_count = file_hashes.len();
            let drift_info = if drifted.is_empty() {
                String::new()
            } else {
                format!(
                    ", drifted=[{}]",
                    drifted
                        .iter()
                        .take(5)
                        .cloned()
                        .collect::<Vec<_>>()
                        .join(",")
                )
            };
            entry.last_file_hashes = file_hashes;
            (
                CacheVerdict::Hit { artifact_key },
                format!(
                    "hit: artifact_key={hex} (first check after update, was={old_hex}, files={file_count}{drift_info})",
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

        let computed = compute_artifact_key(key, &mut file_hashes, entry.key_root.as_deref());

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
        let mut entry = self.contexts.get_mut(key)?;

        // Always update include lists (useful for diagnostics even if hashing fails).
        entry.resolved_includes = scan_result.resolved;
        entry.unresolved_includes = scan_result.unresolved;
        entry.has_computed_includes = scan_result.has_computed;
        entry.last_accessed = Instant::now();
        // DO NOT set state=Warm here â€” wait until all hashes succeed.

        // Compute artifact key â€” if any file is missing a hash, leave state
        // unchanged (Cold stays Cold) so check() doesn't see a Warm context
        // with no artifact key.
        let mut file_hashes = Vec::new();
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

        let artifact_key = compute_artifact_key(key, &mut file_hashes, entry.key_root.as_deref());

        // SUCCESS: all hashes computed â€” transition to Warm atomically with artifact key.
        entry.state = ContextState::Warm;
        entry.artifact_key = Some(artifact_key);
        entry.last_file_hashes = file_hashes;

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
mod tests {
    use super::*;
    use crate::core::NormalizedPath;
    use std::path::Path;

    use super::super::search_paths::IncludeSearchPaths;

    fn make_ctx(source: &str) -> CompileContext {
        CompileContext {
            source_file: NormalizedPath::from(source),
            include_search: IncludeSearchPaths::default(),
            defines: Vec::new(),
            flags: Vec::new(),
            force_includes: Vec::new(),
            unknown_flags: Vec::new(),
        }
    }

    fn always_fresh(_: &Path) -> bool {
        true
    }

    fn never_fresh(_: &Path) -> bool {
        false
    }

    fn dummy_hash(path: &Path) -> Option<ContentHash> {
        Some(crate::hash::hash_bytes(path.to_string_lossy().as_bytes()))
    }

    #[test]
    fn register_returns_consistent_key() {
        let graph = DepGraph::new();
        let ctx = make_ctx("/src/a.c");
        let k1 = graph.register(ctx.clone());
        let k2 = graph.register(ctx);
        assert_eq!(k1, k2);
    }

    #[test]
    fn cold_context_returns_cold() {
        let graph = DepGraph::new();
        let key = graph.register(make_ctx("/src/a.c"));
        let verdict = graph.check(&key, always_fresh, dummy_hash);
        assert!(matches!(verdict, CacheVerdict::Cold));
    }

    #[test]
    fn unregistered_key_returns_cold() {
        let graph = DepGraph::new();
        let ctx = make_ctx("/src/a.c");
        let key = ctx.context_key();
        let verdict = graph.check(&key, always_fresh, dummy_hash);
        assert!(matches!(verdict, CacheVerdict::Cold));
    }

    #[test]
    fn warm_context_all_fresh_returns_hit() {
        let graph = DepGraph::new();
        let key = graph.register(make_ctx("/src/a.c"));

        let scan = ScanResult {
            resolved: vec![NormalizedPath::from("/inc/b.h")],
            unresolved: Vec::new(),
            has_computed: false,
        };
        graph.update(&key, scan, dummy_hash);

        let verdict = graph.check(&key, always_fresh, dummy_hash);
        assert!(matches!(verdict, CacheVerdict::Hit { .. }));
    }

    #[test]
    fn warm_context_source_changed_returns_source_changed() {
        let graph = DepGraph::new();
        let key = graph.register(make_ctx("/src/a.c"));

        let scan = ScanResult {
            resolved: vec![NormalizedPath::from("/inc/b.h")],
            unresolved: Vec::new(),
            has_computed: false,
        };
        graph.update(&key, scan, dummy_hash);

        // Source is stale-by-watcher AND its content hash now differs from
        // the stored hash (post-fallback semantics: a header/source is
        // only "changed" if journal says stale AND the content hash also
        // moved).
        let is_fresh = |p: &Path| p != Path::new("/src/a.c");
        let changed_source_hash = |p: &Path| -> Option<ContentHash> {
            if p == Path::new("/src/a.c") {
                Some(crate::hash::hash_bytes(b"source-modified"))
            } else {
                dummy_hash(p)
            }
        };
        let verdict = graph.check(&key, is_fresh, changed_source_hash);
        assert!(matches!(verdict, CacheVerdict::SourceChanged { .. }));
    }

    #[test]
    fn warm_context_header_changed_returns_headers_changed() {
        let graph = DepGraph::new();
        let key = graph.register(make_ctx("/src/a.c"));

        let scan = ScanResult {
            resolved: vec![
                NormalizedPath::from("/inc/b.h"),
                NormalizedPath::from("/inc/c.h"),
            ],
            unresolved: Vec::new(),
            has_computed: false,
        };
        graph.update(&key, scan, dummy_hash);

        // b.h is stale-by-watcher AND its current content hash differs
        // from the stored hash (so the hash-fallback also flags it).
        let is_fresh = |p: &Path| p != Path::new("/inc/b.h");
        let changed_b_hash = |p: &Path| -> Option<ContentHash> {
            if p == Path::new("/inc/b.h") {
                Some(crate::hash::hash_bytes(b"b-modified"))
            } else {
                dummy_hash(p)
            }
        };
        let verdict = graph.check(&key, is_fresh, changed_b_hash);
        match verdict {
            CacheVerdict::HeadersChanged { changed } => {
                assert_eq!(changed, vec![NormalizedPath::from("/inc/b.h")]);
            }
            other => panic!("expected HeadersChanged, got {other:?}"),
        }
    }

    #[test]
    fn warm_context_header_stale_by_watcher_but_hash_unchanged_returns_hit() {
        // Regression guard for the journal-cold-after-restart fix:
        // an empty in-memory journal post-restart makes `is_fresh` return
        // false for every path, but if the content hash still matches the
        // stored one we must treat the file as fresh-by-content. Before
        // the fix, every cached header was reported as HeadersChanged and
        // every Warm context degraded to a miss on the warm side of the
        // cold-tar-untar-warm perf scenario.
        let graph = DepGraph::new();
        let key = graph.register(make_ctx("/src/a.c"));

        let scan = ScanResult {
            resolved: vec![NormalizedPath::from("/inc/b.h")],
            unresolved: Vec::new(),
            has_computed: false,
        };
        graph.update(&key, scan, dummy_hash);

        // Journal claims b.h has changed (it's never been seen), but
        // dummy_hash returns the same hash for the same path — so the
        // content didn't actually change.
        let verdict = graph.check(&key, never_fresh, dummy_hash);
        assert!(matches!(verdict, CacheVerdict::Hit { .. }));
    }

    #[test]
    fn computed_includes_returns_needs_preprocessor() {
        let graph = DepGraph::new();
        let key = graph.register(make_ctx("/src/a.c"));

        let scan = ScanResult {
            resolved: vec![NormalizedPath::from("/inc/b.h")],
            unresolved: Vec::new(),
            has_computed: true,
        };
        graph.update(&key, scan, dummy_hash);

        let verdict = graph.check(&key, always_fresh, dummy_hash);
        assert!(matches!(verdict, CacheVerdict::NeedsPreprocessor));
    }

    #[test]
    fn show_includes_enables_cache_hit_after_computed() {
        // Simulates the MSVC /showIncludes optimization:
        // 1. First update from scanner: has_computed=true â†’ NeedsPreprocessor
        // 2. Second update from /showIncludes: has_computed=false â†’ Hit
        let graph = DepGraph::new();
        let key = graph.register(make_ctx("/src/a.c"));

        // Scanner found #include MACRO â†’ has_computed=true
        let scanner_scan = ScanResult {
            resolved: vec![NormalizedPath::from("/inc/known.h")],
            unresolved: Vec::new(),
            has_computed: true,
        };
        graph.update(&key, scanner_scan, dummy_hash);

        let verdict = graph.check(&key, always_fresh, dummy_hash);
        assert!(matches!(verdict, CacheVerdict::NeedsPreprocessor));

        // /showIncludes resolved all includes â†’ has_computed=false
        let depfile_scan = ScanResult {
            resolved: vec![
                NormalizedPath::from("/inc/known.h"),
                NormalizedPath::from("/inc/macro_resolved.h"),
            ],
            unresolved: Vec::new(),
            has_computed: false,
        };
        graph.update(&key, depfile_scan, dummy_hash);

        // Now should be a hit.
        let verdict = graph.check(&key, always_fresh, dummy_hash);
        assert!(
            matches!(verdict, CacheVerdict::Hit { .. }),
            "expected Hit after /showIncludes update, got {verdict:?}"
        );
    }

    #[test]
    fn update_sets_warm_state() {
        let graph = DepGraph::new();
        let key = graph.register(make_ctx("/src/a.c"));
        assert_eq!(graph.get_state(&key), Some(ContextState::Cold));

        let scan = ScanResult {
            resolved: Vec::new(),
            unresolved: Vec::new(),
            has_computed: false,
        };
        graph.update(&key, scan, dummy_hash);
        assert_eq!(graph.get_state(&key), Some(ContextState::Warm));
    }

    #[test]
    fn header_change_sets_stale_state() {
        let graph = DepGraph::new();
        let key = graph.register(make_ctx("/src/a.c"));

        let scan = ScanResult {
            resolved: vec![NormalizedPath::from("/h.h")],
            unresolved: Vec::new(),
            has_computed: false,
        };
        graph.update(&key, scan, dummy_hash);
        assert_eq!(graph.get_state(&key), Some(ContextState::Warm));

        // Both the watcher AND the content hash say h.h changed — the
        // hash-fallback can't rescue this one, so the verdict is
        // HeadersChanged and the entry flips to Stale.
        let changed_h_hash = |p: &Path| -> Option<ContentHash> {
            if p == Path::new("/h.h") {
                Some(crate::hash::hash_bytes(b"h-modified"))
            } else {
                dummy_hash(p)
            }
        };
        graph.check(&key, never_fresh, changed_h_hash);
        assert_eq!(graph.get_state(&key), Some(ContextState::Stale));
    }

    #[test]
    fn trim_removes_old_entries() {
        let graph = DepGraph::new();
        let key = graph.register(make_ctx("/src/a.c"));

        let scan = ScanResult {
            resolved: Vec::new(),
            unresolved: Vec::new(),
            has_computed: false,
        };
        graph.update(&key, scan, dummy_hash);

        // Sleep briefly so the entry's last_accessed is older than Duration::ZERO.
        std::thread::sleep(Duration::from_millis(5));

        // Trim with max_age=0: everything not accessed this exact instant is removed.
        let removed = graph.trim(Duration::ZERO);
        assert_eq!(removed, 1);
        assert_eq!(graph.stats().context_count, 0);
    }

    #[test]
    fn trim_keeps_recent_entries() {
        let graph = DepGraph::new();
        graph.register(make_ctx("/src/a.c"));
        let removed = graph.trim(Duration::from_secs(60));
        assert_eq!(removed, 0);
        assert_eq!(graph.stats().context_count, 1);
    }

    #[test]
    fn stats_track_checks_and_hits() {
        let graph = DepGraph::new();
        let key = graph.register(make_ctx("/src/a.c"));

        let scan = ScanResult {
            resolved: Vec::new(),
            unresolved: Vec::new(),
            has_computed: false,
        };
        graph.update(&key, scan, dummy_hash);

        graph.check(&key, always_fresh, dummy_hash);
        graph.check(&key, always_fresh, dummy_hash);

        let stats = graph.stats();
        assert_eq!(stats.checks, 2);
        assert_eq!(stats.hits, 2);
        assert_eq!(stats.misses, 0);
        assert_eq!(stats.context_count, 1);
    }

    #[test]
    fn artifact_key_changes_when_hash_changes() {
        let graph = DepGraph::new();
        let key = graph.register(make_ctx("/src/a.c"));

        let scan = ScanResult {
            resolved: Vec::new(),
            unresolved: Vec::new(),
            has_computed: false,
        };

        let hash_v1 = |_: &Path| Some(crate::hash::hash_bytes(b"v1"));
        let ak1 = graph.update(&key, scan.clone(), hash_v1).unwrap();

        let hash_v2 = |_: &Path| Some(crate::hash::hash_bytes(b"v2"));
        let ak2 = graph.update(&key, scan, hash_v2).unwrap();

        assert_ne!(ak1, ak2);
    }

    #[test]
    fn store_and_get_file_includes() {
        let graph = DepGraph::new();
        let path = NormalizedPath::from("/src/foo.h");
        let includes = vec![super::super::IncludeDirective {
            kind: super::super::IncludeKind::Quoted,
            path: "bar.h".to_string(),
            line: 1,
        }];

        graph.store_file_includes(path.clone(), includes.clone());
        let retrieved = graph.get_file_includes(&path).unwrap();
        assert_eq!(retrieved.len(), 1);
        assert_eq!(retrieved[0].path, "bar.h");
    }

    #[test]
    fn concurrent_register_and_check() {
        use std::sync::Arc;
        use std::thread;

        let graph = Arc::new(DepGraph::new());
        let mut handles = Vec::new();

        // 4 threads registering and checking.
        for t in 0..4 {
            let graph = Arc::clone(&graph);
            handles.push(thread::spawn(move || {
                for i in 0..50 {
                    let ctx = make_ctx(&format!("/src/t{t}_f{i}.c"));
                    let key = graph.register(ctx);

                    let scan = ScanResult {
                        resolved: vec![NormalizedPath::from(format!("/inc/t{t}_h{i}.h"))],
                        unresolved: Vec::new(),
                        has_computed: false,
                    };
                    graph.update(&key, scan, dummy_hash);
                    graph.check(&key, always_fresh, dummy_hash);
                }
            }));
        }

        for h in handles {
            h.join().expect("thread panicked");
        }

        let stats = graph.stats();
        assert_eq!(stats.context_count, 200); // 4 * 50
        assert_eq!(stats.checks, 200);
    }

    #[test]
    fn ingest_compile_commands_registers_contexts() {
        let json = r#"[
            {
                "directory": "/build",
                "command": "g++ -I/project/include -DNDEBUG -std=c++17 -c /project/src/main.cpp -o main.o",
                "file": "/project/src/main.cpp"
            },
            {
                "directory": "/build",
                "command": "g++ -I/project/include -DNDEBUG -std=c++17 -c /project/src/util.cpp -o util.o",
                "file": "/project/src/util.cpp"
            }
        ]"#;

        let commands = super::super::compile_commands::parse_compile_commands_json(json).unwrap();
        let graph = DepGraph::new();
        let system_includes = vec![NormalizedPath::from("/usr/include")];
        let keys = graph.ingest_compile_commands(&commands, &system_includes);

        assert_eq!(keys.len(), 2);
        assert_eq!(graph.stats().context_count, 2);

        // All contexts should be Cold (not yet scanned).
        for key in &keys {
            assert_eq!(graph.get_state(key), Some(ContextState::Cold));
        }
    }

    #[test]
    fn ingest_merges_system_includes() {
        let json = r#"[
            {
                "directory": "/build",
                "command": "g++ -isystem /explicit/system -c /src/main.cpp",
                "file": "/src/main.cpp"
            }
        ]"#;

        let commands = super::super::compile_commands::parse_compile_commands_json(json).unwrap();
        let graph = DepGraph::new();
        let system_includes = vec![NormalizedPath::from("/usr/include")];
        let keys = graph.ingest_compile_commands(&commands, &system_includes);

        assert_eq!(keys.len(), 1);

        // The context should have both the explicit and system includes.
        // We can verify by checking the context key differs with/without system includes.
        let keys_no_sys = graph.ingest_compile_commands(&commands, &[]);

        // Same source + different system includes = different context keys.
        // Wait, ingest re-uses existing contexts if key matches.
        // Since system includes affect the context key, these should differ.
        // But we already registered the first one, so let's check differently.
        // The first call added /usr/include to system paths, so the key
        // incorporates it. A second call with empty system_includes would
        // produce a different key.
        assert_ne!(keys[0], keys_no_sys[0]);
    }

    #[test]
    fn ingest_deduplicates_system_includes() {
        let json = r#"[
            {
                "directory": "/build",
                "command": "g++ -isystem /usr/include -c /src/main.cpp",
                "file": "/src/main.cpp"
            }
        ]"#;

        let commands = super::super::compile_commands::parse_compile_commands_json(json).unwrap();
        let graph = DepGraph::new();
        // /usr/include is already in -isystem, should not be added twice.
        let system_includes = vec![NormalizedPath::from("/usr/include")];
        let keys = graph.ingest_compile_commands(&commands, &system_includes);
        assert_eq!(keys.len(), 1);
    }

    #[test]
    fn clear_resets_everything() {
        let graph = DepGraph::new();
        let key = graph.register(make_ctx("/src/a.c"));

        let scan = ScanResult {
            resolved: vec![NormalizedPath::from("/inc/b.h")],
            unresolved: Vec::new(),
            has_computed: false,
        };
        graph.update(&key, scan, dummy_hash);
        graph.check(&key, always_fresh, dummy_hash);

        let stats_before = graph.stats();
        assert!(stats_before.context_count > 0);
        assert!(stats_before.checks > 0);
        assert!(stats_before.hits > 0);

        graph.clear();

        let stats_after = graph.stats();
        assert_eq!(stats_after.context_count, 0);
        assert_eq!(stats_after.file_count, 0);
        assert_eq!(stats_after.checks, 0);
        assert_eq!(stats_after.hits, 0);
        assert_eq!(stats_after.misses, 0);
    }

    #[test]
    fn mark_stale_changes_state() {
        let graph = DepGraph::new();
        let key = graph.register(make_ctx("/src/a.c"));

        let scan = ScanResult {
            resolved: Vec::new(),
            unresolved: Vec::new(),
            has_computed: false,
        };
        graph.update(&key, scan, dummy_hash);
        assert_eq!(graph.get_state(&key), Some(ContextState::Warm));

        assert!(graph.mark_stale(&key));
        assert_eq!(graph.get_state(&key), Some(ContextState::Stale));
    }

    // â”€â”€ update() atomicity tests â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€

    #[test]
    fn update_with_hash_failure_stays_cold() {
        let graph = DepGraph::new();
        let key = graph.register(make_ctx("/src/a.c"));
        assert_eq!(graph.get_state(&key), Some(ContextState::Cold));

        let scan = ScanResult {
            resolved: vec![NormalizedPath::from("/inc/b.h")],
            unresolved: Vec::new(),
            has_computed: false,
        };
        // Source hash fails â†’ update returns None, state must stay Cold.
        let no_hash = |_: &Path| -> Option<ContentHash> { None };
        let result = graph.update(&key, scan, no_hash);
        assert!(result.is_none());
        assert_eq!(graph.get_state(&key), Some(ContextState::Cold));
    }

    #[test]
    fn update_partial_hash_failure_stays_cold() {
        let graph = DepGraph::new();
        let key = graph.register(make_ctx("/src/a.c"));

        let scan = ScanResult {
            resolved: vec![
                NormalizedPath::from("/inc/a.h"),
                NormalizedPath::from("/inc/b.h"),
                NormalizedPath::from("/inc/c.h"),
            ],
            unresolved: Vec::new(),
            has_computed: false,
        };
        // 2nd header hash fails â†’ state must stay Cold.
        let partial_hash = |p: &Path| -> Option<ContentHash> {
            if p == Path::new("/inc/b.h") {
                None
            } else {
                Some(crate::hash::hash_bytes(p.to_string_lossy().as_bytes()))
            }
        };
        let result = graph.update(&key, scan, partial_hash);
        assert!(result.is_none());
        assert_eq!(graph.get_state(&key), Some(ContextState::Cold));
    }

    #[test]
    fn update_success_transitions_to_warm() {
        let graph = DepGraph::new();
        let key = graph.register(make_ctx("/src/a.c"));
        assert_eq!(graph.get_state(&key), Some(ContextState::Cold));

        let scan = ScanResult {
            resolved: vec![NormalizedPath::from("/inc/b.h")],
            unresolved: Vec::new(),
            has_computed: false,
        };
        let result = graph.update(&key, scan, dummy_hash);
        assert!(result.is_some());
        assert_eq!(graph.get_state(&key), Some(ContextState::Warm));
    }

    #[test]
    fn pch_gen_context_hit_after_update() {
        // Register a PCH-generation context (no force_includes â€” it IS the PCH).
        let graph = DepGraph::new();
        let key = graph.register(make_ctx("/src/pch.h"));

        let scan = ScanResult {
            resolved: vec![
                NormalizedPath::from("/inc/a.h"),
                NormalizedPath::from("/inc/b.h"),
            ],
            unresolved: Vec::new(),
            has_computed: false,
        };
        graph.update(&key, scan, dummy_hash);

        // check() should return Hit, not Cold.
        let verdict = graph.check(&key, always_fresh, dummy_hash);
        assert!(
            matches!(verdict, CacheVerdict::Hit { .. }),
            "expected Hit after update, got {verdict:?}"
        );
    }

    #[test]
    fn warm_context_with_no_artifact_returns_cold_on_check() {
        // Simulate the bug scenario: state=Warm but artifact_key=None.
        // With the fix, this can't happen via update() â€” but if someone
        // manually sets state=Warm, check_diagnostic should handle it.
        let graph = DepGraph::new();
        let ctx = make_ctx("/src/a.c");
        let key = ctx.context_key();

        // Manually insert a Warm entry with no artifact key.
        graph.contexts.insert(
            key,
            ContextEntry {
                context: ctx,
                key_root: None,
                resolved_includes: vec![NormalizedPath::from("/inc/b.h")],
                unresolved_includes: Vec::new(),
                has_computed_includes: false,
                artifact_key: None,
                last_file_hashes: Vec::new(),
                last_accessed: Instant::now(),
                state: ContextState::Warm,
            },
        );

        // check_diagnostic should still produce a valid verdict (not panic).
        // With all fresh, it should compute an artifact key and return Hit.
        let (verdict, _reason) = graph.check_diagnostic(&key, always_fresh, dummy_hash);
        assert!(
            matches!(
                verdict,
                CacheVerdict::Hit { .. } | CacheVerdict::SourceChanged { .. }
            ),
            "warm context with all hashes available should hit, got {verdict:?}"
        );
    }

    #[test]
    fn trim_preserves_force_include_files() {
        let graph = DepGraph::new();

        // Create a context with a force-include (PCH file).
        let mut ctx = make_ctx("/src/a.c");
        ctx.force_includes = vec![NormalizedPath::from("/pch/precompiled.h")];
        let key = graph.register(ctx);

        let scan = ScanResult {
            resolved: vec![NormalizedPath::from("/inc/b.h")],
            unresolved: Vec::new(),
            has_computed: false,
        };
        graph.update(&key, scan, dummy_hash);

        // Populate the files map for both the force-include and resolved include.
        let empty_includes = vec![super::super::IncludeDirective {
            kind: super::super::IncludeKind::Quoted,
            path: "stdafx.h".to_string(),
            line: 1,
        }];
        graph.store_file_includes(
            NormalizedPath::from("/pch/precompiled.h"),
            empty_includes.clone(),
        );
        graph.store_file_includes(NormalizedPath::from("/inc/b.h"), empty_includes);

        // Also add an unreferenced file that should be evicted.
        graph.store_file_includes(
            NormalizedPath::from("/stale/old.h"),
            vec![super::super::IncludeDirective {
                kind: super::super::IncludeKind::Quoted,
                path: "gone.h".to_string(),
                line: 1,
            }],
        );

        assert_eq!(graph.stats().file_count, 3);

        // Trim with a long max_age â€” no contexts should be removed.
        let removed = graph.trim(Duration::from_secs(3600));
        assert_eq!(removed, 0);

        // The force-included PCH file must still be in the files map.
        assert!(
            graph
                .get_file_includes(&NormalizedPath::from("/pch/precompiled.h"))
                .is_some(),
            "force-included PCH file should not be evicted by trim"
        );
        // Regular includes should also be preserved.
        assert!(
            graph
                .get_file_includes(&NormalizedPath::from("/inc/b.h"))
                .is_some(),
            "resolved include should not be evicted by trim"
        );
        // Unreferenced file should be evicted.
        assert!(
            graph
                .get_file_includes(&NormalizedPath::from("/stale/old.h"))
                .is_none(),
            "unreferenced file should be evicted by trim"
        );
        assert_eq!(graph.stats().file_count, 2);
    }
}
