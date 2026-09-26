//! Fixtures for tests of the opt-in C/C++ staged-artifact lane (#1648).
//!
//! Since #1147 the C/C++ lane runs only under `ZCCACHE_STAGED_ARTIFACTS=c-cpp`
//! (or `all`); the default stages Rust alone. Tests of that lane used to
//! depend on the caller exporting the variable, so the ignored suite ran them
//! against the legacy path. They now opt in themselves.

use super::super::*;
use super::CacheDirEnvGuard;

/// Sets `ZCCACHE_STAGED_ARTIFACTS` for one test and restores it on drop.
///
/// The variable is process-global, so the guard serializes on the same lock
/// as [`CacheDirEnvGuard`], which every other process-env mutation shares.
pub(crate) struct StagedArtifactsEnvGuard {
    _lock: Option<std::sync::MutexGuard<'static, ()>>,
    previous: Option<std::ffi::OsString>,
}

impl StagedArtifactsEnvGuard {
    pub(crate) fn set(value: &str) -> Self {
        Self::install(Some(CacheDirEnvGuard::lock()), value)
    }

    /// For a test that already holds the env lock through `held`.
    pub(crate) fn set_while_locked(_held: &CacheDirEnvGuard, value: &str) -> Self {
        Self::install(None, value)
    }

    fn install(lock: Option<std::sync::MutexGuard<'static, ()>>, value: &str) -> Self {
        let previous = std::env::var_os(super::super::persist::STAGED_ARTIFACTS_ENV);
        std::env::set_var(super::super::persist::STAGED_ARTIFACTS_ENV, value);
        Self {
            _lock: lock,
            previous,
        }
    }
}

impl Drop for StagedArtifactsEnvGuard {
    fn drop(&mut self) {
        match &self.previous {
            Some(previous) => {
                std::env::set_var(super::super::persist::STAGED_ARTIFACTS_ENV, previous);
            }
            None => std::env::remove_var(super::super::persist::STAGED_ARTIFACTS_ENV),
        }
    }
}

/// Whether this host exposes a native per-file change marker.
///
/// Multi-source staged publication requires one to prove an input did not
/// change and change back during the compile (#1193). Without it (Linux and
/// macOS today) the snapshot is incomplete and publication fails closed, so
/// the compile still succeeds but is never cached.
pub(in crate::daemon::server) fn native_change_markers() -> bool {
    let dir = tempfile::tempdir().unwrap();
    let probe = dir.path().join("marker-probe");
    std::fs::write(&probe, b"probe").unwrap();
    crate::platform::fs::identity::change_marker(&probe).is_some()
}

/// Wait until every deferred cache publication has finished.
///
/// Single-source C/C++ staged outputs are materialized before publication
/// and published by the persistence worker after the response (#1104), so a
/// test must settle before it inspects publication side effects.
pub(in crate::daemon::server) async fn settle_publication(state: &SharedState) {
    assert!(
        pending_writes::await_all(
            &state.pending_cache_writes,
            std::time::Duration::from_secs(30)
        )
        .await,
        "deferred cache publication did not drain"
    );
    drop(state.artifact_publication.write().await);
}
