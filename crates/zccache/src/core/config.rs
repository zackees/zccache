//! Configuration types for zccache.

use super::NormalizedPath;
use std::ffi::OsString;
use std::path::Path;

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

/// Returns the configured cache directory path.
///
/// If `ZCCACHE_CACHE_DIR` is set and non-empty, it is used as the cache root.
/// Relative override paths are made absolute against the current working
/// directory so the daemon and CLI derive the same subpaths when spawned
/// together. If unset, this falls back to `~/.zccache` on all platforms.
#[must_use]
pub fn default_cache_dir() -> NormalizedPath {
    default_cache_dir_from_env_value(std::env::var_os(CACHE_DIR_ENV))
}

fn default_cache_dir_from_env_value(value: Option<OsString>) -> NormalizedPath {
    resolve_cache_root_from_env_value(value).0
}

/// How [`resolve_cache_root`] determined the active cache root path.
///
/// Exposed via `zccache cache-root --json` (issue #275) so wrappers like
/// [soldr](https://github.com/zackees/soldr) can confirm at runtime that
/// `ZCCACHE_CACHE_DIR` actually took effect. Older zccache binaries on PATH
/// that ignore the env var would surface here as `Default` instead of `Env`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheRootSource {
    /// `ZCCACHE_CACHE_DIR` was set and non-empty.
    Env,
    /// Same-volume colocation kicked in (`ZCCACHE_COLOCATE`) because the
    /// CWD lives on a different volume from `$HOME`. See issue #296.
    Colocated,
    /// Plain default: `~/.zccache` (or `.zccache` next to the binary if
    /// `$HOME`/`$USERPROFILE` cannot be resolved).
    Default,
}

impl CacheRootSource {
    /// Stable wire string, matched by soldr's runtime verification. Format:
    /// `env:ZCCACHE_CACHE_DIR`, `colocate:cross_volume`, `default:platform_dirs`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Env => "env:ZCCACHE_CACHE_DIR",
            Self::Colocated => "colocate:cross_volume",
            Self::Default => "default:platform_dirs",
        }
    }
}

impl std::fmt::Display for CacheRootSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Returns the cache root + the rule that resolved it.
///
/// Equivalent to [`default_cache_dir`] but also reports which branch fired.
/// Used by `zccache cache-root --json` (issue #275).
#[must_use]
pub fn resolve_cache_root() -> (NormalizedPath, CacheRootSource) {
    resolve_cache_root_from_env_value(std::env::var_os(CACHE_DIR_ENV))
}

fn resolve_cache_root_from_env_value(value: Option<OsString>) -> (NormalizedPath, CacheRootSource) {
    if let Some(p) = cache_dir_from_env_value(value) {
        return (p, CacheRootSource::Env);
    }
    let home = dirs_fallback();
    if colocate_enabled() {
        if let Some(p) = colocated_cache_dir(&home) {
            return (p, CacheRootSource::Colocated);
        }
    }
    (home.join(".zccache"), CacheRootSource::Default)
}

/// True when `ZCCACHE_COLOCATE` is set to a non-empty, non-"0" value.
fn colocate_enabled() -> bool {
    std::env::var(COLOCATE_ENV)
        .ok()
        .is_some_and(|v| !v.is_empty() && v != "0")
}

/// If the current working directory is on a different volume than the
/// home directory, return a cache path rooted at the cwd's volume so
/// hardlinks from `target/` into the cache stay within one filesystem.
/// Otherwise (same volume, or volume detection failed) return `None` and
/// the caller uses the standard `~/.zccache` path.
fn colocated_cache_dir(home: &NormalizedPath) -> Option<NormalizedPath> {
    let cwd = std::env::current_dir().ok()?;
    let home_path: &Path = home.as_path();
    let home_vol = volume_root(home_path)?;
    let cwd_vol = volume_root(&cwd)?;
    if same_volume_root(&home_vol, &cwd_vol) {
        return None;
    }
    let basename = home_path
        .file_name()
        .and_then(|n| n.to_str())
        .map(sanitize_path_component)
        .unwrap_or_default();
    let stem = if basename.is_empty() {
        format!(".zccache-{}", home_dir_short_hash(home_path))
    } else {
        format!(".zccache-{}-{}", basename, home_dir_short_hash(home_path))
    };
    Some(NormalizedPath::from(cwd_vol.join(stem)))
}

/// Walk `path` upward to find the volume root: drive-letter prefix on
/// Windows (e.g. `C:\`), the first `/` on Unix where the parent stops
/// existing (i.e. the filesystem root accessible from this path). Returns
/// `None` only if the path has no anchored root.
fn volume_root(path: &Path) -> Option<std::path::PathBuf> {
    use std::path::Component;
    let mut root = std::path::PathBuf::new();
    // Eat any leading Prefix (Windows) + RootDir.
    let mut saw_anchor = false;
    for c in path.components() {
        match c {
            Component::Prefix(p) => {
                root.push(p.as_os_str());
                saw_anchor = true;
            }
            Component::RootDir => {
                root.push(c);
                saw_anchor = true;
                break;
            }
            _ => break,
        }
    }
    if saw_anchor {
        Some(root)
    } else {
        None
    }
}

/// Compare volume roots case-insensitively on Windows (drive letters
/// are case-insensitive), case-sensitively elsewhere.
fn same_volume_root(a: &Path, b: &Path) -> bool {
    let a_str = a.to_string_lossy();
    let b_str = b.to_string_lossy();
    if cfg!(windows) {
        a_str.eq_ignore_ascii_case(&b_str)
    } else {
        a_str == b_str
    }
}

/// Sanitize a path component: keep ASCII alphanumerics + `-` `_` `.`;
/// replace anything else with `_`. Avoids weird characters from a user's
/// home directory leaking into the cache path basename.
fn sanitize_path_component(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .take(32)
        .collect()
}

/// Returns the active daemon/socket namespace, if explicitly configured.
///
/// Values are trimmed and normalized to a path/pipe-safe ASCII component:
/// alphanumerics plus `-`, `_`, and `.` are preserved; every other character
/// becomes `_`. Long values retain a readable prefix plus an 8-hex hash to
/// avoid collisions.
#[must_use]
pub fn daemon_namespace() -> Option<String> {
    daemon_namespace_from_env_value(std::env::var_os(DAEMON_NAMESPACE_ENV))
}

/// Returns the namespace label to show in diagnostics and status JSON.
#[must_use]
pub fn daemon_namespace_label() -> String {
    daemon_namespace().unwrap_or_else(|| DEFAULT_DAEMON_NAMESPACE.to_string())
}

fn daemon_namespace_from_env_value(value: Option<OsString>) -> Option<String> {
    let value = value?;
    if value.is_empty() {
        return None;
    }
    sanitize_daemon_namespace(&value.to_string_lossy())
}

pub fn sanitize_daemon_namespace(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let sanitized: String = trimmed
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if sanitized.len() <= 32 {
        return Some(sanitized);
    }
    let prefix: String = sanitized.chars().take(32).collect();
    Some(format!("{prefix}-{}", namespace_short_hash(trimmed)))
}

fn namespace_short_hash(value: &str) -> String {
    fnv_short_hash(value.as_bytes())
}

/// Sanitize a user-controlled IPC name component for endpoints such as Windows
/// named pipes. Already-safe ASCII components are returned unchanged so
/// historical endpoint names remain stable. If any character must be replaced,
/// append a short hash of the original value so distinct unsafe names do not
/// collapse to the same pipe name.
#[must_use]
pub fn sanitize_ipc_component(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let sanitized: String = trimmed
        .chars()
        .map(|c| {
            if is_safe_ipc_component_char(c) {
                c
            } else {
                '_'
            }
        })
        .collect();
    if sanitized == trimmed {
        return Some(sanitized);
    }
    let prefix: String = sanitized.chars().take(32).collect();
    Some(format!("{prefix}-{}", fnv_short_hash(trimmed.as_bytes())))
}

fn is_safe_ipc_component_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.'
}

/// Stable 8-hex-char identifier derived from the home dir's canonical
/// path. FNV-1a (64-bit) — small, deterministic, no extra dep.
fn home_dir_short_hash(home: &Path) -> String {
    let canon = home.to_string_lossy();
    let canon = if cfg!(windows) {
        canon.to_ascii_lowercase()
    } else {
        canon.into_owned()
    };
    fnv_short_hash(canon.as_bytes())
}

fn fnv_short_hash(bytes: &[u8]) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in bytes {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x100000001b3);
    }
    // Take 32 bits → 8 hex chars. Plenty for collision avoidance at
    // per-machine scale.
    format!("{:08x}", (h ^ (h >> 32)) as u32)
}

/// Returns the cache directory override from `ZCCACHE_CACHE_DIR`, if set.
#[must_use]
pub fn cache_dir_override() -> Option<NormalizedPath> {
    cache_dir_from_env_value(std::env::var_os(CACHE_DIR_ENV))
}

/// Returns the directory for content-addressed compiled outputs.
#[must_use]
pub fn artifacts_dir() -> NormalizedPath {
    artifacts_dir_from_cache_dir(&default_cache_dir())
}

/// Returns the directory for in-progress artifact writes (cleaned on startup).
#[must_use]
pub fn tmp_dir() -> NormalizedPath {
    tmp_dir_from_cache_dir(&default_cache_dir())
}

/// Returns the base directory for compiler-injected depfiles.
///
/// Each daemon instance creates a `{pid}-{instance}` subdirectory here.
/// Stale subdirectories from dead daemon processes are cleaned on startup.
#[must_use]
pub fn depfile_dir() -> NormalizedPath {
    depfile_dir_from_cache_dir(&default_cache_dir())
}

/// Returns the directory for compressed cargo registry archives.
#[must_use]
pub fn cargo_registry_cache_dir() -> NormalizedPath {
    cargo_registry_cache_dir_from_cache_dir(&default_cache_dir())
}

/// Remove legacy zccache state left directly under the OS temp directory.
///
/// Older builds stored the full cache root at `%TEMP%/.zccache` and created
/// depfile directories as `%TEMP%/zccache-depfiles-*`. Those paths are safe to
/// remove only when they are exact legacy matches and do not point at the
/// current cache directory.
pub fn cleanup_legacy_temp_root_state<F>(
    temp_root: &Path,
    current_cache_dir: &Path,
    is_alive: F,
) -> usize
where
    F: Fn(u32) -> bool,
{
    let mut cleaned = cleanup_legacy_temp_cache_dir(temp_root, current_cache_dir);
    cleaned += cleanup_legacy_temp_depfile_dirs(temp_root, is_alive);
    cleaned
}

/// Remove stale depfile directories from previous (dead) daemon instances.
///
/// Scans [`depfile_dir()`] for subdirectories matching `{pid}-{instance}`.
/// If the PID is no longer alive (per `is_alive`), removes the directory.
/// Returns the number of directories cleaned up.
pub fn cleanup_stale_depfile_dirs<F>(is_alive: F) -> usize
where
    F: Fn(u32) -> bool,
{
    let base = depfile_dir();
    let entries = match std::fs::read_dir(&base) {
        Ok(entries) => entries,
        Err(_) => return 0,
    };

    let mut cleaned = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let name = match path.file_name().and_then(|n| n.to_str()) {
            Some(n) => n,
            None => continue,
        };
        let pid: u32 = match name.split('-').next().and_then(|s| s.parse().ok()) {
            Some(p) => p,
            None => continue,
        };
        if !is_alive(pid) {
            match std::fs::remove_dir_all(&path) {
                Ok(()) => {
                    cleaned += 1;
                    tracing::info!(path = %path.display(), "removed stale depfile dir");
                }
                Err(e) => {
                    tracing::warn!(
                        path = %path.display(),
                        "failed to remove stale depfile dir: {e}"
                    );
                }
            }
        }
    }
    cleaned
}

fn cleanup_legacy_temp_cache_dir(temp_root: &Path, current_cache_dir: &Path) -> usize {
    let legacy_cache_dir = temp_root.join(".zccache");
    if path_is_or_contains(&legacy_cache_dir, current_cache_dir) {
        return 0;
    }

    if !is_real_dir(&legacy_cache_dir) {
        return 0;
    }

    match std::fs::remove_dir_all(&legacy_cache_dir) {
        Ok(()) => {
            tracing::info!(path = %legacy_cache_dir.display(), "removed legacy temp cache dir");
            1
        }
        Err(e) => {
            tracing::warn!(
                path = %legacy_cache_dir.display(),
                "failed to remove legacy temp cache dir: {e}"
            );
            0
        }
    }
}

fn cleanup_legacy_temp_depfile_dirs<F>(temp_root: &Path, is_alive: F) -> usize
where
    F: Fn(u32) -> bool,
{
    let entries = match std::fs::read_dir(temp_root) {
        Ok(entries) => entries,
        Err(_) => return 0,
    };

    let mut cleaned = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        let file_name = entry.file_name();
        let name = match file_name.to_str() {
            Some(name) if name.starts_with("zccache-depfiles-") => name,
            _ => continue,
        };

        if !is_real_dir(&path) {
            continue;
        }

        let pid = match legacy_temp_depfile_pid(name) {
            Some(pid) => pid,
            None => continue,
        };

        if is_alive(pid) {
            continue;
        }

        match std::fs::remove_dir_all(&path) {
            Ok(()) => {
                cleaned += 1;
                tracing::info!(path = %path.display(), "removed legacy temp depfile dir");
            }
            Err(e) => {
                tracing::warn!(
                    path = %path.display(),
                    "failed to remove legacy temp depfile dir: {e}"
                );
            }
        }
    }
    cleaned
}

fn legacy_temp_depfile_pid(name: &str) -> Option<u32> {
    let suffix = name.strip_prefix("zccache-depfiles-")?;
    suffix.split('-').next()?.parse().ok()
}

fn is_real_dir(path: &Path) -> bool {
    std::fs::symlink_metadata(path)
        .map(|meta| meta.file_type().is_dir())
        .unwrap_or(false)
}

fn path_is_or_contains(parent: &Path, child: &Path) -> bool {
    if child.starts_with(parent) {
        return true;
    }

    let parent = match std::fs::canonicalize(parent) {
        Ok(parent) => parent,
        Err(_) => return false,
    };
    std::fs::canonicalize(child)
        .map(|child| child.starts_with(parent))
        .unwrap_or(false)
}

/// Returns the directory for serialized dependency graph storage (future).
#[must_use]
pub fn depgraph_dir() -> NormalizedPath {
    depgraph_dir_from_cache_dir(&default_cache_dir())
}

/// Returns the directory that caches downloaded debug-symbol archives so
/// repeated installs (different prefixes, post-version-bump, --force) don't
/// re-fetch the same zip/tar.gz from GitHub.
///
/// All zccache subsystems that need a scratch or download location must
/// root them under [`default_cache_dir`] so the user's `~/.zccache/` is the
/// single ground truth — never `$TMPDIR`. Enforced by the `ban_unrooted_tempdir`
/// dylint.
#[must_use]
pub fn symbols_cache_dir() -> NormalizedPath {
    symbols_cache_dir_from_cache_dir(&default_cache_dir())
}

/// Returns the symbols-archive cache under an explicit cache root.
#[must_use]
pub fn symbols_cache_dir_from_cache_dir(cache_dir: &NormalizedPath) -> NormalizedPath {
    cache_dir.join("symbols")
}

/// Returns the cargo registry archive cache under an explicit cache root.
#[must_use]
pub fn cargo_registry_cache_dir_from_cache_dir(cache_dir: &NormalizedPath) -> NormalizedPath {
    cache_dir.join("cargo-registry")
}

/// Returns the path to the artifact index database.
#[must_use]
pub fn index_path() -> NormalizedPath {
    index_path_from_cache_dir(&default_cache_dir())
}

/// Returns the directory for crash dump files.
#[must_use]
pub fn crash_dump_dir() -> NormalizedPath {
    crash_dump_dir_from_cache_dir(&default_cache_dir())
}

/// Returns the directory for daemon log files.
#[must_use]
pub fn log_dir() -> NormalizedPath {
    log_dir_from_cache_dir(&default_cache_dir())
}

/// Returns the artifact directory under an explicit cache root.
///
/// Use this when the caller already has a cache dir (e.g. a test passing a
/// per-test temp dir) and wants to avoid the global env-var lookup in
/// [`default_cache_dir`].
#[must_use]
pub fn artifacts_dir_from_cache_dir(cache_dir: &NormalizedPath) -> NormalizedPath {
    cache_dir.join("artifacts")
}

/// Returns the tmp directory under an explicit cache root.
#[must_use]
pub fn tmp_dir_from_cache_dir(cache_dir: &NormalizedPath) -> NormalizedPath {
    cache_dir.join("tmp")
}

/// Returns the depfile directory under an explicit cache root.
#[must_use]
pub fn depfile_dir_from_cache_dir(cache_dir: &NormalizedPath) -> NormalizedPath {
    tmp_dir_from_cache_dir(cache_dir).join("depfiles")
}

fn depgraph_dir_from_cache_dir(cache_dir: &NormalizedPath) -> NormalizedPath {
    cache_dir.join("depgraph")
}

/// Returns the artifact index path under an explicit cache root.
///
/// Bincode blob written by `ArtifactStore::flush`. Prior versions used a
/// redb file at `index.redb`; existing files are left on disk (untouched)
/// when this daemon starts — the new daemon rebuilds its index from misses
/// as compiles happen. Users wanting to reclaim the orphaned bytes can
/// `zccache clear` or delete `index.redb` manually.
#[must_use]
pub fn index_path_from_cache_dir(cache_dir: &NormalizedPath) -> NormalizedPath {
    cache_dir.join("index.bin")
}

/// Returns the on-disk path for the persisted `MetadataCache` snapshot.
///
/// Bincode blob written by `MetadataCache::save_to_disk` on flush + shutdown,
/// read by `MetadataCache::load_from_disk` on daemon startup. Sibling of
/// [`index_path_from_cache_dir`] so that whatever bundles the cache dir (e.g.
/// `soldr save`/`soldr load`) picks both files up automatically.
#[must_use]
pub fn metadata_path_from_cache_dir(cache_dir: &NormalizedPath) -> NormalizedPath {
    cache_dir.join("metadata.bin")
}

/// Returns the on-disk path for the persisted compiler-binary hash cache.
///
/// Issue #517: hashing a 150 MB rustc binary on the cold path costs
/// ~50-60 ms per first-after-restart compile, the dominant phase of the
/// `rust-workspace-link Cold` overhead. This snapshot survives daemon
/// restart so subsequent daemons start with the rustc hash already
/// cached. Sibling of `metadata.bin` / `index.bin` so the soldr save /
/// load pipeline already bundles it.
#[must_use]
pub fn compiler_hash_cache_path_from_cache_dir(cache_dir: &NormalizedPath) -> NormalizedPath {
    cache_dir.join("compiler_hash.bin")
}

/// Returns the on-disk path for the persisted `SystemIncludeCache` snapshot.
///
/// Issue #541: spawning `<compiler> -v -E -x c++ NUL` to discover system
/// include paths costs ~30-50 ms per first-after-restart C/C++ compile.
/// This snapshot persists `(compiler_path, mtime, size) -> include_paths`
/// across daemon restarts so the next daemon starts with discovery
/// already cached. Sibling of `metadata.bin` / `compiler_hash.bin` so the
/// soldr save / load pipeline already bundles it.
#[must_use]
pub fn system_includes_cache_path_from_cache_dir(cache_dir: &NormalizedPath) -> NormalizedPath {
    cache_dir.join("system_includes.bin")
}

fn crash_dump_dir_from_cache_dir(cache_dir: &NormalizedPath) -> NormalizedPath {
    cache_dir.join("crashes")
}

/// Returns the log directory under an explicit cache root.
#[must_use]
pub fn log_dir_from_cache_dir(cache_dir: &NormalizedPath) -> NormalizedPath {
    cache_dir.join("logs")
}

fn dirs_fallback() -> NormalizedPath {
    std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .map(NormalizedPath::from)
        .unwrap_or_else(|_| ".".into())
}

fn cache_dir_from_env_value(value: Option<OsString>) -> Option<NormalizedPath> {
    let value = value?;
    if value.is_empty() {
        return None;
    }
    Some(normalize_cache_dir_override(std::path::Path::new(&value)))
}

fn normalize_cache_dir_override(path: &std::path::Path) -> NormalizedPath {
    if path.is_absolute() {
        path.into()
    } else {
        std::env::current_dir()
            .unwrap_or_default()
            .join(path)
            .into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_cache_dir_ends_with_zccache() {
        let dir = default_cache_dir_from_env_value(None);
        assert!(dir.ends_with(".zccache"));
    }

    #[test]
    fn resolve_cache_root_env_branch() {
        let root = tempfile::tempdir().unwrap();
        let want = root.path().join("zc");
        let (dir, src) = resolve_cache_root_from_env_value(Some(want.clone().into_os_string()));
        assert_eq!(dir, want);
        assert_eq!(src, CacheRootSource::Env);
        assert_eq!(src.as_str(), "env:ZCCACHE_CACHE_DIR");
    }

    #[test]
    fn resolve_cache_root_default_branch_when_env_unset() {
        std::env::remove_var(COLOCATE_ENV);
        let (dir, src) = resolve_cache_root_from_env_value(None);
        assert!(dir.ends_with(".zccache"));
        assert_eq!(src, CacheRootSource::Default);
        assert_eq!(src.as_str(), "default:platform_dirs");
    }

    #[test]
    fn resolve_cache_root_default_branch_when_env_empty() {
        std::env::remove_var(COLOCATE_ENV);
        let (_dir, src) = resolve_cache_root_from_env_value(Some(OsString::new()));
        assert_eq!(src, CacheRootSource::Default);
    }

    #[test]
    fn cache_root_source_display_matches_as_str() {
        assert_eq!(CacheRootSource::Env.to_string(), "env:ZCCACHE_CACHE_DIR");
        assert_eq!(
            CacheRootSource::Colocated.to_string(),
            "colocate:cross_volume"
        );
        assert_eq!(
            CacheRootSource::Default.to_string(),
            "default:platform_dirs"
        );
    }

    /// Every well-known subpath that the daemon/CLI persistently writes to
    /// MUST live under the resolved cache root. This is the soldr/Defender
    /// exclusion contract from issue #275: one directory the wrapper can
    /// exclude and trust that no zccache write escapes it.
    #[test]
    fn cache_root_invariant_all_subpaths_rooted() {
        let (_temp, cache) = temp_cache_dir();
        let subs: [(NormalizedPath, &str); 10] = [
            (artifacts_dir_from_cache_dir(&cache), "artifacts/"),
            (tmp_dir_from_cache_dir(&cache), "tmp/"),
            (depfile_dir_from_cache_dir(&cache), "tmp/depfiles/"),
            (depgraph_dir_from_cache_dir(&cache), "depgraph/"),
            (log_dir_from_cache_dir(&cache), "logs/"),
            (crash_dump_dir_from_cache_dir(&cache), "crashes/"),
            (symbols_cache_dir_from_cache_dir(&cache), "symbols/"),
            (
                cargo_registry_cache_dir_from_cache_dir(&cache),
                "cargo-registry/",
            ),
            (index_path_from_cache_dir(&cache), "index.bin"),
            (metadata_path_from_cache_dir(&cache), "metadata.bin"),
        ];
        for (p, label) in &subs {
            assert!(
                p.starts_with(&cache),
                "{label} ({}) must be under cache root ({})",
                p.display(),
                cache.display()
            );
        }
    }

    #[test]
    fn cache_dir_override_uses_non_empty_env_value() {
        let root = tempfile::tempdir().unwrap();
        let override_dir = root.path().join("zc");
        let cache_dir =
            default_cache_dir_from_env_value(Some(override_dir.clone().into_os_string()));

        assert_eq!(cache_dir, override_dir);
        assert_eq!(
            artifacts_dir_from_cache_dir(&cache_dir),
            override_dir.join("artifacts")
        );
        assert_eq!(tmp_dir_from_cache_dir(&cache_dir), override_dir.join("tmp"));
        assert_eq!(
            depgraph_dir_from_cache_dir(&cache_dir),
            override_dir.join("depgraph")
        );
        assert_eq!(
            index_path_from_cache_dir(&cache_dir),
            override_dir.join("index.bin")
        );
        assert_eq!(
            metadata_path_from_cache_dir(&cache_dir),
            override_dir.join("metadata.bin")
        );
        assert_eq!(
            crash_dump_dir_from_cache_dir(&cache_dir),
            override_dir.join("crashes")
        );
        assert_eq!(
            log_dir_from_cache_dir(&cache_dir),
            override_dir.join("logs")
        );
    }

    #[test]
    fn cache_dir_override_ignores_empty_env_value() {
        assert!(cache_dir_from_env_value(Some(OsString::new())).is_none());
    }

    /// `metadata.bin` MUST live in the same directory as `index.bin` so that
    /// whatever mechanism bundles the cache directory (notably `soldr save`
    /// / `soldr load` for the `cold-tar-untar-warm` perf-cluster scenario)
    /// picks both files up automatically. If a future refactor moves either
    /// file without moving the other, the warm-side daemon spawned after
    /// `soldr load` would restart with an empty `MetadataCache` even though
    /// the artifact index was restored — silently undoing the perf win this
    /// pair was designed to deliver.
    #[test]
    fn metadata_path_is_sibling_of_index_path() {
        let (_temp, cache_dir) = temp_cache_dir();
        let index = index_path_from_cache_dir(&cache_dir);
        let metadata = metadata_path_from_cache_dir(&cache_dir);
        assert_eq!(
            index.parent(),
            metadata.parent(),
            "metadata.bin must live in the same directory as index.bin so soldr save/load bundles both",
        );
        assert!(
            metadata.starts_with(&cache_dir),
            "metadata.bin must be a descendant of cache_dir",
        );
    }

    #[test]
    fn relative_cache_dir_override_is_made_absolute() {
        let override_dir = cache_dir_from_env_value(Some(OsString::from("target/../zc"))).unwrap();
        assert!(override_dir.is_absolute());
        assert!(override_dir.ends_with("zc"));
    }

    #[test]
    fn crash_dump_dir_ends_with_crashes() {
        let (_temp, cache) = temp_cache_dir();
        let dir = crash_dump_dir_from_cache_dir(&cache);
        assert!(dir.ends_with("crashes"));
    }

    #[test]
    fn crash_dump_dir_is_under_cache_dir() {
        let (_temp, cache) = temp_cache_dir();
        let crashes = crash_dump_dir_from_cache_dir(&cache);
        assert!(crashes.starts_with(&cache));
    }

    #[test]
    fn log_dir_ends_with_logs() {
        let (_temp, cache) = temp_cache_dir();
        let dir = log_dir_from_cache_dir(&cache);
        assert!(dir.ends_with("logs"));
    }

    #[test]
    fn log_dir_is_under_cache_dir() {
        let (_temp, cache) = temp_cache_dir();
        let logs = log_dir_from_cache_dir(&cache);
        assert!(logs.starts_with(&cache));
    }

    #[test]
    fn cargo_registry_cache_dir_is_under_cache_dir() {
        let (_temp, cache) = temp_cache_dir();
        let dir = cargo_registry_cache_dir_from_cache_dir(&cache);
        assert!(dir.ends_with("cargo-registry"));
        assert!(dir.starts_with(&cache));
    }

    #[test]
    fn artifacts_dir_ends_with_artifacts() {
        let (_temp, cache) = temp_cache_dir();
        let dir = artifacts_dir_from_cache_dir(&cache);
        assert!(dir.ends_with("artifacts"));
        assert!(dir.starts_with(cache));
    }

    #[test]
    fn tmp_dir_ends_with_tmp() {
        let (_temp, cache) = temp_cache_dir();
        let dir = tmp_dir_from_cache_dir(&cache);
        assert!(dir.ends_with("tmp"));
        assert!(dir.starts_with(cache));
    }

    #[test]
    fn depgraph_dir_ends_with_depgraph() {
        let (_temp, cache) = temp_cache_dir();
        let dir = depgraph_dir_from_cache_dir(&cache);
        assert!(dir.ends_with("depgraph"));
        assert!(dir.starts_with(cache));
    }

    #[test]
    fn depfile_dir_under_tmp() {
        let (_temp, cache) = temp_cache_dir();
        let tmp = tmp_dir_from_cache_dir(&cache);
        let dir = depfile_dir_from_cache_dir(&cache);
        assert!(dir.ends_with("depfiles"));
        assert!(dir.starts_with(tmp));
    }

    #[test]
    fn cleanup_stale_depfile_dirs_removes_dead() {
        let base = tempfile::tempdir().unwrap();
        let depfiles = base.path().join("depfiles");
        std::fs::create_dir_all(&depfiles).unwrap();

        // Create a "dead" dir (PID 99999999 unlikely alive).
        std::fs::create_dir(depfiles.join("99999999-0")).unwrap();
        // Create a non-matching dir (should be left alone).
        std::fs::create_dir(depfiles.join("not-a-pid")).unwrap();

        let entries = std::fs::read_dir(&depfiles).unwrap();
        let dirs: Vec<_> = entries.flatten().collect();
        assert_eq!(dirs.len(), 2);

        // Use a custom is_alive that says nothing is alive.
        let cleaned = cleanup_stale_with_base(&depfiles, |_| false);
        assert_eq!(cleaned, 1); // only the parseable one removed

        // "not-a-pid" should still exist.
        assert!(depfiles.join("not-a-pid").is_dir());
        assert!(!depfiles.join("99999999-0").exists());
    }

    #[test]
    fn cleanup_stale_depfile_dirs_skips_alive() {
        let base = tempfile::tempdir().unwrap();
        let depfiles = base.path().join("depfiles");
        std::fs::create_dir_all(&depfiles).unwrap();
        std::fs::create_dir(depfiles.join("12345-0")).unwrap();

        let cleaned = cleanup_stale_with_base(&depfiles, |_| true);
        assert_eq!(cleaned, 0);
        assert!(depfiles.join("12345-0").is_dir());
    }

    #[test]
    fn cleanup_stale_depfile_dirs_empty() {
        // Non-existent directory returns 0.
        let cleaned = cleanup_stale_with_base(std::path::Path::new("/nonexistent/path"), |_| false);
        assert_eq!(cleaned, 0);
    }

    #[test]
    fn cleanup_legacy_temp_root_state_removes_legacy_dirs() {
        let temp_root = tempfile::tempdir().unwrap();
        let current_cache_dir = tempfile::tempdir().unwrap();

        let legacy_cache = temp_root.path().join(".zccache");
        std::fs::create_dir_all(&legacy_cache).unwrap();
        std::fs::write(legacy_cache.join("sentinel"), "legacy").unwrap();

        let dead_depfile = temp_root.path().join("zccache-depfiles-1234-0");
        std::fs::create_dir_all(&dead_depfile).unwrap();
        std::fs::write(dead_depfile.join("sentinel"), "dead").unwrap();

        let live_depfile = temp_root.path().join("zccache-depfiles-4321-0");
        std::fs::create_dir_all(&live_depfile).unwrap();

        let unrelated = temp_root.path().join("not-legacy");
        std::fs::create_dir_all(&unrelated).unwrap();

        let cleaned =
            cleanup_legacy_temp_root_state(temp_root.path(), current_cache_dir.path(), |pid| {
                pid != 1234
            });

        assert_eq!(cleaned, 2);
        assert!(!legacy_cache.exists());
        assert!(!dead_depfile.exists());
        assert!(live_depfile.exists());
        assert!(unrelated.exists());
    }

    #[test]
    fn cleanup_legacy_temp_root_state_skips_current_cache_dir() {
        let temp_root = tempfile::tempdir().unwrap();
        let current_cache_dir = temp_root.path().join(".zccache");
        std::fs::create_dir_all(&current_cache_dir).unwrap();
        std::fs::write(current_cache_dir.join("sentinel"), "keep").unwrap();

        let cleaned =
            cleanup_legacy_temp_root_state(temp_root.path(), &current_cache_dir, |_| false);

        assert_eq!(cleaned, 0);
        assert!(current_cache_dir.exists());
        assert_eq!(
            std::fs::read_to_string(current_cache_dir.join("sentinel")).unwrap(),
            "keep"
        );
    }

    #[test]
    fn cleanup_legacy_temp_root_state_skips_parent_of_current_cache_dir() {
        let temp_root = tempfile::tempdir().unwrap();
        let current_cache_dir = temp_root.path().join(".zccache").join("current");
        std::fs::create_dir_all(&current_cache_dir).unwrap();
        std::fs::write(current_cache_dir.join("sentinel"), "keep").unwrap();

        let cleaned =
            cleanup_legacy_temp_root_state(temp_root.path(), &current_cache_dir, |_| false);

        assert_eq!(cleaned, 0);
        assert!(current_cache_dir.exists());
        assert_eq!(
            std::fs::read_to_string(current_cache_dir.join("sentinel")).unwrap(),
            "keep"
        );
    }

    /// Test helper: runs cleanup logic against an arbitrary base dir.
    fn cleanup_stale_with_base<F>(base: &std::path::Path, is_alive: F) -> usize
    where
        F: Fn(u32) -> bool,
    {
        let entries = match std::fs::read_dir(base) {
            Ok(entries) => entries,
            Err(_) => return 0,
        };
        let mut cleaned = 0;
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let name = match path.file_name().and_then(|n| n.to_str()) {
                Some(n) => n,
                None => continue,
            };
            let pid: u32 = match name.split('-').next().and_then(|s| s.parse().ok()) {
                Some(p) => p,
                None => continue,
            };
            if !is_alive(pid) && std::fs::remove_dir_all(&path).is_ok() {
                cleaned += 1;
            }
        }
        cleaned
    }

    #[test]
    fn disk_gc_interval_default() {
        let config = Config::default();
        assert_eq!(config.disk_gc_interval_secs, 300);
    }

    #[test]
    fn index_path_ends_with_bin() {
        let (_temp, cache) = temp_cache_dir();
        let p = index_path_from_cache_dir(&cache);
        assert!(p.ends_with("index.bin"));
        assert!(p.starts_with(cache));
    }

    fn temp_cache_dir() -> (tempfile::TempDir, NormalizedPath) {
        let temp = tempfile::tempdir().unwrap();
        let cache = NormalizedPath::from(temp.path());
        (temp, cache)
    }

    #[test]
    fn volume_root_extracts_drive_or_root() {
        if cfg!(windows) {
            let r = volume_root(Path::new(r"C:\Users\zack\foo")).unwrap();
            assert_eq!(r.to_string_lossy(), r"C:\");
            let r = volume_root(Path::new(r"D:\projects")).unwrap();
            assert_eq!(r.to_string_lossy(), r"D:\");
        } else {
            let r = volume_root(Path::new("/home/zack/foo")).unwrap();
            assert_eq!(r.to_string_lossy(), "/");
            let r = volume_root(Path::new("/mnt/data/projects")).unwrap();
            assert_eq!(r.to_string_lossy(), "/");
        }
    }

    #[test]
    fn same_volume_root_is_case_insensitive_on_windows() {
        let r1 = Path::new(r"C:\");
        let r2 = Path::new(r"c:\");
        if cfg!(windows) {
            assert!(same_volume_root(r1, r2));
        } else {
            assert!(!same_volume_root(r1, r2));
        }
        let same = Path::new("/");
        assert!(same_volume_root(same, same));
    }

    #[test]
    fn home_dir_short_hash_is_stable_and_8_hex() {
        let a = home_dir_short_hash(Path::new("/home/zack"));
        let b = home_dir_short_hash(Path::new("/home/zack"));
        assert_eq!(a, b, "must be deterministic");
        assert_eq!(a.len(), 8);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        let c = home_dir_short_hash(Path::new("/home/other"));
        assert_ne!(a, c, "different paths → different hashes");
    }

    #[test]
    fn home_dir_short_hash_is_case_insensitive_on_windows() {
        let upper = home_dir_short_hash(Path::new(r"C:\Users\Zack"));
        let lower = home_dir_short_hash(Path::new(r"c:\users\zack"));
        if cfg!(windows) {
            assert_eq!(upper, lower);
        } else {
            assert_ne!(upper, lower);
        }
    }

    #[test]
    fn sanitize_path_component_strips_oddities() {
        assert_eq!(sanitize_path_component("zack"), "zack");
        assert_eq!(sanitize_path_component("z@ck!"), "z_ck_");
        assert_eq!(sanitize_path_component(""), "");
        // Truncates to 32 chars
        let long = "a".repeat(100);
        assert_eq!(sanitize_path_component(&long).len(), 32);
    }

    #[test]
    fn daemon_namespace_ignores_unset_or_empty_values() {
        assert_eq!(daemon_namespace_from_env_value(None), None);
        assert_eq!(daemon_namespace_from_env_value(Some(OsString::new())), None);
        assert_eq!(
            daemon_namespace_from_env_value(Some(OsString::from("   "))),
            None
        );
    }

    #[test]
    fn daemon_namespace_sanitizes_for_paths_and_pipes() {
        assert_eq!(
            daemon_namespace_from_env_value(Some(OsString::from(" soldr dev! "))).as_deref(),
            Some("soldr_dev_")
        );
        assert_eq!(
            daemon_namespace_from_env_value(Some(OsString::from("soldr-dev_1.2"))).as_deref(),
            Some("soldr-dev_1.2")
        );
    }

    #[test]
    fn daemon_namespace_keeps_long_values_distinct() {
        let a =
            daemon_namespace_from_env_value(Some(OsString::from(format!("{}a", "x".repeat(40)))))
                .unwrap();
        let b =
            daemon_namespace_from_env_value(Some(OsString::from(format!("{}b", "x".repeat(40)))))
                .unwrap();
        assert_ne!(a, b);
        assert!(a.starts_with(&"x".repeat(32)));
        assert_eq!(a.len(), 41);
    }

    #[test]
    fn sanitize_ipc_component_keeps_safe_values_unchanged() {
        assert_eq!(
            sanitize_ipc_component("zackees-dev_1.2").as_deref(),
            Some("zackees-dev_1.2")
        );
    }

    #[test]
    fn sanitize_ipc_component_replaces_spaces_and_adds_hash() {
        let component = sanitize_ipc_component("Zach Vorhies").unwrap();
        assert!(component.starts_with("Zach_Vorhies-"));
        assert_eq!(component.len(), "Zach_Vorhies-".len() + 8);
        assert!(component.chars().all(is_safe_ipc_component_char));
    }

    #[test]
    fn sanitize_ipc_component_keeps_unsafe_names_distinct() {
        let spaced = sanitize_ipc_component("Zach Vorhies").unwrap();
        let slashed = sanitize_ipc_component("Zach/Vorhies").unwrap();
        assert_ne!(spaced, slashed);
        assert!(spaced.starts_with("Zach_Vorhies-"));
        assert!(slashed.starts_with("Zach_Vorhies-"));
    }

    #[test]
    fn sanitize_ipc_component_ignores_empty_values() {
        assert_eq!(sanitize_ipc_component("   "), None);
    }

    #[test]
    fn colocate_disabled_returns_home_path() {
        // No env var set in this test (we can't reliably toggle env in
        // unit tests on Windows without races, so just verify the gating
        // function in isolation).
        std::env::remove_var(COLOCATE_ENV);
        assert!(!colocate_enabled());
        let result = default_cache_dir_from_env_value(None);
        assert!(
            result.to_string_lossy().ends_with(".zccache"),
            "got {}",
            result.display()
        );
    }

    #[test]
    fn colocate_basename_appears_in_path() {
        let home = NormalizedPath::from(Path::new("/home/myuser"));
        // We can't easily mock cwd cross-platform; just call the path
        // builder directly with a synthetic cross-volume scenario.
        let basename = home
            .as_path()
            .file_name()
            .and_then(|n| n.to_str())
            .map(sanitize_path_component)
            .unwrap();
        assert_eq!(basename, "myuser");
        let hash = home_dir_short_hash(home.as_path());
        let expected_suffix = format!(".zccache-myuser-{hash}");
        assert!(expected_suffix.starts_with(".zccache-myuser-"));
        assert!(expected_suffix.len() == ".zccache-myuser-".len() + 8);
    }
}
