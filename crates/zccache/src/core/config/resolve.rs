//! Cache-root resolution: env-var override, colocation, version namespacing.
//!
//! The public surface here is `default_cache_dir`, `resolve_cache_root`,
//! `resolve_cache_root_top_level`, `versioned_subdir`, `cache_dir_override`,
//! and the `CacheRootSource` enum. Everything else is internal plumbing for
//! the cross-volume colocation logic (`ZCCACHE_COLOCATE`, issue #296) and
//! the per-daemon-version subdir layout (issues #761 / #762 Phase 0).

use super::namespace::home_dir_short_hash;
use super::{CACHE_DIR_ENV, COLOCATE_ENV};
use crate::core::NormalizedPath;
use std::ffi::OsString;
use std::path::Path;

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

pub(super) fn default_cache_dir_from_env_value(value: Option<OsString>) -> NormalizedPath {
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

pub(super) fn resolve_cache_root_from_env_value(
    value: Option<OsString>,
) -> (NormalizedPath, CacheRootSource) {
    let (root, source) = resolve_cache_root_top_level_from_env_value(value);
    (root.join(versioned_subdir()), source)
}

/// Issue #761 / meta #762 — Phase 0: shared cache state is now
/// per-daemon-version. The top-level root (`~/.zccache/` or whatever
/// `ZCCACHE_CACHE_DIR` points at) is reserved for advisory cross-version
/// metadata only (e.g. a `last-version.txt` migration breadcrumb); every
/// state file the daemon and CLI persistently read/write lives under
/// `<root>/v<daemon-version>/`. Returns the *top-level* root prior to
/// the version segment — callers that need to drop a cross-version
/// marker file at the root use this, everyone else uses
/// `resolve_cache_root` which appends the version automatically.
#[must_use]
pub fn resolve_cache_root_top_level() -> (NormalizedPath, CacheRootSource) {
    resolve_cache_root_top_level_from_env_value(std::env::var_os(CACHE_DIR_ENV))
}

pub(super) fn resolve_cache_root_top_level_from_env_value(
    value: Option<OsString>,
) -> (NormalizedPath, CacheRootSource) {
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

/// The version-suffix path segment appended to every resolved cache
/// root. Format: `v<VERSION>` (e.g. `v1.12.7`) — leading `v` is
/// intentional so a future `zccache clear` that prunes `^v\d+\.\d+\.\d+$`
/// subdirs (#761 Phase 0 follow-up) can match cleanly. Exposed for
/// tooling that wants to enumerate sibling versions.
#[must_use]
pub fn versioned_subdir() -> String {
    format!("v{}", crate::core::VERSION)
}

/// True when `ZCCACHE_COLOCATE` is set to a non-empty, non-"0" value.
pub(super) fn colocate_enabled() -> bool {
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
pub(super) fn volume_root(path: &Path) -> Option<std::path::PathBuf> {
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
pub(super) fn same_volume_root(a: &Path, b: &Path) -> bool {
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
pub(super) fn sanitize_path_component(s: &str) -> String {
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

/// Returns the cache directory override from `ZCCACHE_CACHE_DIR`, if set.
#[must_use]
pub fn cache_dir_override() -> Option<NormalizedPath> {
    cache_dir_from_env_value(std::env::var_os(CACHE_DIR_ENV))
}

fn dirs_fallback() -> NormalizedPath {
    std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .map(NormalizedPath::from)
        .unwrap_or_else(|_| ".".into())
}

pub(super) fn cache_dir_from_env_value(value: Option<OsString>) -> Option<NormalizedPath> {
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

