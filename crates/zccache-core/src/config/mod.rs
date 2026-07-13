//! Configuration types for zccache.
//!
//! This module is split across several files to keep each below the 1,000-LOC
//! cap (see `crates/CLAUDE.md`). The public surface - `Config`, the top-level
//! `pub const` env-var names, the well-known path helpers, the resolve API,
//! the namespace helpers, and the cleanup helpers - is re-exported here so
//! existing facade callers using `zccache::core::config::<Name>` keep compiling
//! unchanged once the root crate re-exports `zccache_core` as `core`.

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

/// Optional explicit root for daemon-owned mutable state. Embedding hosts can
/// set this when the broker has already assigned a stable private directory;
/// it is deliberately separate from `ZCCACHE_CACHE_DIR`, which remains the
/// user-visible top-level root used for endpoint and shared artifact identity.
pub const DAEMON_STATE_DIR_ENV: &str = "ZCCACHE_DAEMON_STATE_DIR";

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

/// Environment variable set by embedding hosts (e.g. soldr's compiled-in
/// `zccache` trampoline, which serves compiles through an embedded
/// in-process zccache service) to forbid the CLI from ever spawning a
/// standalone `zccache-daemon` / `zccache-download-daemon` process
/// (issue #982). Value grammar matches `ZCCACHE_DISABLE`: `1` or
/// case-insensitive `true`. Connecting to an already-running,
/// version-compatible daemon remains allowed — the guard forbids
/// spawning, not talking.
pub const NO_SPAWN_ENV: &str = "ZCCACHE_NO_SPAWN";

/// True when the host forbids standalone daemon spawns via [`NO_SPAWN_ENV`].
#[must_use]
pub fn daemon_spawn_disabled() -> bool {
    no_spawn_from_env_value(std::env::var_os(NO_SPAWN_ENV).as_deref())
}

/// Testable core of [`daemon_spawn_disabled`] — no environment access.
#[must_use]
pub(crate) fn no_spawn_from_env_value(value: Option<&std::ffi::OsStr>) -> bool {
    value
        .map(|v| v.to_string_lossy())
        .is_some_and(|v| v == "1" || v.eq_ignore_ascii_case("true"))
}

/// Standard error for a refused spawn. Names [`NO_SPAWN_ENV`] so operators
/// can find the knob, and points at the embedded service so the failure is
/// self-explaining in host contexts.
#[must_use]
pub fn no_spawn_error(daemon_name: &str) -> String {
    format!(
        "{daemon_name} spawn disabled by host ({NO_SPAWN_ENV}=1); \
         this host serves compiles through an embedded zccache service"
    )
}

// ---------------------------------------------------------------------------
// Re-exports - every public symbol the old single-file module exposed.
// Facade callers use `zccache::core::config::<Name>`, so we keep that path
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
    cache_dir_override, daemon_state_dir, daemon_state_dir_from_cache_dir,
    daemon_state_dir_from_cache_dir_with_namespace, default_cache_dir,
    effective_cache_root_from_top_level, is_version_dir_name, prune_stale_version_dirs,
    prune_stale_version_dirs_in, read_last_version_marker, resolve_cache_root,
    resolve_cache_root_top_level, versioned_subdir, write_last_version_marker,
    write_last_version_marker_in, CacheRootSource, PruneReport, LAST_VERSION_MARKER,
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
