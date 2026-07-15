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

pub(super) fn archive_hash_cache_is_cold(
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

pub(super) fn discard_speculative_archive(
    speculative: &mut Option<CompletedSpeculativeArchive>,
) {
    if let Some(speculative) = speculative.take() {
        let _ = speculative.plan.cleanup();
    }
}
