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

/// Materialize without sharing a writable inode with private or backend bytes.
pub(in crate::daemon::server) fn materialize_independent(
    source: &Path,
    destination: &Path,
) -> io::Result<()> {
    materialize_independent_with_stats(source, destination).map(|_| ())
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
    use super::super::{load_staged_artifact_paths, persist_staged_artifact_paths};
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
}
