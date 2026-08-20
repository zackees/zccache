//! Shared merge rules for the rustc artifact/verdict index row.

use super::super::*;

/// Load the durable row before publishing a cold-depgraph update.
///
/// Startup hydration is deferred, so a persisted artifact may not yet exist
/// in `state.artifacts`. Loading it here prevents a new verdict from replacing
/// sibling verdicts or output metadata that only exist on disk.
pub(super) fn durable_rustc_index(
    state: &SharedState,
    artifact_key_hex: &str,
) -> Option<ArtifactIndex> {
    if !state.artifact_store_loaded.load(Ordering::Acquire) {
        let _ = state.artifact_store.load_from_disk();
        state.artifact_store_loaded.store(true, Ordering::Release);
    }
    state.artifact_store.get(artifact_key_hex)
}

/// Merge a fallback row without allowing it to replace the current verdict.
///
/// `preferred` is the result produced by the current request. A durable or
/// live fallback contributes sibling verdicts and, when the current request
/// is an error with no outputs, the existing shared artifact metadata.
pub(super) fn merge_rustc_index(
    mut preferred: ArtifactIndex,
    mut fallback: ArtifactIndex,
) -> ArtifactIndex {
    if preferred.output_names.is_empty() && !fallback.output_names.is_empty() {
        let preferred_verdicts = std::mem::take(&mut preferred.rustc_verdicts);
        fallback.rustc_verdicts.extend(preferred_verdicts);
        fallback.stored_at_secs = fallback.stored_at_secs.max(preferred.stored_at_secs);
        return fallback;
    }

    for (verdict_key, verdict) in fallback.rustc_verdicts {
        preferred
            .rustc_verdicts
            .entry(verdict_key)
            .or_insert(verdict);
    }
    preferred.stored_at_secs = preferred.stored_at_secs.max(fallback.stored_at_secs);
    preferred
}
