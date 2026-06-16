//! Path canonicalization helpers for depfile parsing.
//!
//! These helpers are shared across the depfile parser and the
//! `show_includes.rs` MSVC `/showIncludes` parser, which also needs to
//! canonicalize header paths into the same `NormalizedPath` form used by
//! the file watcher and journal.

use std::path::Path;
use std::sync::OnceLock;

use dashmap::DashMap;

use crate::core::NormalizedPath;

/// Issue #573: process-wide cache for canonicalize results. The
/// depfile parser hits `std::fs::canonicalize` (realpath) per header
/// token; for a cpp compile pulling `<iostream>` that's ~300 syscalls.
/// System headers (`/usr/include/...`) appear in every cpp compile's
/// depfile — caching by input path string absorbs the syscall cost
/// across the daemon's lifetime.
///
/// Capped at 64k entries to bound memory (~100 bytes/entry => ~6 MB).
/// Beyond the cap, new entries fall back to the uncached path.
fn canonicalize_cache() -> &'static DashMap<String, NormalizedPath> {
    static CACHE: OnceLock<DashMap<String, NormalizedPath>> = OnceLock::new();
    CACHE.get_or_init(DashMap::new)
}

const CANONICALIZE_CACHE_MAX_ENTRIES: usize = 64 * 1024;

#[cfg(test)]
pub(crate) fn canonicalize_cache_len_for_test() -> usize {
    canonicalize_cache().len()
}

/// Canonicalize a path, falling back to the joined path if canonicalization
/// fails (e.g., the file does not exist on disk).
///
/// On Windows, `std::fs::canonicalize` produces `\\?\` extended-length paths.
/// These must be stripped so paths match the format used by the file watcher
/// (which also strips `\\?\`), ensuring journal/metadata lookups work correctly.
pub fn canonicalize_path(path: &Path, cwd: &Path) -> NormalizedPath {
    // Issue #573: cache by input path string. The same set of system
    // headers (`/usr/include/...`) appears in every cpp compile's
    // depfile; without caching, realpath fires per token per compile
    // (~300 syscalls per cpp compile, ~2.7 ms / mean for include_scan_ns
    // in the published benchmark-log).
    let key = path.to_string_lossy().into_owned();
    let cache = canonicalize_cache();
    if let Some(cached) = cache.get(&key) {
        return cached.clone();
    }
    let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| {
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            cwd.join(path)
        }
    });
    let result = strip_win_prefix(canonical.into());
    // Bounded insert — once the cache is saturated, new entries are
    // dropped (still served via uncached recomputation).
    if cache.len() < CANONICALIZE_CACHE_MAX_ENTRIES {
        cache.insert(key, result.clone());
    }
    result
}

/// Strip the `\\?\` extended-length prefix on Windows.
/// No-op on other platforms.
pub fn strip_win_prefix(path: NormalizedPath) -> NormalizedPath {
    #[cfg(windows)]
    {
        let s = path.to_string_lossy();
        if let Some(stripped) = s.strip_prefix(r"\\?\") {
            return NormalizedPath::from(stripped);
        }
    }
    path
}
