//! Cache verdict computation: `check`, `check_diagnostic`, `try_fast_hit`.
//!
//! Carved out of `mod.rs` to keep each file under the 1k-LOC guard.

use std::path::Path;
use std::sync::atomic::Ordering;
use std::time::Instant;

use crate::core::NormalizedPath;
use crate::hash::ContentHash;

use super::super::context::{
    compute_artifact_key_with, compute_rustc_artifact_key_with_root_with, ArtifactKey, ContextKey,
};
use super::{
    collect_rustc_extern_hashes, drifted_paths, format_drift_for_log, CacheVerdict, ContextState,
    DepGraph,
};

impl DepGraph {
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

        // Build file_hashes using references — zero NormalizedPath clones.
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
}
