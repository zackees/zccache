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

use super::resolve::{daemon_state_dir_from_cache_dir, default_cache_dir};
use crate::NormalizedPath;

/// Create `path` and any missing parents, owner-only where the OS expresses
/// that in the filesystem (#1171).
///
/// The cache root and the daemon's socket directory were created with a plain
/// `create_dir_all`, i.e. `0777 & ~umask` — normally `0755`, and `0777` under
/// the `0`/`002` umasks CI images and containers often run with. Those
/// directories hold the IPC endpoint, and anyone who can connect to it can
/// have the daemon spawn a process of their choosing, so the directory mode is
/// an access-control boundary rather than tidiness.
///
/// It is the *load-bearing* one on macOS and the BSDs, where the kernel does
/// not enforce a socket's own mode bits on `connect()` — only search
/// permission on the containing directory, which `0755` grants to everyone.
///
/// Only newly created directories get the mode; an existing directory keeps
/// whatever it has, because silently re-permissioning a user's existing tree
/// is not this function's call to make. Detecting and reporting an
/// already-loose directory is #1171 item 4.
///
/// On Windows this is exactly `create_dir_all`: the equivalent control there
/// is the pipe's DACL, which is #1171 item 2.
/// Ensure an existing directory is not group- or other-writable, tightening
/// it in place if it is (#1171 item 4).
///
/// [`create_dir_all_private`] only sets the mode on directories it creates, so
/// an install predating that change still has a `0755` (or worse) cache root
/// and socket directory. This is the repair path for those, and the reason it
/// returns a `Result`: a directory that is group/other-writable and cannot be
/// tightened is a real exposure, and the caller is expected to refuse to serve
/// rather than continue quietly.
///
/// "Writable" is the test, not "readable". Another user reading the directory
/// listing is uninteresting; another user *creating or replacing entries* in
/// it is how the socket gets substituted.
///
/// Returns `Ok(false)` when nothing needed doing, `Ok(true)` when the mode was
/// tightened, and `Err` when it is still loose afterwards. On Windows this is
/// always `Ok(false)` — the equivalent control is the pipe DACL (#1171 item 2).
pub fn ensure_dir_private(path: &std::path::Path) -> std::io::Result<bool> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let mode = std::fs::metadata(path)?.permissions().mode() & 0o777;
        if mode & 0o022 == 0 {
            return Ok(false);
        }
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;

        // Re-read rather than trusting the write: on some filesystems (and
        // under some mount options) `chmod` reports success without taking
        // effect, and this is exactly the case where a false negative is
        // expensive.
        let now = std::fs::metadata(path)?.permissions().mode() & 0o777;
        if now & 0o022 != 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                format!(
                    "{} is group/other-writable (mode {now:04o}) and could not be tightened",
                    path.display()
                ),
            ));
        }
        Ok(true)
    }
    #[cfg(not(unix))]
    {
        let _ = std::fs::metadata(path)?;
        Ok(false)
    }
}

pub fn create_dir_all_private(path: &std::path::Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(path)
    }
    #[cfg(not(unix))]
    {
        std::fs::create_dir_all(path)
    }
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
    daemon_state_dir_from_cache_dir(cache_dir).join("symbols")
}

/// Returns the cargo registry archive cache under an explicit cache root.
#[must_use]
pub fn cargo_registry_cache_dir_from_cache_dir(cache_dir: &NormalizedPath) -> NormalizedPath {
    daemon_state_dir_from_cache_dir(cache_dir).join("cargo-registry")
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
    daemon_state_dir_from_cache_dir(cache_dir).join("artifacts")
}

/// Returns the tmp directory under an explicit cache root.
#[must_use]
pub fn tmp_dir_from_cache_dir(cache_dir: &NormalizedPath) -> NormalizedPath {
    daemon_state_dir_from_cache_dir(cache_dir).join("tmp")
}

/// Returns the depfile directory under an explicit cache root.
#[must_use]
pub fn depfile_dir_from_cache_dir(cache_dir: &NormalizedPath) -> NormalizedPath {
    tmp_dir_from_cache_dir(cache_dir).join("depfiles")
}

pub fn depgraph_dir_from_cache_dir(cache_dir: &NormalizedPath) -> NormalizedPath {
    daemon_state_dir_from_cache_dir(cache_dir).join("depgraph")
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
    daemon_state_dir_from_cache_dir(cache_dir).join("index.bin")
}

/// Returns the on-disk path for the persisted `MetadataCache` snapshot.
///
/// Bincode blob written by `MetadataCache::save_to_disk` on flush + shutdown,
/// read by `MetadataCache::load_from_disk` on daemon startup. Sibling of
/// [`index_path_from_cache_dir`] so that whatever bundles the cache dir (e.g.
/// `soldr save`/`soldr load`) picks both files up automatically.
#[must_use]
pub fn metadata_path_from_cache_dir(cache_dir: &NormalizedPath) -> NormalizedPath {
    daemon_state_dir_from_cache_dir(cache_dir).join("metadata.bin")
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
    daemon_state_dir_from_cache_dir(cache_dir).join("compiler_hash.bin")
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
    daemon_state_dir_from_cache_dir(cache_dir).join("system_includes.bin")
}

pub(super) fn crash_dump_dir_from_cache_dir(cache_dir: &NormalizedPath) -> NormalizedPath {
    daemon_state_dir_from_cache_dir(cache_dir).join("crashes")
}

/// Returns the log directory under an explicit cache root.
#[must_use]
pub fn log_dir_from_cache_dir(cache_dir: &NormalizedPath) -> NormalizedPath {
    daemon_state_dir_from_cache_dir(cache_dir).join("logs")
}
