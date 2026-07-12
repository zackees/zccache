//! Shared complete-set publication and index commit for staged producers.

use super::*;

pub(super) fn record_staged_publication_failure(state: &SharedState, reason: StagedPublishFailure) {
    use crate::daemon::staged_stats::{StagedCounter, StagedFailure};
    state
        .profiler
        .staged
        .count(StagedCounter::PublicationFailure);
    if reason == StagedPublishFailure::Conflict {
        state
            .profiler
            .staged
            .count(StagedCounter::PublicationConflict);
        state
            .profiler
            .staged
            .failure(StagedFailure::PublicationConflict);
    } else {
        state.profiler.staged.failure(reason.failure());
    }
}

pub(super) fn send_staged_index_insert(
    state: &SharedState,
    key: String,
    metadata: ArtifactIndex,
) -> Result<u64, StagedPublishFailure> {
    let started = std::time::Instant::now();
    #[cfg(test)]
    inject_staged_fault(&state.artifact_dir, StagedFaultPoint::IndexCommit)
        .map_err(|_| StagedPublishFailure::IndexCommit)?;
    state
        .index_writer_tx
        .send(IndexWriterCommand::Insert(key, metadata))
        .map_err(|_| StagedPublishFailure::IndexCommit)?;
    Ok(started.elapsed().as_nanos() as u64)
}

pub(super) fn record_staged_publication_success(
    state: &SharedState,
    persisted: PersistArtifactFileStats,
    index_commit_ns: u64,
) {
    if !persisted.staged {
        return;
    }
    use crate::daemon::staged_stats::{StagedBytes, StagedCounter, StagedTiming};
    state
        .profiler
        .staged
        .count(StagedCounter::PublicationSuccess);
    state
        .profiler
        .staged
        .timing(StagedTiming::Hashing, persisted.staged_hash_ns);
    state.profiler.staged.timing(
        StagedTiming::Publication,
        persisted
            .staged_publication_ns
            .saturating_add(index_commit_ns),
    );
    state
        .profiler
        .staged
        .bytes(StagedBytes::Publication, persisted.copy_bytes);
}

/// Persist a staged producer's complete output set and commit its index entry.
/// A failure is recorded exactly once and never makes the artifact cacheable.
pub(super) fn publish_artifact_paths_observed(
    state: &SharedState,
    key: &str,
    metadata: ArtifactIndex,
    sources: &[NormalizedPath],
) -> Result<PersistArtifactFileStats, StagedPublishFailure> {
    let persisted =
        persist_artifact_paths_with_stats(&state.artifact_dir, key, sources).map_err(|error| {
            staged_publish_failure(&error).unwrap_or(StagedPublishFailure::StoreSetup)
        });
    let persisted = match persisted {
        Ok(persisted) => persisted,
        Err(reason) => {
            record_staged_publication_failure(state, reason);
            return Err(reason);
        }
    };
    let index_commit_ns = match send_staged_index_insert(state, key.to_string(), metadata) {
        Ok(elapsed_ns) => elapsed_ns,
        Err(reason) => {
            record_staged_publication_failure(state, reason);
            return Err(reason);
        }
    };
    record_staged_publication_success(state, persisted, index_commit_ns);
    Ok(persisted)
}

pub(super) fn record_staged_hit_materialization(
    state: &SharedState,
    output_count: usize,
    started: std::time::Instant,
    observed: Option<StagedMaterializationStats>,
) -> bool {
    use crate::daemon::staged_stats::{StagedBytes, StagedCounter, StagedFailure, StagedTiming};
    let elapsed_ns = started.elapsed().as_nanos() as u64;
    match observed {
        Some(observed) => {
            state
                .profiler
                .staged
                .add_count(StagedCounter::MaterializeReflink, observed.reflink_count);
            state
                .profiler
                .staged
                .add_count(StagedCounter::MaterializeHardlink, observed.hardlink_count);
            state
                .profiler
                .staged
                .add_count(StagedCounter::MaterializeCopy, observed.copy_count);
            state
                .profiler
                .staged
                .bytes(StagedBytes::Materialization, observed.copy_bytes);
            state
                .profiler
                .staged
                .timing(StagedTiming::HitMaterialization, elapsed_ns);
            true
        }
        None => {
            state
                .profiler
                .staged
                .count(StagedCounter::MaterializeFailure);
            state
                .profiler
                .staged
                .failure(StagedFailure::RequestedMaterialization);
            state
                .profiler
                .staged
                .timing(StagedTiming::HitMaterialization, elapsed_ns);
            crate::core::lifecycle::write_event(
                "staged_materialization_failed",
                serde_json::json!({
                    "reason": "requested_materialization",
                    "output_count": output_count,
                    "copied_bytes": 0,
                    "elapsed_ns": elapsed_ns,
                }),
            );
            false
        }
    }
}
