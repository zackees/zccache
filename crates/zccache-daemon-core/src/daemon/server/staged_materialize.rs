//! Shared requested-path materialization and salvage observations.

use super::handle_exec::ExecStagedPlan;
use super::*;

fn record_observed_materialization(
    state: &SharedState,
    output_count: usize,
    salvage_reason: Option<&'static str>,
    started: std::time::Instant,
    result: std::io::Result<StagedMaterializationStats>,
) -> std::io::Result<()> {
    use crate::daemon::staged_stats::{StagedBytes, StagedCounter, StagedFailure, StagedTiming};
    match result {
        Ok(observed) => {
            state
                .profiler
                .staged
                .add_count(StagedCounter::MaterializeReflink, observed.reflink_count);
            state
                .profiler
                .staged
                .add_count(StagedCounter::MaterializeCopy, observed.copy_count);
            state
                .profiler
                .staged
                .bytes(StagedBytes::Materialization, observed.copy_bytes);
            let elapsed_ns = started.elapsed().as_nanos() as u64;
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
                        "reason": reason,
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
            if let Some(reason) = salvage_reason {
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
                        "reason": reason,
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
            }
            crate::core::lifecycle::write_event(
                "staged_materialization_failed",
                serde_json::json!({
                    "reason": "requested_materialization",
                    "output_count": output_count,
                    "copied_bytes": progress.copy_bytes,
                    "elapsed_ns": elapsed_ns,
                }),
            );
            Err(error)
        }
    }
}

fn record_salvage_start(
    state: &SharedState,
    output_count: usize,
    salvage_reason: Option<&'static str>,
) {
    let Some(reason) = salvage_reason else {
        return;
    };
    use crate::daemon::staged_stats::StagedCounter;
    state.profiler.staged.count(StagedCounter::SalvageAttempt);
    crate::core::lifecycle::write_event(
        "staged_salvage_started",
        serde_json::json!({
            "reason": reason,
            "output_count": output_count,
            "copied_bytes": 0,
            "elapsed_ns": 0,
        }),
    );
}

pub(super) fn materialize_link_plan_observed(
    state: &SharedState,
    plan: &StagedCompilePlan,
    salvage_reason: Option<&'static str>,
) -> std::io::Result<()> {
    let output_count = plan.output_paths().len();
    record_salvage_start(state, output_count, salvage_reason);
    let started = std::time::Instant::now();
    record_observed_materialization(
        state,
        output_count,
        salvage_reason,
        started,
        plan.materialize(),
    )
}

pub(super) fn materialize_exec_plan_observed(
    state: &SharedState,
    plan: &ExecStagedPlan,
    salvage_reason: Option<&'static str>,
) -> std::io::Result<()> {
    let output_count = plan.output_count();
    record_salvage_start(state, output_count, salvage_reason);
    let started = std::time::Instant::now();
    record_observed_materialization(
        state,
        output_count,
        salvage_reason,
        started,
        plan.materialize(),
    )
}

pub(super) fn materialize_multi_plan_observed(
    state: &SharedState,
    plan: &StagedMultiUnitPlan,
    salvage_reason: Option<&'static str>,
) -> std::io::Result<()> {
    let output_count = plan.outputs.len();
    record_salvage_start(state, output_count, salvage_reason);
    let started = std::time::Instant::now();
    record_observed_materialization(
        state,
        output_count,
        salvage_reason,
        started,
        plan.materialize(),
    )
}
