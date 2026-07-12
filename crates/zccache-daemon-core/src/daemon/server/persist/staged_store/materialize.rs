//! Independent requested-path materialization and physical-work observations.

use super::{copy_output, set_readonly};
use std::fs;
use std::io;
use std::path::Path;

#[derive(Clone, Copy, Debug, Default)]
pub(in crate::daemon::server) struct StagedMaterializationStats {
    pub(in crate::daemon::server) reflink_count: u64,
    pub(in crate::daemon::server) hardlink_count: u64,
    pub(in crate::daemon::server) copy_count: u64,
    pub(in crate::daemon::server) copy_bytes: u64,
}

impl StagedMaterializationStats {
    pub(in crate::daemon::server) fn add(&mut self, other: Self) {
        self.reflink_count = self.reflink_count.saturating_add(other.reflink_count);
        self.hardlink_count = self.hardlink_count.saturating_add(other.hardlink_count);
        self.copy_count = self.copy_count.saturating_add(other.copy_count);
        self.copy_bytes = self.copy_bytes.saturating_add(other.copy_bytes);
    }
}

#[derive(Debug)]
struct StagedMaterializationError {
    source: io::Error,
    progress: StagedMaterializationStats,
}

impl std::fmt::Display for StagedMaterializationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.source.fmt(formatter)
    }
}

impl std::error::Error for StagedMaterializationError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

pub(in crate::daemon::server) fn materialization_error(
    source: io::Error,
    progress: StagedMaterializationStats,
) -> io::Error {
    io::Error::new(
        source.kind(),
        StagedMaterializationError { source, progress },
    )
}

pub(in crate::daemon::server) fn materialization_error_progress(
    error: &io::Error,
) -> StagedMaterializationStats {
    error
        .get_ref()
        .and_then(|source| source.downcast_ref::<StagedMaterializationError>())
        .map_or_else(StagedMaterializationStats::default, |error| error.progress)
}

pub(in crate::daemon::server) fn materialize_independent_with_stats(
    source: &Path,
    destination: &Path,
) -> io::Result<StagedMaterializationStats> {
    if let Ok(metadata) = fs::metadata(destination) {
        if metadata.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::IsADirectory,
                format!(
                    "output destination is a directory: {}",
                    destination.display()
                ),
            ));
        }
        let _ = set_readonly(destination, false);
        fs::remove_file(destination)?;
    }
    copy_output(source, destination).map(|(reflink, copy_bytes)| {
        let _ = set_readonly(destination, false);
        StagedMaterializationStats {
            reflink_count: u64::from(reflink),
            hardlink_count: 0,
            copy_count: u64::from(!reflink),
            copy_bytes,
        }
    })
}

#[cfg(test)]
mod tests {
    use super::super::{
        load_staged_artifact_paths, persist_staged_artifact_paths, StagedFaultGuard,
        StagedFaultPoint,
    };
    use super::*;

    #[test]
    fn staged_persist_and_materialization_report_physical_work() {
        let dir = tempfile::tempdir().unwrap();
        let artifact_dir = dir.path().join("artifacts");
        fs::create_dir_all(&artifact_dir).unwrap();
        let source = dir.path().join("source.rlib");
        fs::write(&source, b"observable staged payload").unwrap();

        let persisted =
            persist_staged_artifact_paths(&artifact_dir, &"9".repeat(64), &[source.into()])
                .unwrap();
        assert!(persisted.staged);
        assert_eq!(persisted.reflink_count + persisted.copy_count, 1);

        let payload = load_staged_artifact_paths(&artifact_dir, &"9".repeat(64), &[25])
            .unwrap()
            .unwrap()
            .remove(0);
        let destination = dir.path().join("restored.rlib");
        let materialized = materialize_independent_with_stats(&payload, &destination).unwrap();
        assert_eq!(materialized.reflink_count + materialized.copy_count, 1);
        assert_eq!(fs::read(destination).unwrap(), b"observable staged payload");
    }

    #[test]
    fn independent_materialization_faults_fall_back_or_fail_cleanly() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source.rlib");
        fs::write(&source, b"independent materialization payload").unwrap();

        let fallback = dir.path().join("fallback.rlib");
        let reflink_fault =
            StagedFaultGuard::arm(&fallback, [StagedFaultPoint::MaterializeReflink]);
        let observed = materialize_independent_with_stats(&source, &fallback).unwrap();
        assert_eq!(observed.reflink_count, 0);
        assert_eq!(observed.copy_count, 1);
        assert_eq!(observed.copy_bytes, 35);
        assert_eq!(
            fs::read(&fallback).unwrap(),
            b"independent materialization payload"
        );
        reflink_fault.assert_all_consumed();

        let failed = dir.path().join("failed.rlib");
        let all_faults = StagedFaultGuard::arm(
            &failed,
            [
                StagedFaultPoint::MaterializeReflink,
                StagedFaultPoint::MaterializeCopy,
            ],
        );
        materialize_independent_with_stats(&source, &failed).unwrap_err();
        assert!(
            !failed.exists(),
            "failed copy tier left a partial destination"
        );
        all_faults.assert_all_consumed();
    }
}
