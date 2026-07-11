//! Mtime preservation + sibling-floor refinement.
//!
//! See the iter7 invariant in `CLAUDE.md`: preservation is the fast path
//! (zero extra syscalls, no cargo-fingerprint regression). The only
//! exception is the sibling-floor refinement in [`touch_mtime`] / the
//! batch-floor entry point — required so cargo's "dep_mtime ≤ my_mtime"
//! check doesn't misfire on out-of-order materialization (issues #466 / #467).

use super::*;

/// **Preserve** the cache file's stored mtime on the materialized artifact
/// by default, only bumping UP to the max of existing sibling compilation
/// artifacts (`*.rlib` / `*.rmeta` / `*.so` / `*.dylib` / `*.dll` /
/// `*.exe` / `*.a` / `*.lib`) in the same directory when that is needed
/// to satisfy cargo's "dep_mtime ≤ my_mtime" fingerprint invariant.
/// Preservation is the fast path. The floor is a corrective.
///
/// **Why preservation is the default** (iter7): stamping `now()` made
/// cargo treat hardlinked cache hits as "externally modified", invalidating
/// the downstream graph and paying re-link / re-fingerprint cost that
/// fully cancelled the cache savings. Measured 5.9 ms → 2.8 ms per-hit,
/// and recovery of the `bin`-cell recompile cascade (cold-tar-untar-warm,
/// warm 11.6 s → 9.8 s). Preservation is also the cheapest policy.
///
/// **Why the floor exception exists** (issues #466 / #467): cargo's
/// `Fingerprint::check_filesystem` emits `FsStatusOutdated::StaleDependency`
/// when any dep's artifact mtime is strictly greater than the dependent's
/// (`dep_mtime > my_mtime → stale`). Cache files for transitively-
/// dependent crates are not guaranteed to have correctly-ordered mtimes:
/// archive truncation, parallel cache stores, and out-of-order
/// re-materialization all break the dep-before-dependent invariant. The
/// GH bench measured this as 31 crates recompiling on every "warm with
/// target intact" build, taking ~3 s vs sccache's 215 ms baseline. The
/// floor closes that gap without re-introducing the iter7 regression
/// because it only ever increases mtime to a *stable sibling-derived
/// value* (not `now()`), and the value is idempotent across rebuilds:
/// 1. The next hit on the same artifact takes the hardlink / same-file
///    fast path and returns without re-flooring.
/// 2. If we do re-floor, sibling mtimes only ever grow (cache hits
///    materialise with their cache mtime; the floor floors UP from
///    there), so the value converges.
///
/// **Cost**: in isolation (no siblings, or all siblings older than the
/// cache mtime), the floor is a pure no-op after one `read_dir` + N
/// stats — ~50 µs on a `target/debug/deps/` with 300 entries. When the
/// floor actually bumps, the amortised cost is hidden behind the cargo
/// recompilation it prevents (~3 s saved for 50 µs of stat work).
///
/// Disable via `ZCCACHE_DISABLE_MTIME_FLOOR=1` if the floor causes
/// problems with a specific build system (this also disables the
/// preservation guarantee's enforcement, so the cache file's mtime is
/// what survives — still not `now()`).
pub(in crate::daemon::server) fn touch_mtime(path: &Path) {
    if mtime_floor_disabled() {
        return;
    }
    let _ = floor_artifact_mtime_to_sibling_max(path);
}

pub(in crate::daemon::server) fn floor_materialized_outputs_to_input_max<'a>(
    output_paths: impl IntoIterator<Item = &'a Path>,
    input_paths: impl IntoIterator<Item = &'a Path>,
    minimum_mtime: std::time::SystemTime,
) {
    if mtime_floor_disabled() {
        return;
    }

    let outputs: Vec<&Path> = output_paths.into_iter().collect();
    if outputs.is_empty() {
        return;
    }

    let mut max_mtime = minimum_mtime;
    for path in outputs.iter().copied().chain(input_paths) {
        let Ok(mtime) = std::fs::metadata(path).and_then(|metadata| metadata.modified()) else {
            continue;
        };
        if mtime > max_mtime {
            max_mtime = mtime;
        }
    }

    let ft = filetime::FileTime::from_system_time(max_mtime);
    for path in outputs {
        let Ok(current) = std::fs::metadata(path).and_then(|metadata| metadata.modified()) else {
            continue;
        };
        if current < max_mtime {
            let _ = set_materialized_mtime(path, ft);
        }
    }
}

fn mtime_floor_disabled() -> bool {
    use std::sync::OnceLock;
    static CACHED: OnceLock<bool> = OnceLock::new();
    *CACHED.get_or_init(|| {
        std::env::var("ZCCACHE_DISABLE_MTIME_FLOOR")
            .ok()
            .is_some_and(|v| !v.is_empty() && v != "0")
    })
}

pub(in crate::daemon::server) fn floor_artifact_mtime_to_sibling_max(
    path: &Path,
) -> std::io::Result<()> {
    if let Some(ft) = compute_sibling_floor(path)? {
        let _ = set_materialized_mtime(path, ft);
    }
    Ok(())
}

/// Compute the sibling-floor mtime for `path` without applying it. Returns
/// `None` when the floor is disabled or would be a no-op (no sibling is
/// newer than `path`'s current mtime). Split out from
/// [`floor_artifact_mtime_to_sibling_max`] so same-inode call sites can
/// check whether flooring is actually needed before deciding how to apply
/// it (e.g. detaching a hardlinked output instead of mutating the shared
/// blob in place).
pub(in crate::daemon::server) fn compute_sibling_floor(
    path: &Path,
) -> std::io::Result<Option<filetime::FileTime>> {
    if mtime_floor_disabled() {
        return Ok(None);
    }
    let parent = match path.parent() {
        Some(p) => p,
        None => return Ok(None),
    };
    let my_mtime = std::fs::metadata(path)?.modified()?;
    let mut max_mtime = my_mtime;
    for entry in std::fs::read_dir(parent)?.flatten() {
        let p = entry.path();
        // Skip self — comparing against our own mtime is a no-op but
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
            if m > max_mtime {
                max_mtime = m;
            }
        }
    }
    if max_mtime > my_mtime {
        Ok(Some(filetime::FileTime::from_system_time(max_mtime)))
    } else {
        Ok(None)
    }
}

pub(in crate::daemon::server) fn set_materialized_mtime(
    path: &Path,
    mtime: filetime::FileTime,
) -> std::io::Result<()> {
    let readonly = std::fs::metadata(path)?.permissions().readonly();
    if readonly {
        make_writable(path)?;
    }
    let result = filetime::set_file_mtime(path, mtime);
    if readonly {
        let restore = set_readonly(path, true);
        if result.is_ok() {
            restore?;
        }
    }
    result
}
