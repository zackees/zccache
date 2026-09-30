//! Completion signal for the background disk-maintenance loop (#1846).
//!
//! The startup eviction scan is asynchronous: `run()` returns to the accept
//! loop while the scan is still queued behind artifact/depgraph loading and
//! several blocking-pool hops. Its on-disk marker is the *durable* record, but
//! polling that file under a wall-clock budget turns CPU starvation into a
//! false failure. This watch channel publishes each pass outcome in-process so
//! a caller can await the pass itself, and learns immediately when it failed
//! instead of waiting out a timeout. No IPC message is involved.

use std::io;

/// Snapshot of what the maintenance loop has finished so far.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DiskMaintenanceProgress {
    /// Passes finished, whatever their outcome.
    pub attempts: u64,
    /// Full (daily) passes that finished successfully and wrote their marker.
    pub full_completed: u64,
    /// Rendering of the most recent failure, if any pass failed.
    pub last_error: Option<String>,
}

impl DiskMaintenanceProgress {
    /// True once the first full pass succeeded or any pass failed -- the two
    /// outcomes after which waiting longer cannot change the answer.
    #[must_use]
    pub fn settled(&self) -> bool {
        self.full_completed > 0 || self.last_error.is_some()
    }
}

pub(super) fn record_pass(
    progress: &tokio::sync::watch::Sender<DiskMaintenanceProgress>,
    full: bool,
    outcome: &io::Result<super::DiskMaintenanceReport>,
) {
    progress.send_modify(|state| {
        state.attempts += 1;
        match outcome {
            Ok(_) => state.full_completed += u64::from(full),
            Err(error) => state.last_error = Some(error.to_string()),
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::server::{MaintenanceKind, MaintenancePressure};

    fn report(kind: MaintenanceKind) -> super::super::DiskMaintenanceReport {
        super::super::DiskMaintenanceReport {
            kind,
            pressure: MaintenancePressure::None,
            budget_bytes: 0,
            usage_before_bytes: 0,
            usage_after_bytes: 0,
            bytes_reclaimed: 0,
            artifacts_removed: 0,
            expired_artifacts_removed: 0,
            pending_write_bytes: 0,
            retired_bytes_reclaimed: 0,
        }
    }

    #[test]
    fn pressure_pass_does_not_settle_but_full_pass_does() {
        let (tx, rx) = tokio::sync::watch::channel(DiskMaintenanceProgress::default());
        record_pass(&tx, false, &Ok(report(MaintenanceKind::Pressure)));
        assert_eq!(rx.borrow().attempts, 1);
        assert!(!rx.borrow().settled());
        record_pass(&tx, true, &Ok(report(MaintenanceKind::Full)));
        assert!(rx.borrow().settled());
        assert_eq!(rx.borrow().full_completed, 1);
    }

    #[test]
    fn failed_pass_settles_with_its_error() {
        let (tx, rx) = tokio::sync::watch::channel(DiskMaintenanceProgress::default());
        record_pass(&tx, true, &Err(io::Error::other("scan exploded")));
        let progress = rx.borrow();
        assert!(progress.settled());
        assert_eq!(progress.full_completed, 0);
        assert_eq!(progress.last_error.as_deref(), Some("scan exploded"));
    }
}
