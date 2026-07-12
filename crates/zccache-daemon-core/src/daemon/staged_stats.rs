//! Bounded aggregate telemetry for immutable staged compiler outputs (#1071).

use std::sync::atomic::{AtomicU64, Ordering};

use crate::protocol::StagedProfileSummary;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(usize)]
pub(crate) enum StagedCounter {
    PlanAttempted,
    PlanEnabled,
    PlanUnsupported,
    PlanError,
    CompilerStaged,
    PublicationSuccess,
    PublicationFailure,
    PublicationConflict,
    SalvageAttempt,
    SalvageSuccess,
    SalvageFailure,
    MaterializeReflink,
    MaterializeHardlink,
    MaterializeCopy,
    MaterializeFailure,
}

const COUNTERS: &[(StagedCounter, &str)] = &[
    (StagedCounter::PlanAttempted, "plan_attempted"),
    (StagedCounter::PlanEnabled, "plan_enabled"),
    (StagedCounter::PlanUnsupported, "plan_unsupported"),
    (StagedCounter::PlanError, "plan_error"),
    (StagedCounter::CompilerStaged, "compiler_staged"),
    (StagedCounter::PublicationSuccess, "publication_success"),
    (StagedCounter::PublicationFailure, "publication_failure"),
    (StagedCounter::PublicationConflict, "publication_conflict"),
    (StagedCounter::SalvageAttempt, "salvage_attempt"),
    (StagedCounter::SalvageSuccess, "salvage_success"),
    (StagedCounter::SalvageFailure, "salvage_failure"),
    (StagedCounter::MaterializeReflink, "materialize_reflink"),
    (
        StagedCounter::MaterializeHardlink,
        "materialize_hardlink_shared",
    ),
    (StagedCounter::MaterializeCopy, "materialize_copy"),
    (StagedCounter::MaterializeFailure, "materialize_failure"),
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(usize)]
pub(crate) enum StagedTiming {
    Planning,
    Compiler,
    Hashing,
    Publication,
    Salvage,
    MissMaterialization,
    HitMaterialization,
}

const TIMINGS: &[(StagedTiming, &str)] = &[
    (StagedTiming::Planning, "planning"),
    (StagedTiming::Compiler, "compiler_staging"),
    (StagedTiming::Hashing, "hashing"),
    (StagedTiming::Publication, "publication"),
    (StagedTiming::Salvage, "salvage"),
    (StagedTiming::MissMaterialization, "miss_materialization"),
    (StagedTiming::HitMaterialization, "hit_materialization"),
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(usize)]
pub(crate) enum StagedBytes {
    Publication,
    Salvage,
    Materialization,
}

const BYTES: &[(StagedBytes, &str)] = &[
    (StagedBytes::Publication, "publication_copied"),
    (StagedBytes::Salvage, "salvage_copied"),
    (StagedBytes::Materialization, "materialization_copied"),
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(usize)]
pub(crate) enum StagedFailure {
    UnsupportedShape,
    Planning,
    PlanLaneDisabled,
    PlanOutputToStdout,
    PlanOutputNameCollision,
    PlanUnmodeledSideOutput,
    PlanUnsupportedOutputRole,
    PlanMissingRequiredOutputFlag,
    PlanMissingOptionValue,
    PlanOutputMissingFilename,
    PlanUnsupportedOutputPath,
    PlanAmbiguousOutputArgument,
    PlanOutputNotInArguments,
    PlanNoDeclaredOutputs,
    PlanStagingDirectoryCreate,
    OutputValidation,
    Hashing,
    DurableDigest,
    Manifest,
    Publication,
    PublicationConflict,
    Salvage,
    RequestedMaterialization,
    CorruptObject,
}

const FAILURES: &[(StagedFailure, &str)] = &[
    (StagedFailure::UnsupportedShape, "unsupported_shape"),
    (StagedFailure::Planning, "planning"),
    (StagedFailure::PlanLaneDisabled, "plan_lane_disabled"),
    (StagedFailure::PlanOutputToStdout, "plan_output_to_stdout"),
    (
        StagedFailure::PlanOutputNameCollision,
        "plan_output_name_collision",
    ),
    (
        StagedFailure::PlanUnmodeledSideOutput,
        "plan_unmodeled_side_output",
    ),
    (
        StagedFailure::PlanUnsupportedOutputRole,
        "plan_unsupported_output_role",
    ),
    (
        StagedFailure::PlanMissingRequiredOutputFlag,
        "plan_missing_required_output_flag",
    ),
    (
        StagedFailure::PlanMissingOptionValue,
        "plan_missing_option_value",
    ),
    (
        StagedFailure::PlanOutputMissingFilename,
        "plan_output_missing_filename",
    ),
    (
        StagedFailure::PlanUnsupportedOutputPath,
        "plan_unsupported_output_path",
    ),
    (
        StagedFailure::PlanAmbiguousOutputArgument,
        "plan_ambiguous_output_argument",
    ),
    (
        StagedFailure::PlanOutputNotInArguments,
        "plan_output_not_in_arguments",
    ),
    (
        StagedFailure::PlanNoDeclaredOutputs,
        "plan_no_declared_outputs",
    ),
    (
        StagedFailure::PlanStagingDirectoryCreate,
        "plan_staging_directory_create",
    ),
    (StagedFailure::OutputValidation, "output_validation"),
    (StagedFailure::Hashing, "hashing"),
    (StagedFailure::DurableDigest, "durable_digest"),
    (StagedFailure::Manifest, "manifest"),
    (StagedFailure::Publication, "publication"),
    (StagedFailure::PublicationConflict, "publication_conflict"),
    (StagedFailure::Salvage, "salvage"),
    (
        StagedFailure::RequestedMaterialization,
        "requested_materialization",
    ),
    (StagedFailure::CorruptObject, "corrupt_object"),
];

pub(crate) struct StagedProfiler {
    counters: [AtomicU64; COUNTERS.len()],
    timings_ns: [AtomicU64; TIMINGS.len()],
    bytes: [AtomicU64; BYTES.len()],
    failures: [AtomicU64; FAILURES.len()],
}

fn saturating_atomic_add(value: &AtomicU64, amount: u64) {
    let _ = value.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
        Some(current.saturating_add(amount))
    });
}

impl StagedProfiler {
    pub(crate) fn new() -> Self {
        Self {
            counters: std::array::from_fn(|_| AtomicU64::new(0)),
            timings_ns: std::array::from_fn(|_| AtomicU64::new(0)),
            bytes: std::array::from_fn(|_| AtomicU64::new(0)),
            failures: std::array::from_fn(|_| AtomicU64::new(0)),
        }
    }

    pub(crate) fn count(&self, counter: StagedCounter) {
        self.add_count(counter, 1);
    }

    pub(crate) fn add_count(&self, counter: StagedCounter, amount: u64) {
        saturating_atomic_add(&self.counters[counter as usize], amount);
    }

    pub(crate) fn timing(&self, timing: StagedTiming, elapsed_ns: u64) {
        saturating_atomic_add(&self.timings_ns[timing as usize], elapsed_ns);
    }

    pub(crate) fn bytes(&self, kind: StagedBytes, amount: u64) {
        saturating_atomic_add(&self.bytes[kind as usize], amount);
    }

    pub(crate) fn failure(&self, failure: StagedFailure) {
        saturating_atomic_add(&self.failures[failure as usize], 1);
    }

    pub(crate) fn reset(&self) {
        for value in self
            .counters
            .iter()
            .chain(&self.timings_ns)
            .chain(&self.bytes)
            .chain(&self.failures)
        {
            value.store(0, Ordering::Relaxed);
        }
    }

    pub(crate) fn snapshot(&self) -> StagedProfileSummary {
        StagedProfileSummary {
            counters: COUNTERS
                .iter()
                .map(|(kind, name)| {
                    (
                        (*name).to_string(),
                        self.counters[*kind as usize].load(Ordering::Relaxed),
                    )
                })
                .collect(),
            timings_ns: TIMINGS
                .iter()
                .map(|(kind, name)| {
                    (
                        (*name).to_string(),
                        self.timings_ns[*kind as usize].load(Ordering::Relaxed),
                    )
                })
                .collect(),
            bytes: BYTES
                .iter()
                .map(|(kind, name)| {
                    (
                        (*name).to_string(),
                        self.bytes[*kind as usize].load(Ordering::Relaxed),
                    )
                })
                .collect(),
            failures: FAILURES
                .iter()
                .map(|(kind, name)| {
                    (
                        (*name).to_string(),
                        self.failures[*kind as usize].load(Ordering::Relaxed),
                    )
                })
                .collect(),
        }
    }
}

impl Default for StagedProfiler {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_uses_only_bounded_non_sensitive_keys() {
        let profiler = StagedProfiler::new();
        let snapshot = profiler.snapshot();
        for key in snapshot
            .counters
            .keys()
            .chain(snapshot.timings_ns.keys())
            .chain(snapshot.bytes.keys())
            .chain(snapshot.failures.keys())
        {
            assert!(key
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte == b'_'));
            assert!(!key.contains('/') && !key.contains('\\') && !key.contains(':'));
        }
    }

    #[test]
    fn accumulation_saturates_and_reset_clears_every_family() {
        let profiler = StagedProfiler::new();
        profiler.counters[StagedCounter::PlanAttempted as usize].store(u64::MAX, Ordering::Relaxed);
        profiler.count(StagedCounter::PlanAttempted);
        profiler.timing(StagedTiming::Planning, 7);
        profiler.bytes(StagedBytes::Publication, 11);
        profiler.failure(StagedFailure::Planning);

        let saturated = profiler.snapshot();
        assert_eq!(saturated.counters["plan_attempted"], u64::MAX);
        assert_eq!(saturated.timings_ns["planning"], 7);
        assert_eq!(saturated.bytes["publication_copied"], 11);
        assert_eq!(saturated.failures["planning"], 1);

        profiler.reset();
        let reset = profiler.snapshot();
        assert!(reset
            .counters
            .values()
            .chain(reset.timings_ns.values())
            .chain(reset.bytes.values())
            .chain(reset.failures.values())
            .all(|value| *value == 0));
    }
}
