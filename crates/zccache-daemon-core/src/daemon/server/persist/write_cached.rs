//! Capability-driven cache-hit materialization (#1039).

use super::*;

pub(in crate::daemon::server) fn write_cached_output(
    out_path: &Path,
    cache_file: &Path,
    data: &[u8],
) -> std::io::Result<()> {
    if !cache_file.exists() {
        remove_materialized_output(out_path)?;
        return std::fs::write(out_path, data);
    }
    materialize_cached_file(out_path, cache_file)
}

pub(in crate::daemon::server) fn write_cached_file(
    out_path: &Path,
    cache_file: &Path,
) -> std::io::Result<()> {
    materialize_cached_file(out_path, cache_file)
}

fn materialize_cached_file(out_path: &Path, cache_file: &Path) -> std::io::Result<()> {
    let staged = is_staged_artifact_path(cache_file);
    if !staged {
        verify_registered_blob(cache_file)?;
    }
    if same_file(out_path, cache_file) {
        if staged {
            let floor =
                filetime::FileTime::from_last_modification_time(&std::fs::metadata(cache_file)?);
            detach_with_floored_mtime(out_path, cache_file, floor)?;
            return Ok(());
        }
        set_readonly(cache_file, readonly_enabled())?;
        match compute_sibling_floor(out_path)? {
            Some(floor) => detach_with_floored_mtime(out_path, cache_file, floor)?,
            None => register_hardlink(cache_file, out_path)?,
        }
        return Ok(());
    }
    remove_materialized_output(out_path)?;
    let caps = fs_caps(cache_file, out_path);
    if caps.reflink && reflink_copy::reflink(cache_file, out_path).is_ok() {
        make_writable(out_path)?;
        restore_cache_mtime(cache_file, out_path)?;
        touch_mtime(out_path);
        return Ok(());
    }
    // A failed link-count query must not be read as "at capacity" — that
    // silently defeats the hardlink tier (falls through to a full copy)
    // on every transient stat/handle failure. Fall back to 0 (unknown ==
    // assume no existing links yet) like the other `hard_link_count`
    // call sites in this module; a genuinely-too-many-links file still
    // fails the real `std::fs::hard_link` call below, which already has
    // a graceful copy fallback.
    if !staged && hardlink_below_limit(caps, hard_link_count(cache_file).unwrap_or_default()) {
        // Only flip the blob read-only *after* the link actually lands.
        // Read-only exists to protect the blob once it's shared; setting
        // it beforehand serves no purpose on the attempt path and, if
        // `hard_link` fails, left the blob stuck read-only forever (no
        // revert existed on the failure path below).
        let registration = prepare_hardlink_registration(cache_file, out_path)?;
        match std::fs::hard_link(cache_file, out_path) {
            Ok(()) => {
                if let Err(error) = set_readonly(cache_file, readonly_enabled()) {
                    tracing::warn!(
                        event = "cow_hardlink_readonly_failed",
                        cache_file = %cache_file.display(),
                        out_path = %out_path.display(),
                        error = %error,
                        "hardlink protection failed after creation; falling back to copy"
                    );
                    let _ = cleanup_failed_hardlink(registration, cache_file, out_path);
                } else {
                    match commit_hardlink_registration(registration, out_path) {
                        Ok(()) => {
                            touch_mtime(out_path);
                            return Ok(());
                        }
                        Err(error) => {
                            // A failure here (including a transient stat/handle
                            // error resolving the just-created link's identity)
                            // must not become a hard failure of the whole
                            // materialization — fall back to a copy the same
                            // way a failed std::fs::hard_link already does
                            // (issue #1042).
                            tracing::warn!(
                                event = "cow_hardlink_registration_commit_failed",
                                cache_file = %cache_file.display(),
                                out_path = %out_path.display(),
                                error = %error,
                                "hardlink registration commit failed after a successful hardlink; falling back to copy"
                            );
                            let _ = cleanup_failed_hardlink(registration, cache_file, out_path);
                        }
                    }
                }
            }
            Err(error) => {
                tracing::warn!(
                    event = "cow_hardlink_fallback_to_copy",
                    cache_file = %cache_file.display(),
                    out_path = %out_path.display(),
                    error = %error,
                    "hardlink materialization failed despite capability probe; falling back to copy"
                );
                cancel_hardlink_registration(registration, out_path);
                commit_registered_detach(registration, out_path);
            }
        }
    }
    std::fs::copy(cache_file, out_path)?;
    make_writable(out_path)?;
    restore_cache_mtime(cache_file, out_path)?;
    touch_mtime(out_path);
    Ok(())
}

fn cleanup_failed_hardlink(
    registration: FileId,
    cache_file: &Path,
    out_path: &Path,
) -> std::io::Result<()> {
    cancel_hardlink_registration(registration, out_path);

    let removed = match make_writable(out_path) {
        Ok(()) => remove_output_file(out_path),
        Err(error) => Err(error),
    }
    .or_else(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            Ok(())
        } else {
            Err(error)
        }
    });
    if removed.is_ok() {
        commit_registered_detach(registration, out_path);
    }
    let restored = if removed.is_ok() {
        set_readonly(cache_file, readonly_enabled())
    } else {
        Ok(())
    };

    removed.and(restored)
}

/// `out_path` IS `cache_file` here (same inode, already hardlinked from a
/// prior materialization) and the sibling-floor mtime requirement
/// (#466/#467) needs to raise this output's mtime. Mutating it in place
/// would bump the *shared blob's* mtime too, corrupting it for every other
/// hardlink pointing at the same cache entry. Detach this specific output
/// into a private copy instead, so only it gets the floored mtime — the
/// cache blob itself, and every other output still hardlinked to it, is
/// left untouched.
fn detach_with_floored_mtime(
    out_path: &Path,
    cache_file: &Path,
    floor: filetime::FileTime,
) -> std::io::Result<()> {
    let registration = prepare_registered_detach(out_path);
    make_writable(out_path)?;
    remove_output_file(out_path)?;
    std::fs::copy(cache_file, out_path)?;
    make_writable(out_path)?;
    let result = set_materialized_mtime(out_path, floor);
    set_readonly(cache_file, readonly_enabled())?;
    if let Some((id, _)) = registration {
        commit_registered_detach(id, out_path);
    }
    result
}

fn restore_cache_mtime(cache_file: &Path, out_path: &Path) -> std::io::Result<()> {
    let mtime = filetime::FileTime::from_last_modification_time(&std::fs::metadata(cache_file)?);
    filetime::set_file_mtime(out_path, mtime)
}

fn remove_materialized_output(path: &Path) -> std::io::Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    }
    let registration = prepare_registered_detach(path);
    if let Err(error) = make_writable(path) {
        return Err(error);
    }
    if let Err(error) = remove_output_file(path) {
        if let Some((_, blob_path)) = &registration {
            let _ = set_readonly(blob_path, readonly_enabled());
        }
        return Err(error);
    }
    if let Some((id, _)) = &registration {
        commit_registered_detach(*id, path);
    }
    if let Some((_, blob_path)) = registration {
        let _ = set_readonly(&blob_path, readonly_enabled());
    }
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
