//! Materialize cached output to its target path: single-file write helpers
//! and the parallel batch entry points used by the cached-hit path.

use super::*;

/// Write cached output to disk. Optimized syscall sequence:
/// 1. Try hardlink directly (1 syscall — common case when output doesn't exist)
/// 2. If output already exists: check if it's the same file (skip if so)
/// 3. Remove existing output and retry hardlink (2 syscalls)
/// 4. Fall back to fs::write from memory (1 syscall)
///
/// **Mtime policy**: the output inherits the cache file's stored mtime by
/// default. We deliberately do NOT stamp `now()` — that was the pre-iter7
/// behaviour and it caused cargo's incremental fingerprint to mark
/// hardlinked artifacts as "externally modified", invalidating the
/// downstream graph (measured 5.9 ms → 2.8 ms per-hit + recovery of the
/// `bin`-cell recompile cascade in the cold-tar-untar-warm scenario).
/// Preservation is also the cheapest possible policy: zero extra syscalls.
///
/// The only exception is the sibling-floor refinement in [`touch_mtime`],
/// which bumps the artifact's mtime UP **only when** an existing sibling
/// artifact in the same directory already has a higher mtime — required to
/// keep cargo's "dep_mtime ≤ my_mtime" check from misfiring on
/// out-of-order materialization (issues #466 / #467). In isolation (no
/// siblings, or all siblings older), the floor is a no-op and the cache
/// mtime is preserved verbatim — the fast path.
///
/// The hardlink-first order optimizes for the rebuild scenario where outputs
/// don't exist yet (1 syscall). For incremental builds where outputs exist
/// as hardlinks, the failed hardlink + same_file check is still fast.
pub(in crate::daemon::server) fn write_cached_output(
    out_path: &Path,
    cache_file: &Path,
    data: &[u8],
) -> std::io::Result<()> {
    // Fast path: hardlink directly (works when out_path doesn't exist yet).
    // This is the cheapest path — one kernel call when no output exists.
    if std::fs::hard_link(cache_file, out_path).is_ok() {
        touch_mtime(out_path);
        return Ok(());
    }
    // Hardlink failed — output probably exists. Check if it's already
    // the same file (hardlinked from a previous hit). Compare file
    // identity (inode/volume+index), NOT file size — two different
    // compilations can produce .o files with identical sizes but
    // different content (alignment, padding).
    if same_file(out_path, cache_file) {
        touch_mtime(out_path);
        return Ok(());
    }
    // Output exists but is different — remove and retry
    let _ = std::fs::remove_file(out_path);
    if std::fs::hard_link(cache_file, out_path).is_ok() {
        touch_mtime(out_path);
        return Ok(());
    }
    // Hardlink failed entirely (cross-device, no cache file) — copy from memory.
    // fs::write creates a new file with current mtime, so no touch needed.
    std::fs::write(out_path, data)
}

pub(in crate::daemon::server) fn write_cached_file(
    out_path: &Path,
    cache_file: &Path,
) -> std::io::Result<()> {
    if std::fs::hard_link(cache_file, out_path).is_ok() {
        touch_mtime(out_path);
        return Ok(());
    }
    if same_file(out_path, cache_file) {
        touch_mtime(out_path);
        return Ok(());
    }
    let _ = std::fs::remove_file(out_path);
    if std::fs::hard_link(cache_file, out_path).is_ok() {
        touch_mtime(out_path);
        return Ok(());
    }
    std::fs::copy(cache_file, out_path)?;
    touch_mtime(out_path);
    Ok(())
}

pub(in crate::daemon::server) fn write_cached_payload(
    out_path: &Path,
    cache_file: &Path,
    payload: &CachedPayload,
) -> std::io::Result<()> {
    match payload {
        CachedPayload::Bytes(data) => write_cached_output(out_path, cache_file, data),
        CachedPayload::File(path) => write_cached_file(out_path, path),
    }
}

/// Write a batch of cached payloads to their target paths in parallel.
///
/// `targets[i]` is the `(out_path, cache_file)` pair for `payloads[i]`. The
/// parent of each `out_path` is created if it does not exist (matches the
/// per-site behavior of the link-hit path). Returns `true` iff every write
/// succeeded; `false` on first failure (callers either fall through to a
/// fallback path or ignore the result, mirroring the prior serial loops).
///
/// Threshold: rayon is only used when `targets.len() >= 4`. For N ≤ 3 the
/// per-iteration thread-pool dispatch cost (~300 µs) is comparable to the
/// hardlink syscalls themselves, so a serial loop is faster. The
/// `benches/write_payloads.rs` micro-benchmark sets this cut-off
/// empirically on the Windows / NTFS host.
pub(in crate::daemon::server) const PAR_WRITE_THRESHOLD: usize = 4;

pub(in crate::daemon::server) fn write_payloads_par<P, Q>(
    targets: &[(P, Q)],
    payloads: &[CachedPayload],
) -> bool
where
    P: AsRef<Path> + Sync,
    Q: AsRef<Path> + Sync,
{
    debug_assert_eq!(targets.len(), payloads.len());
    let write_one = |out: &Path, cache: &Path, payload: &CachedPayload| -> bool {
        if let Some(parent) = out.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        write_cached_payload(out, cache, payload).is_ok()
    };
    if targets.len() < PAR_WRITE_THRESHOLD {
        return targets
            .iter()
            .zip(payloads.iter())
            .all(|((out, cache), payload)| write_one(out.as_ref(), cache.as_ref(), payload));
    }
    use rayon::prelude::*;
    targets
        .par_iter()
        .zip(payloads.par_iter())
        .all(|((out, cache), payload)| write_one(out.as_ref(), cache.as_ref(), payload))
}

pub(in crate::daemon::server) fn write_payloads_par_with_mtime_floor<P, Q, R>(
    targets: &[(P, Q)],
    payloads: &[CachedPayload],
    floor_paths: &[R],
) -> bool
where
    P: AsRef<Path> + Sync,
    Q: AsRef<Path> + Sync,
    R: AsRef<Path>,
{
    if !write_payloads_par(targets, payloads) {
        return false;
    }
    let batch_floor = std::time::SystemTime::now();
    floor_materialized_outputs_to_input_max(
        targets.iter().map(|(out, _)| out.as_ref()),
        floor_paths.iter().map(|path| path.as_ref()),
        batch_floor,
    );
    true
}
