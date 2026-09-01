//! Unit tests for `server/` submodules. Originally a single 2.3K-LOC
//! `tests.rs`; split per domain so each file stays well under 1,000 LOC.
//! Each module owns whatever fixture / helper code its tests use, except
//! for the crate-wide `CacheDirEnvGuard` defined below.

use std::path::Path;

mod artifact_store_deferred;
mod cache_trim;
mod clear_handler;
mod client_env;
mod compiler_hash;
mod connection_disconnect;
mod connection_ipc;
mod connection_self_profile;
mod deferred_cold_path;
mod depgraph_reset_reachability;
mod disk_maintenance;
mod embedded_flush;
mod exec_probe;
mod fingerprint;
mod fs_matrix;
mod index_writer_gone;
mod link_cache;
mod metadata_deferred;
mod multi_restart_context_key;
mod pack;
mod path_remap;
mod pch;
mod pending_cache_writes;
mod post_link_hook;
mod release_worktree_handles;
mod rsp_cache;
mod rustc_depinfo;
mod server_ipc;
mod session_errors;
mod session_staged_attribution;
mod staged_compiler_sets;
mod system_includes_deferred;
mod watcher_lifecycle;
mod write_cached;

/// Bind a daemon server on a fresh endpoint, rooted at a cache directory
/// under `cache_root` that no other test can reach.
///
/// Prefer this over [`super::DaemonServer::bind`] in tests. `bind` resolves
/// its cache directory from the process-global `ZCCACHE_CACHE_DIR`, so a test
/// that calls it without holding [`CacheDirEnvGuard`]'s lock reads whatever
/// root a *concurrently running* guarded test has installed. Two daemons then
/// share one cache root and the loser fails to acquire it — surfacing as
/// `bind(..).unwrap()` panicking with an `Endpoint` error rather than as any
/// assertion in the test that happened to lose (issues #1254, #1261).
pub(super) fn bind_isolated_server(cache_root: &Path) -> super::DaemonServer {
    bind_isolated_server_at(&crate::ipc::unique_test_endpoint(), cache_root)
}

/// As [`bind_isolated_server`], for tests that need the endpoint themselves
/// (to connect a client) and so must mint it before binding.
pub(super) fn bind_isolated_server_at(endpoint: &str, cache_root: &Path) -> super::DaemonServer {
    let cache_dir: crate::core::NormalizedPath = cache_root.join("zccache-cache").into();
    super::DaemonServer::bind_with_cache_dir(endpoint, &cache_dir)
        .expect("bind daemon on an isolated cache root")
}

/// RAII guard that overrides `ZCCACHE_CACHE_DIR` for the duration of a
/// single test, restoring the previous value on drop. This is the one owner
/// for process-global cache-dir test mutations across the daemon crate. It
/// also clears the daemon namespace while installed and restores it on drop,
/// so a fixture cannot inherit another test's daemon identity.
pub(crate) struct CacheDirEnvGuard {
    _lock: std::sync::MutexGuard<'static, ()>,
    previous_cache_dir: Option<std::ffi::OsString>,
    previous_namespace: Option<std::ffi::OsString>,
}

static CACHE_DIR_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

impl CacheDirEnvGuard {
    /// Serializes tests that read or mutate daemon process environment without
    /// changing any variables itself. Tests with explicit cache roots use
    /// this when they must remain immune to concurrent staged-policy changes.
    pub(crate) fn lock() -> std::sync::MutexGuard<'static, ()> {
        CACHE_DIR_ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub(crate) fn set(path: &Path) -> Self {
        let lock = Self::lock();
        Self::set_with_lock(path, lock)
    }

    /// Attempts to override the cache directory without waiting for another
    /// test that owns the process-global environment. Used by regression tests
    /// to prove their fixture shares this guard rather than a private lock.
    pub(crate) fn try_set(path: &Path) -> Option<Self> {
        let lock = match CACHE_DIR_ENV_LOCK.try_lock() {
            Ok(lock) => lock,
            Err(std::sync::TryLockError::Poisoned(error)) => error.into_inner(),
            Err(std::sync::TryLockError::WouldBlock) => return None,
        };
        Some(Self::set_with_lock(path, lock))
    }

    fn set_with_lock(path: &Path, lock: std::sync::MutexGuard<'static, ()>) -> Self {
        let previous_cache_dir = std::env::var_os(crate::core::config::CACHE_DIR_ENV);
        let previous_namespace = std::env::var_os(crate::core::config::DAEMON_NAMESPACE_ENV);
        std::env::set_var(crate::core::config::CACHE_DIR_ENV, path);
        std::env::remove_var(crate::core::config::DAEMON_NAMESPACE_ENV);
        Self {
            _lock: lock,
            previous_cache_dir,
            previous_namespace,
        }
    }

    pub(super) fn set_with_namespace(path: &Path, namespace: &str) -> Self {
        let lock = Self::lock();
        let previous_cache_dir = std::env::var_os(crate::core::config::CACHE_DIR_ENV);
        let previous_namespace = std::env::var_os(crate::core::config::DAEMON_NAMESPACE_ENV);
        std::env::set_var(crate::core::config::CACHE_DIR_ENV, path);
        std::env::set_var(crate::core::config::DAEMON_NAMESPACE_ENV, namespace);
        Self {
            _lock: lock,
            previous_cache_dir,
            previous_namespace,
        }
    }
}

impl Drop for CacheDirEnvGuard {
    fn drop(&mut self) {
        match &self.previous_cache_dir {
            Some(previous) => std::env::set_var(crate::core::config::CACHE_DIR_ENV, previous),
            None => std::env::remove_var(crate::core::config::CACHE_DIR_ENV),
        }
        match &self.previous_namespace {
            Some(previous) => {
                std::env::set_var(crate::core::config::DAEMON_NAMESPACE_ENV, previous);
            }
            None => std::env::remove_var(crate::core::config::DAEMON_NAMESPACE_ENV),
        }
    }
}
