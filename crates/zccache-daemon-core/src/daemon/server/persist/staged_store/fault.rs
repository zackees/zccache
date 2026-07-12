//! Deterministic, path-scoped staged-pipeline fault injection for tests.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::daemon::server) enum StagedFaultPoint {
    GenerationCreate,
    OutputCopy(usize),
    OutputHash(usize),
    DurableDigest(usize),
    ManifestWrite,
    ManifestSync,
    GenerationSync,
    GenerationPublish,
    PointerCommit,
    PointerSync,
    IndexCommit,
    MaterializeOutput(usize),
    MaterializeReflink,
    MaterializeHardlink,
    MaterializeCopy,
}

struct ActiveFaults {
    token: u64,
    scope: PathBuf,
    points: Vec<StagedFaultPoint>,
}

static NEXT_TOKEN: AtomicU64 = AtomicU64::new(1);
static ACTIVE: OnceLock<Mutex<Vec<ActiveFaults>>> = OnceLock::new();

fn active() -> &'static Mutex<Vec<ActiveFaults>> {
    ACTIVE.get_or_init(|| Mutex::new(Vec::new()))
}

pub(in crate::daemon::server) struct StagedFaultGuard {
    token: u64,
}

impl StagedFaultGuard {
    pub(in crate::daemon::server) fn arm(
        scope: &Path,
        points: impl IntoIterator<Item = StagedFaultPoint>,
    ) -> Self {
        let token = NEXT_TOKEN.fetch_add(1, Ordering::Relaxed);
        active()
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .push(ActiveFaults {
                token,
                scope: scope.to_path_buf(),
                points: points.into_iter().collect(),
            });
        Self { token }
    }

    pub(in crate::daemon::server) fn assert_all_consumed(&self) {
        let active = active().lock().unwrap_or_else(|error| error.into_inner());
        let pending = active
            .iter()
            .find(|faults| faults.token == self.token)
            .map_or(0, |faults| faults.points.len());
        assert_eq!(
            pending, 0,
            "{pending} staged fault point(s) were not reached"
        );
    }
}

impl Drop for StagedFaultGuard {
    fn drop(&mut self) {
        active()
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .retain(|faults| faults.token != self.token);
    }
}

pub(in crate::daemon::server) fn inject(
    scope_path: &Path,
    point: StagedFaultPoint,
) -> io::Result<()> {
    let mut active = active().lock().unwrap_or_else(|error| error.into_inner());
    for faults in active.iter_mut() {
        if !scope_path.starts_with(&faults.scope) {
            continue;
        }
        if let Some(index) = faults
            .points
            .iter()
            .position(|candidate| *candidate == point)
        {
            faults.points.remove(index);
            return Err(io::Error::other(format!(
                "injected staged fault: {point:?}"
            )));
        }
    }
    Ok(())
}
