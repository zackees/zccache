//! Capability-driven cache-hit materialization (#1039).

use super::*;

/// Evidence that a cache payload disappeared after its metadata was selected.
/// Only this type authorizes a caller to invalidate a depgraph entry.
#[derive(Debug)]
pub(in crate::daemon::server) struct CacheBlobMissing {
    pub(in crate::daemon::server) path: NormalizedPath,
    pub(in crate::daemon::server) error: std::io::Error,
}

/// The cache payload still exists, but it could not be verified or read.
/// This is a soft miss and is deliberately not evidence for invalidation.
#[derive(Debug)]
pub(in crate::daemon::server) struct CacheReadFailure {
    pub(in crate::daemon::server) path: NormalizedPath,
    pub(in crate::daemon::server) error: std::io::Error,
}

/// A write to the current invocation's requested output failed. The cached
/// artifact remains valid and must not be evicted.
#[derive(Debug)]
pub(in crate::daemon::server) struct DestinationWriteFailure {
    pub(in crate::daemon::server) path: NormalizedPath,
    pub(in crate::daemon::server) error: std::io::Error,
}

#[derive(Debug)]
pub(in crate::daemon::server) enum MaterializationFailure {
    CacheBlobMissing(CacheBlobMissing),
    CacheRead(CacheReadFailure),
    DestinationWrite(DestinationWriteFailure),
}

pub(in crate::daemon::server) type MaterializationResult<T> = Result<T, MaterializationFailure>;

pub(in crate::daemon::server) fn report_materialization_failure(
    cache_root: &Path,
    artifact_key: &str,
    consumer: &'static str,
    failure: &MaterializationFailure,
) {
    match failure {
        MaterializationFailure::CacheBlobMissing(missing) => {
            record_miss_reason(miss_reason::NO_ARTIFACT_FOR_KEY);
            tracing::warn!(
                event = "cache_blob_missing",
                artifact_key,
                consumer,
                cache_path = %missing.path.display(),
                error = %missing.error,
                "cached artifact payload disappeared during materialization"
            );
        }
        MaterializationFailure::CacheRead(cache_read) => {
            record_miss_reason(miss_reason::NO_ARTIFACT_FOR_KEY);
            tracing::warn!(
                event = "cache_payload_read_failed",
                artifact_key,
                consumer,
                cache_path = %cache_read.path.display(),
                error = %cache_read.error,
                "cached artifact payload could not be verified or read"
            );
        }
        MaterializationFailure::DestinationWrite(destination) => {
            record_miss_reason(miss_reason::DESTINATION_WRITE_FAILED);
            tracing::warn!(
                event = "destination_write_failed",
                artifact_key,
                consumer,
                output_path = %destination.path.display(),
                error = %destination.error,
                "cached artifact could not be written to the requested destination"
            );
            crate::core::lifecycle::write_event_in_cache_root(
                cache_root,
                crate::core::lifecycle::EVENT_DESTINATION_WRITE_FAILED,
                serde_json::json!({
                    "artifact_key": artifact_key,
                    "consumer": consumer,
                    "path": destination.path.display().to_string(),
                    "errno": destination.error.raw_os_error(),
                    "evicted": false,
                }),
            );
        }
    }
}

#[cfg(test)]
pub(in crate::daemon::server) fn write_cached_output(
    out_path: &Path,
    cache_file: &Path,
    data: &[u8],
) -> std::io::Result<()> {
    if !cache_file.exists() {
        remove_materialized_output(out_path)?;
        return std::fs::write(out_path, data);
    }
    materialize_cached_file(
        out_path,
        cache_file,
        crate::compiler::DeliveryPolicy::IndependentOnly,
    )
    .map(|_| ())
}

#[cfg(test)]
pub(in crate::daemon::server) fn write_cached_file(
    out_path: &Path,
    cache_file: &Path,
) -> std::io::Result<()> {
    materialize_cached_file(
        out_path,
        cache_file,
        crate::compiler::DeliveryPolicy::IndependentOnly,
    )
    .map(|_| ())
}

#[cfg(test)]
fn materialize_cached_file(
    out_path: &Path,
    cache_file: &Path,
    delivery: crate::compiler::DeliveryPolicy,
) -> std::io::Result<StagedMaterializationStats> {
    materialize_cached_file_observed(out_path, cache_file, delivery, false)
}

#[cfg(test)]
fn materialize_cached_file_observed(
    out_path: &Path,
    cache_file: &Path,
    delivery: crate::compiler::DeliveryPolicy,
    force_observation: bool,
) -> std::io::Result<StagedMaterializationStats> {
    verify_registered_blob(cache_file)?;
    materialize_verified_cached_file_observed(out_path, cache_file, delivery, force_observation)
}

fn materialize_verified_cached_file_observed(
    out_path: &Path,
    cache_file: &Path,
    delivery: crate::compiler::DeliveryPolicy,
    force_observation: bool,
) -> std::io::Result<StagedMaterializationStats> {
    let staged = is_staged_artifact_path(cache_file);
    let observe = force_observation || staged;
    let observed = |reflink_count, hardlink_count, copy_count, copy_bytes| {
        if observe {
            StagedMaterializationStats {
                reflink_count,
                hardlink_count,
                copy_count,
                copy_bytes,
            }
        } else {
            StagedMaterializationStats::default()
        }
    };
    let hardlink_allowed =
        !staged || matches!(delivery, crate::compiler::DeliveryPolicy::HardlinkEligible);
    if crate::platform::fs::identity::same_file(out_path, cache_file).unwrap_or(false) {
        if !hardlink_allowed {
            let bytes = std::fs::metadata(cache_file)?.len();
            let floor =
                filetime::FileTime::from_last_modification_time(&std::fs::metadata(cache_file)?);
            detach_with_floored_mtime(out_path, cache_file, floor)?;
            return Ok(observed(0, 0, 1, bytes));
        }
        crate::platform::fs::permissions::set_readonly(cache_file, readonly_enabled())?;
        match compute_sibling_floor(out_path)? {
            Some(floor) => {
                let bytes = std::fs::metadata(cache_file)?.len();
                detach_with_floored_mtime(out_path, cache_file, floor)?;
                return Ok(observed(0, 0, 1, bytes));
            }
            None => register_hardlink(cache_file, out_path)?,
        }
        return Ok(observed(0, 1, 0, 0));
    }
    remove_materialized_output(out_path)?;
    let caps = fs_caps(cache_file, out_path);
    let reflink_allowed = {
        #[cfg(test)]
        {
            inject_staged_fault(out_path, StagedFaultPoint::MaterializeReflink).is_ok()
        }
        #[cfg(not(test))]
        {
            true
        }
    };
    if reflink_allowed && caps.reflink && reflink_copy::reflink(cache_file, out_path).is_ok() {
        crate::platform::fs::permissions::make_writable(out_path)?;
        restore_cache_mtime(cache_file, out_path)?;
        touch_mtime(out_path);
        return Ok(observed(1, 0, 0, 0));
    }
    // A failed link-count query must not be read as "at capacity" — that
    // silently defeats the hardlink tier (falls through to a full copy)
    // on every transient stat/handle failure. Fall back to 0 (unknown ==
    // assume no existing links yet) like the other `hard_link_count`
    // call sites in this module; a genuinely-too-many-links file still
    // fails the real `std::fs::hard_link` call below, which already has
    // a graceful copy fallback.
    let hardlink_candidate = hardlink_allowed
        && hardlink_below_limit(
            caps,
            crate::platform::fs::links::hard_link_count(cache_file).unwrap_or_default(),
        );
    #[cfg(test)]
    let hardlink_candidate = hardlink_candidate
        && inject_staged_fault(out_path, StagedFaultPoint::MaterializeHardlink).is_ok();
    if hardlink_candidate {
        // Only flip the blob read-only *after* the link actually lands.
        // Read-only exists to protect the blob once it's shared; setting
        // it beforehand serves no purpose on the attempt path and, if
        // `hard_link` fails, left the blob stuck read-only forever (no
        // revert existed on the failure path below).
        let registration = prepare_hardlink_registration(cache_file, out_path)?;
        match std::fs::hard_link(cache_file, out_path) {
            Ok(()) => {
                if let Err(error) =
                    crate::platform::fs::permissions::set_readonly(cache_file, readonly_enabled())
                {
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
                            return Ok(observed(0, 1, 0, 0));
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
    #[cfg(test)]
    inject_staged_fault(out_path, StagedFaultPoint::MaterializeCopy)?;
    let copied_bytes = std::fs::copy(cache_file, out_path)?;
    crate::platform::fs::permissions::make_writable(out_path)?;
    restore_cache_mtime(cache_file, out_path)?;
    touch_mtime(out_path);
    Ok(observed(0, 0, 1, copied_bytes))
}

#[cfg(test)]
pub(in crate::daemon::server) fn write_cached_file_observed(
    out_path: &Path,
    cache_file: &Path,
) -> std::io::Result<StagedMaterializationStats> {
    materialize_cached_file_observed(
        out_path,
        cache_file,
        crate::compiler::DeliveryPolicy::IndependentOnly,
        true,
    )
}

fn cleanup_failed_hardlink(
    registration: crate::platform::fs::FileIdentity,
    cache_file: &Path,
    out_path: &Path,
) -> std::io::Result<()> {
    cancel_hardlink_registration(registration, out_path);

    let removed = match crate::platform::fs::permissions::make_writable(out_path) {
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
        crate::platform::fs::permissions::set_readonly(cache_file, readonly_enabled())
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
    crate::platform::fs::permissions::make_writable(out_path)?;
    remove_output_file(out_path)?;
    std::fs::copy(cache_file, out_path)?;
    crate::platform::fs::permissions::make_writable(out_path)?;
    let result = set_materialized_mtime(out_path, floor);
    crate::platform::fs::permissions::set_readonly(cache_file, readonly_enabled())?;
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
    crate::platform::fs::permissions::make_writable(path)?;
    if let Err(error) = remove_output_file(path) {
        if let Some((_, blob_path)) = &registration {
            let _ = crate::platform::fs::permissions::set_readonly(blob_path, readonly_enabled());
        }
        return Err(error);
    }
    if let Some((id, _)) = &registration {
        commit_registered_detach(*id, path);
    }
    if let Some((_, blob_path)) = registration {
        let _ = crate::platform::fs::permissions::set_readonly(&blob_path, readonly_enabled());
    }
    Ok(())
}

pub(in crate::daemon::server) fn write_cached_payload_with_policy_stats(
    out_path: &Path,
    payload: &CachedPayload,
    delivery: crate::compiler::DeliveryPolicy,
) -> MaterializationResult<StagedMaterializationStats> {
    match payload {
        CachedPayload::Bytes(data) => {
            remove_materialized_output(out_path)
                .map_err(|error| destination_write_failure(out_path, error))?;
            std::fs::write(out_path, data.as_slice())
                .map_err(|error| destination_write_failure(out_path, error))?;
            Ok(StagedMaterializationStats::default())
        }
        CachedPayload::File(path) => {
            verify_registered_blob(path).map_err(|error| classify_cache_read_error(path, error))?;
            materialize_verified_cached_file_observed(out_path, path, delivery, false)
                .map_err(|error| classify_file_materialization_error(out_path, path, error))
        }
    }
}

pub(in crate::daemon::server) const PAR_WRITE_THRESHOLD: usize = 4;

pub(in crate::daemon::server) fn write_payloads_par_observed<P>(
    targets: &[P],
    payloads: &[CachedPayload],
) -> MaterializationResult<StagedMaterializationStats>
where
    P: AsRef<Path> + Sync,
{
    if targets.len() != payloads.len() {
        return Err(payload_count_mismatch(targets.len(), payloads.len()));
    }
    let write_one = |out: &Path,
                     payload: &CachedPayload|
     -> MaterializationResult<StagedMaterializationStats> {
        if let Some(parent) = out.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| destination_write_failure(out, error))?;
        }
        write_cached_payload_with_policy_stats(
            out,
            payload,
            crate::compiler::DeliveryPolicy::IndependentOnly,
        )
    };
    if targets.len() < PAR_WRITE_THRESHOLD {
        let mut observed = StagedMaterializationStats::default();
        for (out, payload) in targets.iter().zip(payloads) {
            observed.add(write_one(out.as_ref(), payload)?);
        }
        return Ok(observed);
    }
    use rayon::prelude::*;
    targets
        .par_iter()
        .zip(payloads.par_iter())
        .map(|(out, payload)| write_one(out.as_ref(), payload))
        .try_reduce(StagedMaterializationStats::default, |mut total, one| {
            total.add(one);
            Ok(total)
        })
}

#[cfg(test)]
pub(in crate::daemon::server) fn write_payloads_par_with_mtime_floor<P, R>(
    targets: &[P],
    payloads: &[CachedPayload],
    floor_paths: &[R],
) -> bool
where
    P: AsRef<Path> + Sync,
    R: AsRef<Path>,
{
    let policies = vec![crate::compiler::DeliveryPolicy::IndependentOnly; targets.len()];
    write_payloads_par_with_mtime_floor_and_policies(targets, payloads, floor_paths, &policies)
}

#[cfg(test)]
pub(in crate::daemon::server) fn write_payloads_par_with_mtime_floor_and_policies<P, R>(
    targets: &[P],
    payloads: &[CachedPayload],
    floor_paths: &[R],
    policies: &[crate::compiler::DeliveryPolicy],
) -> bool
where
    P: AsRef<Path> + Sync,
    R: AsRef<Path>,
{
    write_payloads_par_with_mtime_floor_and_policies_observed(
        targets,
        payloads,
        floor_paths,
        policies,
    )
    .is_ok()
}

pub(in crate::daemon::server) fn write_payloads_par_with_mtime_floor_and_policies_observed<P, R>(
    targets: &[P],
    payloads: &[CachedPayload],
    floor_paths: &[R],
    policies: &[crate::compiler::DeliveryPolicy],
) -> MaterializationResult<StagedMaterializationStats>
where
    P: AsRef<Path> + Sync,
    R: AsRef<Path>,
{
    write_payloads_par_with_mtime_floor_and_policies_observed_impl(
        targets,
        payloads,
        floor_paths,
        policies,
        false,
    )
}

/// Deliver compiler-private staged paths before their durable cache blobs have
/// been registered. These sources are protected by the payload guard's plan
/// ownership, so they must use independent reflink/copy delivery and must not
/// pass through durable-blob digest verification or hardlink registration.
pub(in crate::daemon::server) fn write_provisional_payloads_par_with_mtime_floor_observed<P, R>(
    targets: &[P],
    payloads: &[CachedPayload],
    floor_paths: &[R],
    policies: &[crate::compiler::DeliveryPolicy],
) -> MaterializationResult<StagedMaterializationStats>
where
    P: AsRef<Path> + Sync,
    R: AsRef<Path>,
{
    write_payloads_par_with_mtime_floor_and_policies_observed_impl(
        targets,
        payloads,
        floor_paths,
        policies,
        true,
    )
}

fn write_payloads_par_with_mtime_floor_and_policies_observed_impl<P, R>(
    targets: &[P],
    payloads: &[CachedPayload],
    floor_paths: &[R],
    policies: &[crate::compiler::DeliveryPolicy],
    provisional_staged: bool,
) -> MaterializationResult<StagedMaterializationStats>
where
    P: AsRef<Path> + Sync,
    R: AsRef<Path>,
{
    if targets.len() != payloads.len() {
        return Err(payload_count_mismatch(targets.len(), payloads.len()));
    }
    if targets.len() != policies.len() {
        return Err(cache_read_failure(
            Path::new("<cached-delivery-policy-set>"),
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "cached target count {} does not match delivery-policy count {}",
                    targets.len(),
                    policies.len()
                ),
            ),
        ));
    }
    let write_one = |out: &Path,
                     payload: &CachedPayload,
                     policy: crate::compiler::DeliveryPolicy|
     -> MaterializationResult<StagedMaterializationStats> {
        if let Some(parent) = out.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| destination_write_failure(out, error))?;
        }
        if provisional_staged {
            match payload {
                CachedPayload::File(path) => {
                    crate::daemon::server::persist::materialize_independent_with_stats(
                        path.as_path(),
                        out,
                    )
                    .map_err(|error| classify_file_materialization_error(out, path, error))
                }
                CachedPayload::Bytes(_) => {
                    write_cached_payload_with_policy_stats(out, payload, policy)
                }
            }
        } else {
            write_cached_payload_with_policy_stats(out, payload, policy)
        }
    };
    let observed = if targets.len() < PAR_WRITE_THRESHOLD {
        let mut observed = StagedMaterializationStats::default();
        for ((out, payload), policy) in targets.iter().zip(payloads).zip(policies) {
            observed.add(write_one(out.as_ref(), payload, *policy)?);
        }
        observed
    } else {
        use rayon::prelude::*;
        targets
            .par_iter()
            .zip(payloads.par_iter())
            .zip(policies.par_iter())
            .map(|((out, payload), policy)| write_one(out.as_ref(), payload, *policy))
            .try_reduce(StagedMaterializationStats::default, |mut total, one| {
                total.add(one);
                Ok(total)
            })?
    };
    // This `now()` seed is a deliberate, contested exception to CLAUDE.md's
    // "never stamp now() on cache hits" rule. Read this before "fixing" it —
    // it has been flagged and investigated twice (#1158 most recently).
    //
    // Because `now()` dominates every real file mtime, seeding the floor with
    // it means every materialized output is stamped to ~now(), not floored up
    // to a stable sibling. Two *measured* findings point in opposite
    // directions about whether that is right for rustc/cargo:
    //
    //  * #599 (fixed, and pinned by `batch_floor_freshens_*` in `tests.rs`):
    //    a hit is still a rustc invocation from cargo's perspective. Restore an
    //    old mtime and cargo records a stale output, so the *next* no-op build
    //    recompiles the graph — measured at 14x slower "warm (target intact)".
    //  * iter7 (CLAUDE.md): stamping now() on hits invalidates cargo's
    //    fingerprint the other way, measured at 5.9ms -> 2.8ms per hit and
    //    warm 11.6s -> 9.8s.
    //
    // Both cannot be fully right for the same consumer, and the difference is
    // ~0.44s across a `medium` warm build — smaller than the ~10s run-to-run
    // noise on a 4-CPU Docker VM, so it cannot be settled on a busy machine.
    // #1158 has the full analysis and the exact A/B to run.
    //
    // Until someone resolves it with repeatable numbers (PERF.md `--matrix
    // --repeat 5` on a quiet box), this stays as-is: it is the behaviour a
    // closed regression test asserts, and reverting it on reasoning alone
    // would risk reopening a 14x dev-inner-loop regression to chase a
    // sub-second one. If you do change it, CLAUDE.md's guidance is to gate the
    // now() seed on the *consumer* (make/ninja need fresh outputs; cargo may
    // not) rather than re-globalizing either behaviour.
    let batch_floor = std::time::SystemTime::now();
    floor_materialized_outputs_to_input_max(
        targets.iter().map(|out| out.as_ref()),
        floor_paths.iter().map(|path| path.as_ref()),
        batch_floor,
    );
    Ok(observed)
}

pub(in crate::daemon::server) fn cache_blob_missing(
    path: &Path,
    error: std::io::Error,
) -> MaterializationFailure {
    MaterializationFailure::CacheBlobMissing(CacheBlobMissing {
        path: path.into(),
        error,
    })
}

fn payload_count_mismatch(target_count: usize, payload_count: usize) -> MaterializationFailure {
    cache_read_failure(
        Path::new("<cached-payload-set>"),
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "cached target count {target_count} does not match payload count {payload_count}"
            ),
        ),
    )
}

pub(in crate::daemon::server) fn cache_read_failure(
    path: &Path,
    error: std::io::Error,
) -> MaterializationFailure {
    MaterializationFailure::CacheRead(CacheReadFailure {
        path: path.into(),
        error,
    })
}

fn destination_write_failure(path: &Path, error: std::io::Error) -> MaterializationFailure {
    MaterializationFailure::DestinationWrite(DestinationWriteFailure {
        path: path.into(),
        error,
    })
}

fn classify_file_materialization_error(
    output_path: &Path,
    cache_path: &Path,
    error: std::io::Error,
) -> MaterializationFailure {
    match std::fs::metadata(cache_path) {
        Err(source_error) if source_error.kind() == std::io::ErrorKind::NotFound => {
            cache_blob_missing(cache_path, source_error)
        }
        _ => destination_write_failure(output_path, error),
    }
}

fn classify_cache_read_error(cache_path: &Path, error: std::io::Error) -> MaterializationFailure {
    match std::fs::metadata(cache_path) {
        Err(source_error) if source_error.kind() == std::io::ErrorKind::NotFound => {
            cache_blob_missing(cache_path, source_error)
        }
        _ => cache_read_failure(cache_path, error),
    }
}
