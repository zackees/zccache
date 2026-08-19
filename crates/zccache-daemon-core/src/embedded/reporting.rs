//! Internal conversions from daemon-engine reports to the public facade.

use super::{
    DetailedFlushReport, DiskMaintenanceKind, DiskMaintenancePressure, DiskMaintenanceReport,
    FlushReport, FlushStepOutcome, FlushStepReport, ServiceStats,
};
use crate::daemon::server::{
    DiskMaintenanceReport as InternalDiskMaintenanceReport, EmbeddedFlushReport,
    EmbeddedStatsSnapshot, FlushStepOutcome as InternalFlushStepOutcome,
    MaintenanceKind as InternalMaintenanceKind, MaintenancePressure as InternalMaintenancePressure,
};

impl ServiceStats {
    pub(super) fn from_snapshot(snapshot: EmbeddedStatsSnapshot) -> Self {
        let status = snapshot.status;
        Self {
            cache_root: status.cache_dir,
            uptime_secs: status.uptime_secs,
            total_compilations: status.total_compilations,
            cache_hits: status.cache_hits,
            cache_misses: status.cache_misses,
            non_cacheable: status.non_cacheable,
            compile_errors: status.compile_errors,
            compile_errors_cached: status.compile_errors_cached,
            time_saved_ms: status.time_saved_ms,
            artifact_count: status.artifact_count,
            cache_size_bytes: status.cache_size_bytes,
            metadata_entries: status.metadata_entries,
            dep_graph_contexts: status.dep_graph_contexts,
            dep_graph_files: status.dep_graph_files,
            sessions_total: status.sessions_total,
            sessions_active: status.sessions_active,
            phase_profile: snapshot.phase_profile,
        }
    }
}

impl FlushReport {
    pub(super) fn from_report(report: EmbeddedFlushReport) -> Self {
        Self {
            pending_writes_drained: report.pending_writes_drained,
            artifact_entries: report.artifact_entries,
            metadata_entries: report.metadata_entries,
        }
    }
}

impl DetailedFlushReport {
    pub(super) fn from_report(report: EmbeddedFlushReport) -> Self {
        debug_assert_eq!(report.is_complete(), {
            report.pending_writes_drained
                && report.index_writer_drained
                && report
                    .steps
                    .iter()
                    .all(|step| matches!(step.outcome, InternalFlushStepOutcome::Completed))
        });
        Self {
            pending_writes_drained: report.pending_writes_drained,
            index_writer_drained: report.index_writer_drained,
            steps: report
                .steps
                .into_iter()
                .map(|step| FlushStepReport {
                    step: step.step,
                    outcome: match step.outcome {
                        InternalFlushStepOutcome::Completed => FlushStepOutcome::Completed,
                        InternalFlushStepOutcome::Failed(error) => FlushStepOutcome::Failed(error),
                        InternalFlushStepOutcome::TimedOut => FlushStepOutcome::TimedOut,
                    },
                })
                .collect(),
            artifact_entries: report.artifact_entries,
            metadata_entries: report.metadata_entries,
        }
    }
}

impl DiskMaintenanceReport {
    pub(super) fn from_report(report: InternalDiskMaintenanceReport) -> Self {
        Self {
            kind: match report.kind {
                InternalMaintenanceKind::Pressure => DiskMaintenanceKind::Pressure,
                InternalMaintenanceKind::Full => DiskMaintenanceKind::Full,
            },
            pressure: match report.pressure {
                InternalMaintenancePressure::None => DiskMaintenancePressure::None,
                InternalMaintenancePressure::Soft => DiskMaintenancePressure::Soft,
                InternalMaintenancePressure::Hard => DiskMaintenancePressure::Hard,
            },
            budget_bytes: report.budget_bytes,
            usage_before_bytes: report.usage_before_bytes,
            usage_after_bytes: report.usage_after_bytes,
            bytes_reclaimed: report.bytes_reclaimed,
            artifacts_removed: report.artifacts_removed,
            expired_artifacts_removed: report.expired_artifacts_removed,
            pending_write_bytes: report.pending_write_bytes,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::FlushReport;

    #[test]
    fn legacy_flush_report_literal_and_exhaustive_pattern_still_compile() {
        let report = FlushReport {
            pending_writes_drained: true,
            artifact_entries: 2,
            metadata_entries: 3,
        };
        let FlushReport {
            pending_writes_drained,
            artifact_entries,
            metadata_entries,
        } = report;
        assert!(pending_writes_drained);
        assert_eq!((artifact_entries, metadata_entries), (2, 3));
    }
}
