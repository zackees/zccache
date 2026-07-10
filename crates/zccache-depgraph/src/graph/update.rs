//! `DepGraph::update` — record full include list after a compile.
//!
//! Carved out of `mod.rs` to keep each file under the 1k-LOC guard.

use std::time::Instant;

use super::super::context::{
    compute_rustc_artifact_key_with_root_with, fold_rustc_env_deps_into_artifact_key, ArtifactKey,
    ContextKey,
};
use super::super::scanner::ScanResult;
use super::{
    collect_rustc_extern_hashes, depgraph_update_profile_enabled, hash_env_dep_value, ContextState,
    DepGraph,
};
use zccache_hash::ContentHash;

impl DepGraph {
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
        G: Fn(&std::path::Path) -> Option<ContentHash>,
    {
        self.update_with_env(key, scan_result, get_hash, &[], |_| None)
    }

    /// [`Self::update`] with the rustc env-dep set scanned from dep-info
    /// (zccache#1021).
    ///
    /// `env_dep_names` are the env variable names rustc recorded as
    /// `# env-dep:` lines for this compile; `env_value` resolves the value
    /// each had in the environment the compile ran under. The snapshot
    /// `(name, value_hash)` is stored per context so check paths can fold
    /// CURRENT values into the artifact key, and the key stored here is
    /// folded the same way so both sides agree.
    pub fn update_with_env<G, E>(
        &self,
        key: &ContextKey,
        scan_result: ScanResult,
        get_hash: G,
        env_dep_names: &[String],
        env_value: E,
    ) -> Option<ArtifactKey>
    where
        G: Fn(&std::path::Path) -> Option<ContentHash>,
        E: Fn(&str) -> Option<String>,
    {
        // Snapshot the env-dep values the compile actually saw, replacing
        // any prior snapshot (the name set can change across compiles).
        let mut env_hashes: Vec<(String, Option<ContentHash>)> = env_dep_names
            .iter()
            .map(|name| {
                let value = env_value(name);
                (name.clone(), hash_env_dep_value(value.as_deref()))
            })
            .collect();
        self.set_rustc_env_deps(*key, env_hashes.clone());
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
        // DO NOT set state=Warm here — wait until all hashes succeed.

        // Compute artifact key — if any file is missing a hash, leave state
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
                None => return None, // Incomplete hashes → state stays unchanged
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
            let base = compute_rustc_artifact_key_with_root_with(
                key,
                &mut file_hashes,
                &mut extern_hashes,
                entry.key_root.as_deref(),
                |path, key_root| self.cached_normalize_key_path(path, key_root),
            );
            fold_rustc_env_deps_into_artifact_key(base, &mut env_hashes)
        } else {
            // Issue #591: closure-free path for cc/cpp. For paths NOT
            // under `key_root` (system headers — the common case),
            // `NormalizedPath::case_key()` is the answer with zero
            // allocation. For paths under `key_root` we compute fresh
            // via `normalize_key_path(path, Some(root))`. Handles both
            // `key_root: None` (was #585's compute_artifact_key_normalized_inplace)
            // and `key_root: Some` in one shape.
            crate::context::compute_artifact_key_normalized_with_root(
                key,
                &file_hashes,
                entry.key_root.as_deref(),
            )
        };
        let artifact_key_compute_ns = t_artifact_key
            .map(|t| t.elapsed().as_nanos() as u64)
            .unwrap_or(0);

        let t_finalize = profile_enabled.then(Instant::now);
        // SUCCESS: all hashes computed — transition to Warm atomically with artifact key.
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
}
