//! Unit tests for `server/` submodules. Originally a single 2.3K-LOC
//! `tests.rs`; split per domain so each file stays well under 1,000 LOC.
//! Each module owns whatever fixture / helper code its tests use, except
//! for the truly cross-module `CacheDirEnvGuard` defined below.

use std::path::Path;

mod cache_trim;
mod client_env;
mod compiler_hash;
mod fingerprint;
mod link_cache;
mod pack;
mod pch;
mod post_link_hook;
mod server_ipc;
mod write_cached;

/// RAII guard that overrides `ZCCACHE_CACHE_DIR` for the duration of a
/// single test, restoring the previous value on drop. Shared between
/// `link_cache` (link side-effects test) and `server_ipc`
/// (`cli_compile_unknown_uuid_is_idempotent`) — both need an isolated
/// per-test cache dir so they don't clash with any production daemon
/// writing the global index blob.
pub(super) struct CacheDirEnvGuard {
    previous: Option<std::ffi::OsString>,
}

impl CacheDirEnvGuard {
    pub(super) fn set(path: &Path) -> Self {
        let previous = std::env::var_os(zccache::core::config::CACHE_DIR_ENV);
        std::env::set_var(zccache::core::config::CACHE_DIR_ENV, path);
        Self { previous }
    }
}

impl Drop for CacheDirEnvGuard {
    fn drop(&mut self) {
        match &self.previous {
            Some(previous) => std::env::set_var(zccache::core::config::CACHE_DIR_ENV, previous),
            None => std::env::remove_var(zccache::core::config::CACHE_DIR_ENV),
        }
    }
}
