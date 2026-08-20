//! Link/archive hash discovery and cold-archive speculation helpers.

use super::*;

pub(super) struct CompletedSpeculativeArchive {
    pub(super) plan: StagedCompilePlan,
    pub(super) result: Response,
    pub(super) process_ns: u64,
}

pub(super) struct LinkHashResults {
    pub(super) tool_hash: Option<ContentHash>,
    pub(super) inputs: Vec<(NormalizedPath, Option<ContentHash>)>,
    pub(super) wall_ns: u64,
    pub(super) tool_ns: u64,
    pub(super) inputs_ns: u64,
}

pub(super) struct PreparedLinkMiss {
    pub(super) directory_plan_result: Option<StagedPlanOutcome<StagedDirectoryPlan>>,
    pub(super) staged_plan_result: Option<StagedPlanOutcome<StagedCompilePlan>>,
    pub(super) dir_snapshot_result:
        Option<std::io::Result<crate::daemon::side_effect::DirSnapshot>>,
    pub(super) planning_ns: u64,
}

pub(super) struct LinkMissPreparationRequest<'a> {
    pub(super) staging_dir: &'a Path,
    pub(super) args: &'a [String],
    pub(super) output_path: &'a NormalizedPath,
    pub(super) secondary_outputs: &'a [NormalizedPath],
    pub(super) cwd: &'a Path,
    pub(super) is_directory_bundle: bool,
    pub(super) output_dir: &'a Path,
    pub(super) sibling_input_names: &'a std::collections::HashSet<std::ffi::OsString>,
}

pub(super) fn hash_link_inputs(
    state: &SharedState,
    tool: &Path,
    inputs: &[NormalizedPath],
    profile_enabled: bool,
) -> LinkHashResults {
    use rayon::prelude::*;

    let wall_started = profile_enabled.then(std::time::Instant::now);
    let ((tool_hash, tool_ns), (input_hashes, inputs_ns)) = rayon::join(
        || {
            let started = profile_enabled.then(std::time::Instant::now);
            let hash = hash_file_via_cache(state, tool);
            let elapsed = started
                .map(|started| started.elapsed().as_nanos() as u64)
                .unwrap_or(0);
            (hash, elapsed)
        },
        || {
            let started = profile_enabled.then(std::time::Instant::now);
            let hashes = inputs
                .par_iter()
                .map(|input| {
                    let hash = hash_normalized_file_via_cache(state, input);
                    (input.clone(), hash)
                })
                .collect();
            let elapsed = started
                .map(|started| started.elapsed().as_nanos() as u64)
                .unwrap_or(0);
            (hashes, elapsed)
        },
    );
    LinkHashResults {
        tool_hash,
        inputs: input_hashes,
        wall_ns: wall_started
            .map(|started| started.elapsed().as_nanos() as u64)
            .unwrap_or(0),
        tool_ns,
        inputs_ns,
    }
}

pub(super) fn link_hash_cache_is_cold(
    state: &SharedState,
    tool: &Path,
    inputs: &[NormalizedPath],
) -> bool {
    let metadata = state.cache_system.metadata();
    metadata
        .get_cached_hash(&NormalizedPath::new(tool))
        .is_none()
        || inputs
            .iter()
            .any(|input| metadata.get_cached_hash(input).is_none())
}

pub(super) fn should_prepare_link_miss_during_hash(
    is_archive: bool,
    hash_cache_is_cold: bool,
) -> bool {
    !is_archive && hash_cache_is_cold
}

pub(super) fn prepare_link_miss(request: LinkMissPreparationRequest<'_>) -> PreparedLinkMiss {
    let LinkMissPreparationRequest {
        staging_dir,
        args,
        output_path,
        secondary_outputs,
        cwd,
        is_directory_bundle,
        output_dir,
        sibling_input_names,
    } = request;
    let planning_started = std::time::Instant::now();
    let directory_plan_result = is_directory_bundle
        .then(|| StagedDirectoryPlan::dsymutil(staging_dir, args, output_path, cwd));
    let directory_plan_enabled = matches!(
        directory_plan_result.as_ref(),
        Some(StagedPlanOutcome::Enabled(_))
    );
    let staged_plan_result = directory_plan_result
        .is_none()
        .then(|| StagedCompilePlan::link(staging_dir, args, output_path, secondary_outputs, cwd));
    let planning_ns = planning_started.elapsed().as_nanos() as u64;
    let dir_snapshot_result = (!directory_plan_enabled).then(|| {
        crate::daemon::side_effect::snapshot_directory_excluding(output_dir, sibling_input_names)
    });
    PreparedLinkMiss {
        directory_plan_result,
        staged_plan_result,
        dir_snapshot_result,
        planning_ns,
    }
}

pub(super) fn discard_speculative_archive(speculative: &mut Option<CompletedSpeculativeArchive>) {
    if let Some(speculative) = speculative.take() {
        let _ = speculative.plan.cleanup();
    }
}

#[cfg(test)]
mod tests {
    use super::should_prepare_link_miss_during_hash;

    #[test]
    fn cold_non_archive_hash_prepares_link_miss_in_parallel() {
        assert!(should_prepare_link_miss_during_hash(false, true));
        assert!(!should_prepare_link_miss_during_hash(false, false));
        assert!(!should_prepare_link_miss_during_hash(true, true));
    }
}
