//! Maintenance, stats, and accessor methods for [`DepGraph`].
//!
//! Carved out of `mod.rs` to keep each file under the 1k-LOC guard.

use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use crate::core::NormalizedPath;

use super::super::context::{CompileContext, ContextKey};
use super::super::scanner::IncludeDirective;
use super::{ContextEntry, ContextState, DepGraph, DepGraphStats, FileEntry};

impl DepGraph {
    /// Clear `artifact_key` on every context whose currently-recorded
    /// artifact key is in `evicted_hex`. Returns the number of contexts
    /// whose key was cleared.
    ///
    /// **Issue #680 — eviction-divergence fix.** When the disk artifact
    /// GC (`evict_disk_artifacts`) removes an artifact, the depgraph
    /// contexts that point at the now-evicted key still report
    /// `CacheVerdict::Hit { artifact_key }` on their next check, which
    /// then surfaces in the daemon log as `artifact_not_found` and a
    /// wasted recompile (the user observed a 15.7% real hit rate on a
    /// soldr dogfood rebuild that should have been ~99%). Wiring the
    /// disk GC to call this method after each eviction batch keeps the
    /// two stores in agreement: the next check on an evicted context
    /// returns `Cold` (forcing a clean miss + re-store) instead of a
    /// stale `Hit`.
    ///
    /// Key comparison uses the hex form (`ArtifactKey::hash().to_hex()`)
    /// so callers — which already have the evicted artifacts' string
    /// keys from the disk eviction pass — do not have to round-trip
    /// through `ArtifactKey` construction.
    ///
    /// `last_accessed` is intentionally NOT bumped — this is a passive
    /// invalidation, and resetting access time would extend the
    /// context's `trim()` lifetime past its disk-evicted artifact.
    pub fn invalidate_artifact_keys(
        &self,
        evicted_hex: &std::collections::HashSet<String>,
    ) -> usize {
        if evicted_hex.is_empty() {
            return 0;
        }
        // Two-phase to avoid holding DashMap shard locks across the
        // `evicted_hex.contains(...)` allocation: read-only scan collects
        // the context keys whose artifact_key needs clearing, then a
        // bounded set of `get_mut` writes does the clear. Each `get_mut`
        // takes only the matching shard's lock briefly. This pattern is
        // what `trim()` and the existing artifact retention loops use to
        // stay deadlock-free under concurrent compile traffic.
        let mut to_clear: Vec<ContextKey> = Vec::new();
        for entry in self.contexts.iter() {
            if let Some(ref ak) = entry.value().artifact_key {
                if evicted_hex.contains(ak.hash().to_hex().as_str()) {
                    to_clear.push(*entry.key());
                }
            }
        }
        let mut cleared = 0;
        for ctx_key in &to_clear {
            if let Some(mut entry) = self.contexts.get_mut(ctx_key) {
                if entry.artifact_key.is_some() {
                    entry.artifact_key = None;
                    cleared += 1;
                }
            }
        }
        cleared
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
        commands: &[super::super::compile_commands::CompileCommand],
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
