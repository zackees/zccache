//! Cache-root resolution: env-var override, colocation, version namespacing.
//!
//! The public surface here is `default_cache_dir`, `resolve_cache_root`,
//! `resolve_cache_root_top_level`, `versioned_subdir`, `cache_dir_override`,
//! and the `CacheRootSource` enum. Everything else is internal plumbing for
//! the cross-volume colocation logic (`ZCCACHE_COLOCATE`, issue #296) and
//! the per-daemon-version subdir layout (issues #761 / #762 Phase 0).

use super::namespace::home_dir_short_hash;
use super::{CACHE_DIR_ENV, COLOCATE_ENV};
use crate::NormalizedPath;
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
    (effective_cache_root_from_top_level(&root), source)
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
    format!("v{}", crate::VERSION)
}

/// Convert a user-facing cache root into the effective daemon cache root.
///
/// `ZCCACHE_CACHE_DIR` and `--cache-dir` name the top-level root owned by the
/// caller. The daemon's persistent state lives one segment below that root in
/// `v<VERSION>`. If the caller already supplied the effective root, leave it
/// alone so diagnostics, broker manifests, and compatibility paths do not
/// double-append the version segment.
#[must_use]
pub fn effective_cache_root_from_top_level(cache_root: &NormalizedPath) -> NormalizedPath {
    let version = versioned_subdir();
    if cache_root
        .file_name()
        .and_then(|segment| segment.to_str())
        .is_some_and(|segment| segment == version)
    {
        return cache_root.clone();
    }
    cache_root.join(version)
}

/// Advisory top-level marker file recording the last daemon version that
/// bound. Lives at `<top-level>/last-version.txt` — NEVER authoritative for
/// identity (that is the versioned subdir); diagnostics + a warm-start hint
/// only. Issue #1005 / #761 Phase-0 follow-up.
pub const LAST_VERSION_MARKER: &str = "last-version.txt";

/// Write `crate::VERSION` to the advisory `<top-level>/last-version.txt`.
/// Best-effort — the error is returned for the caller to log, never fatal.
pub fn write_last_version_marker() -> std::io::Result<()> {
    write_last_version_marker_in(resolve_cache_root_top_level().0.as_path())
}

/// Test seam for [`write_last_version_marker`]: write the marker under an
/// explicit top-level dir.
pub fn write_last_version_marker_in(top_level: &std::path::Path) -> std::io::Result<()> {
    std::fs::create_dir_all(top_level)?;
    std::fs::write(
        top_level.join(LAST_VERSION_MARKER),
        format!("{}\n", crate::VERSION),
    )
}

/// Read the advisory last-version marker, if present and non-empty.
#[must_use]
pub fn read_last_version_marker() -> Option<String> {
    let (top, _) = resolve_cache_root_top_level();
    std::fs::read_to_string(top.join(LAST_VERSION_MARKER).as_path())
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// True if `name` is a versioned cache subdir (`v<major>.<minor>.<patch>`),
/// the shape [`versioned_subdir`] produces. Used to identify prunable siblings
/// without pulling in a regex dependency.
#[must_use]
pub fn is_version_dir_name(name: &str) -> bool {
    let Some(rest) = name.strip_prefix('v') else {
        return false;
    };
    let parts: Vec<&str> = rest.split('.').collect();
    parts.len() == 3
        && parts
            .iter()
            .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
}

/// Outcome of [`prune_stale_version_dirs`].
#[derive(Debug, Default, Clone)]
pub struct PruneReport {
    /// Version dirs successfully removed.
    pub removed: Vec<String>,
    /// Version dirs left in place (removal failed — most often a live daemon
    /// on Windows holds its `zccache-daemon.exe` open; retried next prune).
    pub skipped: Vec<String>,
}

/// Prune stale sibling `v<VERSION>` cache dirs under the top-level root,
/// keeping the CURRENT version's dir.
///
/// Conservative + best-effort (issue #1005): a dir whose removal fails — the
/// signal that a live daemon of that version still holds its
/// `zccache-daemon.exe` open (Windows) — is **skipped**, not force-displaced,
/// so `zccache clear` never nukes a running daemon's state and never fails.
/// The skipped dir is reclaimed on a later prune once that daemon exits.
pub fn prune_stale_version_dirs() -> PruneReport {
    prune_stale_version_dirs_in(
        resolve_cache_root_top_level().0.as_path(),
        &versioned_subdir(),
    )
}

/// Test seam for [`prune_stale_version_dirs`].
pub fn prune_stale_version_dirs_in(top_level: &std::path::Path, keep: &str) -> PruneReport {
    let mut report = PruneReport::default();
    let entries = match std::fs::read_dir(top_level) {
        Ok(e) => e,
        Err(_) => return report,
    };
    for entry in entries.flatten() {
        if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        if name == keep || !is_version_dir_name(&name) {
            continue;
        }
        match std::fs::remove_dir_all(entry.path()) {
            Ok(()) => report.removed.push(name),
            Err(_) => report.skipped.push(name),
        }
    }
    report
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

#[cfg(test)]
mod version_hygiene_tests {
    use super::*;

    #[test]
    fn is_version_dir_name_accepts_semver_dirs() {
        assert!(is_version_dir_name("v1.12.15"));
        assert!(is_version_dir_name("v0.0.0"));
        assert!(is_version_dir_name("v10.200.3000"));
    }

    #[test]
    fn is_version_dir_name_rejects_non_version_dirs() {
        assert!(!is_version_dir_name("v1.12")); // only 2 parts
        assert!(!is_version_dir_name("v1.12.15.1")); // 4 parts
        assert!(!is_version_dir_name("1.12.15")); // no leading v
        assert!(!is_version_dir_name("vX.Y.Z")); // non-numeric
        assert!(!is_version_dir_name("v1.12.")); // empty part
        assert!(!is_version_dir_name("logs"));
        assert!(!is_version_dir_name("runtime-binaries"));
        assert!(!is_version_dir_name("v"));
    }

    #[test]
    fn prune_removes_stale_siblings_keeps_current_and_non_version_dirs() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let top = tmp.path();
        for name in ["v1.12.13", "v1.12.14", "v1.12.15", "logs", "crashes"] {
            std::fs::create_dir_all(top.join(name)).unwrap();
            std::fs::write(top.join(name).join("marker"), b"x").unwrap();
        }
        // Keep v1.12.15 (the "current" version); prune the two older siblings.
        let report = prune_stale_version_dirs_in(top, "v1.12.15");

        let mut removed = report.removed.clone();
        removed.sort();
        assert_eq!(removed, vec!["v1.12.13", "v1.12.14"]);
        assert!(report.skipped.is_empty());

        assert!(top.join("v1.12.15").is_dir(), "current version kept");
        assert!(top.join("logs").is_dir(), "non-version dir kept");
        assert!(top.join("crashes").is_dir(), "non-version dir kept");
        assert!(!top.join("v1.12.13").exists());
        assert!(!top.join("v1.12.14").exists());
    }

    #[test]
    fn write_last_version_marker_writes_current_version() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let top = tmp.path().join("nested-top");
        write_last_version_marker_in(&top).expect("marker write");
        let contents = std::fs::read_to_string(top.join(LAST_VERSION_MARKER)).expect("read marker");
        assert_eq!(contents.trim(), crate::VERSION);
    }

    #[test]
    fn prune_is_noop_on_missing_top_level() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let missing = tmp.path().join("does-not-exist");
        let report = prune_stale_version_dirs_in(&missing, "v1.12.15");
        assert!(report.removed.is_empty() && report.skipped.is_empty());
    }
}
