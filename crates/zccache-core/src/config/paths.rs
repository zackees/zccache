//! Well-known subpaths under the resolved cache root.
//!
//! Two shapes per subpath:
//! * `*_dir()` / `*_path()` — convenience wrappers that call [`default_cache_dir`]
//!   and then `*_from_cache_dir`. Use these when no cache dir is already in hand.
//! * `*_from_cache_dir(&NormalizedPath)` — pure path joiners. Use these in tests
//!   that pass a per-test temp dir, or when the caller has already resolved
//!   the cache root and wants to avoid the global env-var lookup.
//!
//! Every persistent file the daemon and CLI read or write MUST live under
//! the resolved cache root — this is the soldr/Defender exclusion contract
//! from issue #275. The `cache_root_invariant_all_subpaths_rooted` test in
//! `tests.rs` guards that invariant.

use super::resolve::default_cache_dir;
use crate::NormalizedPath;

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

pub(super) fn depgraph_dir_from_cache_dir(cache_dir: &NormalizedPath) -> NormalizedPath {
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

pub(super) fn crash_dump_dir_from_cache_dir(cache_dir: &NormalizedPath) -> NormalizedPath {
    cache_dir.join("crashes")
}

/// Returns the log directory under an explicit cache root.
#[must_use]
pub fn log_dir_from_cache_dir(cache_dir: &NormalizedPath) -> NormalizedPath {
    cache_dir.join("logs")
}
