//! `Request::Clear` handler: wipes every in-memory cache, every on-disk
//! artifact, and the metadata/depgraph snapshots.

use super::*;

/// Handle a Clear request: wipe all caches and reset stats.
pub(super) async fn handle_clear(state: &SharedState) -> Response {
    // Snapshot counts before clearing.
    let artifacts_removed = {
        let count = state.artifacts.len() as u64;
        state.artifacts.clear();
        count
    };
    let metadata_cleared = state.cache_system.metadata().len() as u64;
    let dep_graph_contexts_cleared = state.dep_graph.load().stats().context_count as u64;

    // Calculate on-disk artifact size before deleting.
    let on_disk_bytes_freed = match std::fs::read_dir(&state.artifact_dir) {
        Ok(entries) => entries
            .filter_map(|e| e.ok())
            .filter_map(|e| e.metadata().ok().map(|m| m.len()))
            .sum(),
        Err(_) => 0,
    };

    // Clear all subsystems.
    //
    // Issue #558: `system_includes` and `compiler_hash_cache` are NOT
    // cleared — they're compiler-environment data keyed by
    // `(compiler_path, mtime, size)` and self-correcting via stat-verify
    // on access. Wiping them costs ~44 ms (re-probe via `<compiler> -v
    // -E`) / ~50–60 ms (re-blake3 the compiler binary) on the next
    // compile, paying nothing toward the user's intent of clearing
    // built artifacts. The on-disk persistence (issues #517 and #541)
    // exists specifically to survive across daemon lifecycle — Clear
    // is not a stronger lifecycle event than restart.
    state.dep_graph.load().clear();
    state.cache_system.clear();
    state.fast_hit_cache.clear();
    state.request_cache.clear();
    state.request_validation_cache.clear();
    state.rsp_cache.clear();
    state.watched_raw_dirs.clear();
    state.watched_dirs.lock().await.clear();

    // Reset stats and profiler.
    state.stats.reset();
    state.profiler.reset();

    // Delete on-disk artifact files in parallel.
    if let Ok(entries) = std::fs::read_dir(&state.artifact_dir) {
        use rayon::prelude::*;
        let paths: Vec<_> = entries.flatten().map(|e| e.path()).collect();
        paths.par_iter().for_each(|p| {
            let _ = std::fs::remove_file(p);
        });
    }

    // Clear the in-memory artifact index and persist the empty state.
    state.artifact_store.clear();
    let _ = state.artifact_store.flush();

    // Persist the (now empty) metadata cache so the prior on-disk
    // snapshot stays consistent with the live state. Empty snapshots
    // skip the write entirely, but if a previous snapshot exists we
    // also remove it — without that, a subsequent daemon would
    // restore stale entries that `Clear` was meant to wipe.
    if let Err(e) = state
        .cache_system
        .metadata()
        .save_to_disk(state.metadata_path.as_path())
    {
        tracing::warn!(
            path = %state.metadata_path.display(),
            "metadata cache save during Clear failed: {e}"
        );
    }
    let _ = std::fs::remove_file(state.metadata_path.as_path());

    // Delete on-disk depgraph snapshot.
    let _ = std::fs::remove_file(crate::depgraph::depgraph_file_path());

    // Purge the CLI-side meson-configure cache (issue #710). The daemon
    // does not write this directory itself — `zccache meson configure`
    // (`crates/zccache/src/cli/commands/meson_cache.rs`) does — but it
    // lives under the same cache root, and a `zccache clear` that left
    // poisoned v2 snapshots behind would let a stale clang PCH come back
    // on the next configure-cache hit. Best-effort: silently skip if the
    // directory does not exist.
    if let Some(cache_root) = state.artifact_dir.parent() {
        let _ = std::fs::remove_dir_all(cache_root.join("meson-configure"));
    }

    tracing::info!(
        artifacts_removed,
        metadata_cleared,
        dep_graph_contexts_cleared,
        on_disk_bytes_freed,
        "cache cleared"
    );

    Response::Cleared {
        artifacts_removed,
        metadata_cleared,
        dep_graph_contexts_cleared,
        on_disk_bytes_freed,
    }
}
