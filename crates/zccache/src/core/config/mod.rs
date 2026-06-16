//! Configuration types for zccache.
//!
//! This module is split across several files to keep each below the 1,000-LOC
//! cap (see `crates/CLAUDE.md`). The public surface — `Config`, the top-level
//! `pub const` env-var names, the well-known path helpers, the resolve API,
//! the namespace helpers, and the cleanup helpers — is re-exported here so
//! existing callers using `crate::core::config::<Name>` keep compiling
//! unchanged.

pub mod cleanup;
pub mod namespace;
pub mod paths;
pub mod resolve;

use super::NormalizedPath;

/// Environment variable used to override the zccache cache root.
pub const CACHE_DIR_ENV: &str = "ZCCACHE_CACHE_DIR";

/// Environment variable used to select a daemon/socket namespace.
///
/// The default daemon identity remains unchanged when this is unset or empty.
/// Managed wrappers such as soldr can set a non-empty value (for example
/// `soldr-dev`) to run a development daemon beside the user's normal daemon
/// without sharing IPC endpoints, lock files, or lifecycle logs.
pub const DAEMON_NAMESPACE_ENV: &str = "ZCCACHE_DAEMON_NAMESPACE";

/// Human-readable namespace label reported when no explicit daemon namespace
/// is configured. This label is diagnostic only; default endpoint/path names
/// deliberately do not include it so existing users keep the same layout.
pub const DEFAULT_DAEMON_NAMESPACE: &str = "default";

/// Conventional namespace for zccache daemon development under soldr.
pub const DEV_DAEMON_NAMESPACE: &str = "dev";

/// Default idle timeout in seconds before the daemon auto-shuts down.
///
/// `3600` = 60 minutes. Single source of truth for both
/// [`Config::default`] and the `zccache-daemon` `--idle-timeout` clap
/// argument. Operators can override via the `ZCCACHE_IDLE_TIMEOUT_SECS`
/// env var or the `--idle-timeout` flag; `0` disables auto-shutdown.
pub const DEFAULT_IDLE_TIMEOUT_SECS: u64 = 3600;

/// Environment variable used to opt into co-locating the cache on the
/// project's volume when it differs from the home volume. When set to a
/// non-empty, non-"0" value, `default_cache_dir` returns a path on the
/// CWD's volume so hardlink-based cache writes (see issue #296) succeed
/// without falling back to byte-copy. Unset → behaviour unchanged.
pub const COLOCATE_ENV: &str = "ZCCACHE_COLOCATE";

// ---------------------------------------------------------------------------
// Re-exports — every public symbol the old single-file module exposed.
// External callers use `crate::core::config::<Name>`, so we keep that path
// stable by re-exporting from each submodule here.
// ---------------------------------------------------------------------------

pub use cleanup::{cleanup_legacy_temp_root_state, cleanup_stale_depfile_dirs};
pub use namespace::{
    daemon_namespace, daemon_namespace_label, sanitize_daemon_namespace, sanitize_ipc_component,
};
pub use paths::{
    artifacts_dir, artifacts_dir_from_cache_dir, cargo_registry_cache_dir,
    cargo_registry_cache_dir_from_cache_dir, compiler_hash_cache_path_from_cache_dir,
    crash_dump_dir, depfile_dir, depfile_dir_from_cache_dir, depgraph_dir, index_path,
    index_path_from_cache_dir, log_dir, log_dir_from_cache_dir, metadata_path_from_cache_dir,
    symbols_cache_dir, symbols_cache_dir_from_cache_dir, system_includes_cache_path_from_cache_dir,
    tmp_dir, tmp_dir_from_cache_dir,
};
pub use resolve::{
    cache_dir_override, default_cache_dir, resolve_cache_root, resolve_cache_root_top_level,
    versioned_subdir, CacheRootSource,
};

/// Top-level configuration for zccache.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct Config {
    /// Path to the artifact cache directory.
    pub cache_dir: NormalizedPath,
    /// Maximum artifact cache size in bytes.
    pub max_cache_size: u64,
    /// Daemon idle timeout in seconds before auto-shutdown.
    pub idle_timeout_secs: u64,
    /// Whether to enable the file watcher.
    pub enable_watcher: bool,
    /// Whether to use polling fallback for file watching.
    pub watcher_poll_fallback: bool,
    /// Log level filter (e.g., "info", "debug", "trace").
    pub log_level: String,
    /// Maximum in-memory cache budget in bytes (default: 1 GB).
    pub max_memory_bytes: u64,
    /// How often (in seconds) the memory eviction background task runs (default: 30).
    pub eviction_interval_secs: u64,
    /// How often (in seconds) the disk artifact GC task runs (default: 300).
    pub disk_gc_interval_secs: u64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            cache_dir: default_cache_dir(),
            max_cache_size: 10 * 1024 * 1024 * 1024, // 10 GB
            idle_timeout_secs: DEFAULT_IDLE_TIMEOUT_SECS,
            enable_watcher: true,
            watcher_poll_fallback: false,
            log_level: String::from("info"),
            max_memory_bytes: 1_073_741_824, // 1 GB
            eviction_interval_secs: 30,
            disk_gc_interval_secs: 300,
        }
    }
}

#[cfg(test)]
mod tests;
