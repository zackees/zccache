//! Deterministic, path-scoped staged-pipeline pause hooks for race tests.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::sync::{Mutex, OnceLock};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::daemon::server) enum StagedHookPoint {
    PublicationStoreLocked,
    MaterializeOutput,
}

struct ActiveHook {
    token: u64,
    scope: PathBuf,
    point: StagedHookPoint,
    reached: SyncSender<()>,
    resume: Receiver<()>,
}

static NEXT_TOKEN: AtomicU64 = AtomicU64::new(1);
static ACTIVE: OnceLock<Mutex<Vec<ActiveHook>>> = OnceLock::new();

fn active() -> &'static Mutex<Vec<ActiveHook>> {
    ACTIVE.get_or_init(|| Mutex::new(Vec::new()))
}

pub(in crate::daemon::server) struct StagedHookGuard {
    token: u64,
    reached: Receiver<()>,
    resume: SyncSender<()>,
}

impl StagedHookGuard {
    pub(in crate::daemon::server) fn arm(scope: &Path, point: StagedHookPoint) -> Self {
        let token = NEXT_TOKEN.fetch_add(1, Ordering::Relaxed);
        let (reached_tx, reached) = mpsc::sync_channel(1);
        let (resume, resume_rx) = mpsc::sync_channel(1);
        active()
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .push(ActiveHook {
                token,
                scope: scope.to_path_buf(),
                point,
                reached: reached_tx,
                resume: resume_rx,
            });
        Self {
            token,
            reached,
            resume,
        }
    }

    pub(in crate::daemon::server) fn wait_until_reached(&self) {
        self.reached
            .recv()
            .expect("staged hook operation ended before reaching its pause point");
    }

    pub(in crate::daemon::server) fn resume(&self) {
        self.resume
            .send(())
            .expect("staged hook operation ended before it was resumed");
    }
}

impl Drop for StagedHookGuard {
    fn drop(&mut self) {
        active()
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .retain(|hook| hook.token != self.token);
        let _ = self.resume.try_send(());
    }
}

pub(in crate::daemon::server) fn pause(scope_path: &Path, point: StagedHookPoint) {
    let hook = {
        let mut active = active().lock().unwrap_or_else(|error| error.into_inner());
        let Some(index) = active
            .iter()
            .position(|hook| scope_path.starts_with(&hook.scope) && hook.point == point)
        else {
            return;
        };
        active.remove(index)
    };
    let _ = hook.reached.send(());
    let _ = hook.resume.recv();
}
