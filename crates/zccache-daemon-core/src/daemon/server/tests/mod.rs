//! Unit tests for `server/` submodules. Originally a single 2.3K-LOC
//! `tests.rs`; split per domain so each file stays well under 1,000 LOC.
//! Each module owns whatever fixture / helper code its tests use, except
//! for the truly cross-module `CacheDirEnvGuard` defined below.

use std::path::Path;

mod artifact_store_deferred;
mod cache_trim;
mod clear_handler;
mod client_env;
mod compiler_hash;
mod deferred_cold_path;
mod disk_maintenance;
mod embedded_flush;
mod exec_probe;
mod fingerprint;
mod fs_matrix;
mod link_cache;
mod metadata_deferred;
mod pack;
mod path_remap;
mod pch;
mod pending_cache_writes;
mod post_link_hook;
mod release_worktree_handles;
mod rustc_depinfo;
mod server_ipc;
mod session_errors;
mod session_staged_attribution;
mod staged_compiler_sets;
mod system_includes_deferred;
mod write_cached;

/// RAII guard that overrides `ZCCACHE_CACHE_DIR` for the duration of a
/// single test, restoring the previous value on drop. Shared between
/// `link_cache` (link side-effects test) and `server_ipc`
/// (`cli_compile_unknown_uuid_is_idempotent`) — both need an isolated
/// per-test cache dir so they don't clash with any production daemon
/// writing the global index blob.
pub(super) struct CacheDirEnvGuard {
    _lock: std::sync::MutexGuard<'static, ()>,
    previous_cache_dir: Option<std::ffi::OsString>,
    previous_namespace: Option<std::ffi::OsString>,
}

static CACHE_DIR_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

impl CacheDirEnvGuard {
    pub(super) fn set(path: &Path) -> Self {
        let lock = CACHE_DIR_ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
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
        let lock = CACHE_DIR_ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
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
