//! Cache publication and requested-path delivery for directory link outputs.

use super::*;

fn publish_and_materialize_staged_directory(
    state: &SharedState,
    plan: &StagedDirectoryPlan,
    key: &str,
    metadata: ArtifactIndex,
) -> std::io::Result<bool> {
    let publication = publish_artifact_paths_observed(
        state,
        key,
        metadata,
        std::slice::from_ref(plan.archive_path()),
    );
    let salvage_reason = publication.as_ref().err().map(|reason| reason.id());
    materialize_directory_plan_observed(state, plan, salvage_reason)?;
    Ok(publication.is_ok())
}

pub(super) fn cache_staged_directory_link(
    state: &Arc<SharedState>,
    plan: &StagedDirectoryPlan,
    key: &str,
    stdout: &Arc<Vec<u8>>,
    stderr: &Arc<Vec<u8>>,
) -> std::io::Result<()> {
    let bundle_size = match plan.pack() {
        Ok(size) => size,
        Err(error) => {
            tracing::warn!(%error, "directory output is not bundle-cacheable");
            return materialize_directory_plan_observed(state, plan, None);
        }
    };
    let bytes = match std::fs::read(plan.archive_path()) {
        Ok(bytes) => bytes,
        Err(error) => {
            tracing::warn!(%error, "failed to read staged directory bundle");
            return materialize_directory_plan_observed(state, plan, None);
        }
    };
    let artifact = ArtifactData {
        outputs: vec![ArtifactOutput {
            name: plan.output_name(),
            payload: ArtifactPayload::Bytes(Arc::new(bytes)),
        }],
        stdout: Arc::clone(stdout),
        stderr: Arc::clone(stderr),
        exit_code: 0,
    };
    let cached = CachedArtifact::from_artifact_data(&artifact);
    let payload_size = usize::try_from(bundle_size).unwrap_or(usize::MAX);
    state
        .in_flight_bytes
        .fetch_add(payload_size, Ordering::Relaxed);
    let _guard = InFlightGuard {
        state: Arc::clone(state),
        size: payload_size,
    };
    let cacheable =
        publish_and_materialize_staged_directory(state, plan, key, cached.meta.clone())?;
    if cacheable {
        state.artifacts.insert(key.to_string(), cached);
        tracing::debug!(%key, "directory artifact cached");
    }
    Ok(())
}
