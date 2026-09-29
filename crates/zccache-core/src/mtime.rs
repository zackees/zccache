//! The single owner of every materialized-output mtime decision (#1771).
//!
//! # Contract
//!
//! A materialized output carries the mtime of the cache object it references,
//! **identical for LINK, REFLINK and COPY**. The delivery mode chooses the
//! syscall (hardlink, clone, copy) and never the resulting mtime. Where a
//! policy below *raises* that mtime, the raise is applied identically in every
//! mode; a hit whose mtime must be raised cannot share the cache inode (that
//! would rewrite the blob for every other link), so it is delivered as an
//! independent file carrying the raised value.
//!
//! No production file outside this module may set a file time, except the
//! allowlist in `tests/mtime_owner_workspace.rs` (each entry states why it is
//! not a materialized output). That guard scans the whole workspace.
//!
//! This module holds the pure policy and the one `set_file_mtime` seam. The
//! delivery plumbing (seals, permissions, tiers) stays in the daemon and CLI.
//!
//! # Policies
//!
//! - [`resolve_hit_mtime`]: the per-file default (`ObjectMtime`) plus the
//!   `SiblingFloor` refinement.
//! - [`BatchPolicy::NativeFreshHit`] / [`BatchPolicy::RustcInputFloor`]: the
//!   batch materializer's end-of-hit floor ([`apply_batch_policy_with`]).
//! - [`stamp_recorded_mtime`] / [`recorded_mtime_or_now`]: a recorded mtime
//!   (directory bundles, rust-plan manifests) replayed onto a restored output.
//! - [`touch_cache_object`]: LRU recency for a cache object (`zccache warm`).
//!
//! ## `ObjectMtime` (the default)
//!
//! Preserve the cache object's stored mtime; never stamp `now()` per file.
//! Preservation is the fast path (iter7: 5.9 ms -> 2.8 ms per hit, and no
//! cargo-fingerprint regression). A hardlink already inherits it.
//!
//! ## `SiblingFloor` (#466 / #467)
//!
//! Cargo's `Fingerprint::check_filesystem` reports `StaleDependency` when a
//! dependency's artifact mtime is strictly greater than the dependent's.
//! Out-of-order materialization breaks dep-before-dependent ordering, so the
//! output is floored UP to the newest sibling artifact (`rlib`, `rmeta`, `so`,
//! `dylib`, `dll`, `exe`, `a`, `lib`) in its directory. The floor only ever
//! picks a stable sibling-derived value, never `now()`. `O(deps)` per hit; see
//! #1771 for the measured cost.
//!
//! ## Batch policies
//!
//! The batch materializer (`write_payloads_par_*`) stamps every output of one
//! hit with one floor, seeded with `now()`:
//!
//! - `NativeFreshHit` (C/C++/Emscripten, link, exec): the `now()` seed alone.
//!   It already puts an object at least as new as every source and header, as
//!   a bare compiler would, so recorded inputs are not statted (#1770).
//! - `RustcInputFloor` (rustc): the `now()` seed plus the newest recorded
//!   input (#599). **The `now()` seed is contested for rustc: see #1158 before
//!   touching it.** This policy exists so that decision can be made per
//!   consumer without changing native builds.
//!
//! Both raise a hardlinked output in place, which rewrites the shared blob's
//! mtime (#1819). That is today's behaviour, kept unchanged here.

use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub use kernal_api::platform::fs::FileTime;

/// Whether the caller applies a batch policy after delivering, which decides
/// whether the per-file sibling floor has anything left to do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SiblingFloorPass {
    /// Nothing follows: the sibling floor decides the final mtime, and a raised
    /// hit is delivered independently so the shared blob is never rewritten.
    PerFile,
    /// [`apply_batch_policy_with`] runs after delivery and stamps every output
    /// to at least its `now()` seed, which is never below a sibling's mtime, so
    /// the per-file floor could not change the final value. It is skipped: the
    /// hit keeps its hardlink and saves the `read_dir` of the output directory.
    BatchFollows,
}

/// The mtime a hit's output must carry, and whether a floor raised it.
#[derive(Clone, Copy, Debug)]
pub struct HitMtime {
    /// The value the output carries: the cache object's mtime, or the sibling
    /// floor when that is newer.
    pub mtime: FileTime,
    /// A floor raised `mtime` above the cache object's own. The output must
    /// then be an independent file: sharing the cache inode would rewrite the
    /// blob's mtime for every other link.
    pub raised: bool,
}

/// `ObjectMtime` then `SiblingFloor`: the mtime a hit of `cache_file` at
/// `out_path` must carry, identical whatever tier delivers it.
///
/// One stat of the cache object, plus one `read_dir` of the output's directory
/// when the floor is enabled (`ZCCACHE_DISABLE_MTIME_FLOOR` skips the scan and
/// leaves pure preservation).
///
/// # Errors
/// Returns the I/O error from statting `cache_file`.
pub fn resolve_hit_mtime(
    out_path: &Path,
    cache_file: &Path,
    pass: SiblingFloorPass,
) -> std::io::Result<HitMtime> {
    let object = FileTime::from_last_modification_time(&std::fs::metadata(cache_file)?);
    if pass == SiblingFloorPass::BatchFollows {
        return Ok(HitMtime {
            mtime: object,
            raised: false,
        });
    }
    // A directory that cannot be listed degrades to plain preservation.
    Ok(
        match sibling_floor_above(out_path, object).unwrap_or(None) {
            Some(floor) => HitMtime {
                mtime: floor,
                raised: true,
            },
            None => HitMtime {
                mtime: object,
                raised: false,
            },
        },
    )
}

/// Stamp `mtime` on a writable materialized output. The caller has made the
/// output writable (a read-only file cannot take a timestamp on Windows).
///
/// # Errors
/// Returns the I/O error from setting the file time.
pub fn stamp_mtime(path: &Path, mtime: FileTime) -> std::io::Result<()> {
    kernal_api::platform::fs::set_file_mtime(path, mtime)
}

/// Replay a recorded timestamp onto a restored output.
///
/// # Errors
/// `InvalidData` when the timestamp is unrepresentable, else the I/O error
/// from setting the file time.
pub fn stamp_recorded_mtime(path: &Path, seconds: u64, nanos: u32) -> std::io::Result<()> {
    let modified = UNIX_EPOCH
        .checked_add(Duration::new(seconds, nanos))
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidData, "invalid mtime"))?;
    stamp_mtime(path, FileTime::from_system_time(modified))
}

/// The mtime a manifest-recorded object timestamp (unix nanoseconds) yields:
/// the recording itself, or `now()` when the manifest predates the field and
/// recorded `0`.
#[must_use]
pub fn recorded_mtime_or_now(unix_nanos: u64) -> FileTime {
    if unix_nanos == 0 {
        return FileTime::from_system_time(SystemTime::now());
    }
    FileTime::from_system_time(UNIX_EPOCH + Duration::from_nanos(unix_nanos))
}

/// Mark a cache object as recently used (LRU recency). This is bookkeeping on
/// the cache object, not on an output; every output later delivered from the
/// object then carries this value by the contract. Works on a read-only object.
///
/// # Errors
/// Returns the I/O error from setting the file time.
pub fn touch_cache_object(object: &Path, at: FileTime) -> std::io::Result<()> {
    stamp_mtime(object, at)
}

/// Which floor the batch materializer applies to one hit's outputs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BatchPolicy {
    /// The `now()` seed only (#1770).
    NativeFreshHit,
    /// The `now()` seed plus the newest recorded input (#599). The seed is the
    /// contested part: see #1158.
    RustcInputFloor,
}

impl BatchPolicy {
    /// Rustc requests are the only ones that carry recorded inputs.
    #[must_use]
    pub fn for_inputs(has_inputs: bool) -> Self {
        if has_inputs {
            Self::RustcInputFloor
        } else {
            Self::NativeFreshHit
        }
    }
}

/// Apply `policy` to every output of one hit, raising each through `raise`
/// (which owns seals and permissions). `seed` is `now()` in production; it is a
/// parameter so mode-invariance is testable with a fixed value.
pub fn apply_batch_policy_with<'a>(
    policy: BatchPolicy,
    output_paths: impl IntoIterator<Item = &'a Path>,
    input_paths: impl IntoIterator<Item = &'a Path>,
    seed: SystemTime,
    raise: impl Fn(&Path, FileTime) -> std::io::Result<()>,
) {
    if mtime_floor_disabled() {
        return;
    }

    let outputs: Vec<&Path> = output_paths.into_iter().collect();
    if outputs.is_empty() {
        return;
    }
    let inputs: Vec<&Path> = match policy {
        BatchPolicy::NativeFreshHit => Vec::new(),
        BatchPolicy::RustcInputFloor => input_paths.into_iter().collect(),
    };

    let mut max_mtime = seed;
    for path in outputs.iter().copied().chain(inputs.iter().copied()) {
        let Ok(mtime) = std::fs::metadata(path).and_then(|metadata| metadata.modified()) else {
            continue;
        };
        if mtime > max_mtime {
            max_mtime = mtime;
        }
    }

    let ft = FileTime::from_system_time(max_mtime);
    for path in outputs {
        let Ok(current) = std::fs::metadata(path).and_then(|metadata| metadata.modified()) else {
            continue;
        };
        if current < max_mtime {
            let _ = raise(path, ft);
        }
    }
}

/// Whether `ZCCACHE_DISABLE_MTIME_FLOOR` turns every floor off (read once).
#[must_use]
pub fn mtime_floor_disabled() -> bool {
    use std::sync::OnceLock;
    static CACHED: OnceLock<bool> = OnceLock::new();
    *CACHED.get_or_init(|| {
        std::env::var("ZCCACHE_DISABLE_MTIME_FLOOR")
            .ok()
            .is_some_and(|v| !v.is_empty() && v != "0")
    })
}

/// The newest sibling-artifact mtime in `path`'s directory when it exceeds
/// `base`, else `None` (also `None` when the floor is disabled). `path` itself
/// is skipped, so it need not exist yet.
///
/// # Errors
/// Returns the I/O error from listing the directory.
pub fn sibling_floor_above(path: &Path, base: FileTime) -> std::io::Result<Option<FileTime>> {
    if mtime_floor_disabled() {
        return Ok(None);
    }
    let parent = match path.parent() {
        Some(p) => p,
        None => return Ok(None),
    };
    let mut max_mtime = base;
    for entry in std::fs::read_dir(parent)?.flatten() {
        let p = entry.path();
        // Skip self: comparing against our own mtime is a no-op but
        // would waste a stat.
        if p == path {
            continue;
        }
        // Filter to artifact extensions cargo's `Fingerprint::outputs`
        // tracks. Other entries (.d depfiles, .json metadata,
        // .fingerprint state) don't participate in the StaleDependency
        // comparison.
        let ext = match p.extension().and_then(|s| s.to_str()) {
            Some(e) => e,
            None => continue,
        };
        if !matches!(
            ext,
            "rlib" | "rmeta" | "so" | "dylib" | "dll" | "exe" | "a" | "lib"
        ) {
            continue;
        }
        if let Ok(m) = entry.metadata().and_then(|md| md.modified()) {
            let m = FileTime::from_system_time(m);
            if m > max_mtime {
                max_mtime = m;
            }
        }
    }
    if max_mtime > base {
        Ok(Some(max_mtime))
    } else {
        Ok(None)
    }
}

#[cfg(test)]
#[path = "mtime_tests.rs"]
mod tests;
