//! Shared publication and output-delivery accounting for staged producers.

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
) -> Result<(), StagedPublishFailure> {
    #[cfg(test)]
    inject_staged_fault(&state.artifact_dir, StagedFaultPoint::IndexCommit)
        .map_err(|_| StagedPublishFailure::IndexCommit)?;
    state
        .index_writer_tx
        .send(IndexWriterCommand::Insert(key, metadata))
        .map_err(|_| StagedPublishFailure::IndexCommit)
}

/// Publish a complete v2 generation and expose its index entry only after the
/// generation's visibility pointer is durable.
pub(super) fn publish_staged_artifact(
    state: &SharedState,
    key: &str,
    metadata: ArtifactIndex,
    sources: &[NormalizedPath],
) -> Result<(), StagedPublishFailure> {
    use crate::daemon::staged_stats::{StagedBytes, StagedCounter, StagedTiming};

    let persisted =
        persist_artifact_paths_with_stats(&state.artifact_dir, key, sources).map_err(|error| {
            let reason = staged_publish_failure(&error).unwrap_or(StagedPublishFailure::StoreSetup);
            record_staged_publication_failure(state, reason);
            reason
        })?;
    if !persisted.staged {
        let reason = StagedPublishFailure::StoreSetup;
        record_staged_publication_failure(state, reason);
        return Err(reason);
    }
    send_staged_index_insert(state, key.to_string(), metadata).inspect_err(|reason| {
        record_staged_publication_failure(state, *reason);
    })?;

    state
        .profiler
        .staged
        .count(StagedCounter::PublicationSuccess);
    state
        .profiler
        .staged
        .timing(StagedTiming::Hashing, persisted.staged_hash_ns);
    state
        .profiler
        .staged
        .timing(StagedTiming::Publication, persisted.staged_publication_ns);
    state
        .profiler
        .staged
        .bytes(StagedBytes::Publication, persisted.copy_bytes);
    Ok(())
}

pub(super) fn materialize_staged_outputs(
    state: &SharedState,
    output_count: usize,
    salvage_reason: Option<StagedPublishFailure>,
    materialize: impl FnOnce() -> std::io::Result<StagedMaterializationStats>,
) -> std::io::Result<()> {
    use crate::daemon::staged_stats::{StagedBytes, StagedCounter, StagedFailure, StagedTiming};

    if let Some(reason) = salvage_reason {
        state.profiler.staged.count(StagedCounter::SalvageAttempt);
        crate::core::lifecycle::write_event(
            "staged_salvage_started",
            serde_json::json!({
                "reason": reason.id(),
                "output_count": output_count,
                "copied_bytes": 0,
                "elapsed_ns": 0,
            }),
        );
    }

    let started = std::time::Instant::now();
    match materialize() {
        Ok(observed) => {
            let elapsed_ns = started.elapsed().as_nanos() as u64;
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
            if let Some(reason) = salvage_reason {
                state.profiler.staged.count(StagedCounter::SalvageSuccess);
                state
                    .profiler
                    .staged
                    .timing(StagedTiming::Salvage, elapsed_ns);
                state
                    .profiler
                    .staged
                    .bytes(StagedBytes::Salvage, observed.copy_bytes);
                crate::core::lifecycle::write_event(
                    "staged_salvage_complete",
                    serde_json::json!({
                        "reason": reason.id(),
                        "output_count": output_count,
                        "copied_bytes": observed.copy_bytes,
                        "elapsed_ns": elapsed_ns,
                    }),
                );
            } else {
                state
                    .profiler
                    .staged
                    .timing(StagedTiming::MissMaterialization, elapsed_ns);
            }
            Ok(())
        }
        Err(error) => {
            let elapsed_ns = started.elapsed().as_nanos() as u64;
            let progress = materialization_error_progress(&error);
            state
                .profiler
                .staged
                .add_count(StagedCounter::MaterializeReflink, progress.reflink_count);
            state
                .profiler
                .staged
                .add_count(StagedCounter::MaterializeHardlink, progress.hardlink_count);
            state
                .profiler
                .staged
                .add_count(StagedCounter::MaterializeCopy, progress.copy_count);
            state
                .profiler
                .staged
                .bytes(StagedBytes::Materialization, progress.copy_bytes);
            state
                .profiler
                .staged
                .count(StagedCounter::MaterializeFailure);
            state
                .profiler
                .staged
                .failure(StagedFailure::RequestedMaterialization);

            if salvage_reason.is_some() {
                state.profiler.staged.count(StagedCounter::SalvageFailure);
                state.profiler.staged.failure(StagedFailure::Salvage);
                state
                    .profiler
                    .staged
                    .timing(StagedTiming::Salvage, elapsed_ns);
                state
                    .profiler
                    .staged
                    .bytes(StagedBytes::Salvage, progress.copy_bytes);
                crate::core::lifecycle::write_event(
                    "staged_salvage_failed",
                    serde_json::json!({
                        "reason": "requested_materialization",
                        "output_count": output_count,
                        "copied_bytes": progress.copy_bytes,
                        "elapsed_ns": elapsed_ns,
                    }),
                );
            } else {
                state
                    .profiler
                    .staged
                    .timing(StagedTiming::MissMaterialization, elapsed_ns);
                crate::core::lifecycle::write_event(
                    "staged_materialization_failed",
                    serde_json::json!({
                        "reason": "requested_materialization",
                        "output_count": output_count,
                        "copied_bytes": progress.copy_bytes,
                        "elapsed_ns": elapsed_ns,
                    }),
                );
            }
            Err(error)
        }
    }
}
