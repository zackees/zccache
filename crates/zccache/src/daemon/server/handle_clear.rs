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
    let dep_graph_contexts_cleared = state.dep_graph.stats().context_count as u64;

    // Calculate on-disk artifact size before deleting.
    let on_disk_bytes_freed = match std::fs::read_dir(&state.artifact_dir) {
        Ok(entries) => entries
            .filter_map(|e| e.ok())
            .filter_map(|e| e.metadata().ok().map(|m| m.len()))
            .sum(),
        Err(_) => 0,
    };

    // Clear all subsystems.
    state.dep_graph.clear();
    state.cache_system.clear();
    state.fast_hit_cache.clear();
    state.request_cache.clear();
    state.request_validation_cache.clear();
    state.rsp_cache.clear();
    state.watched_raw_dirs.clear();
    state.system_includes.lock().await.clear();
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
    let _ = std::fs::remove_file(zccache::depgraph::depgraph_file_path());

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
