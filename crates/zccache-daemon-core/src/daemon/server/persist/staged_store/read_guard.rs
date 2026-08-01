//! Shared staged-generation ownership for cache-hit materialization.

use super::{is_staged_link_or_reparse, open_store_lock, pointer_path, staged_root, STAGED_ROOT};
use std::fs::{self, File};
use std::io;
use std::path::Path;
use std::time::Instant;

pub(in crate::daemon::server) fn staged_key_supported(key_hex: &str) -> bool {
    !key_hex.is_empty()
        && key_hex.len() <= 128
        && key_hex.bytes().all(|byte| byte.is_ascii_hexdigit())
}

pub(in crate::daemon::server) fn is_staged_artifact_path(path: &Path) -> bool {
    let mut components = path.components().rev();
    let Some(output) = components
        .next()
        .and_then(|component| component.as_os_str().to_str())
    else {
        return false;
    };
    let Some(generation) = components
        .next()
        .and_then(|component| component.as_os_str().to_str())
    else {
        return false;
    };
    let Some(key) = components
        .next()
        .and_then(|component| component.as_os_str().to_str())
    else {
        return false;
    };
    let Some(root) = components
        .next()
        .and_then(|component| component.as_os_str().to_str())
    else {
        return false;
    };
    let root_matches = if cfg!(windows) {
        root.eq_ignore_ascii_case(STAGED_ROOT)
    } else {
        root == STAGED_ROOT
    };
    let key_matches =
        key.len() <= 128 && !key.is_empty() && key.bytes().all(|byte| byte.is_ascii_hexdigit());
    let generation_matches =
        generation.len() == 64 && generation.bytes().all(|byte| byte.is_ascii_hexdigit());
    let output_matches = output
        .strip_prefix("output-")
        .is_some_and(|index| !index.is_empty() && index.bytes().all(|byte| byte.is_ascii_digit()));
    root_matches && key_matches && generation_matches && output_matches
}

pub(in crate::daemon::server) fn is_staged_artifact_root(path: &Path) -> bool {
    path.file_name().is_some_and(|name| {
        if cfg!(windows) {
            name.to_string_lossy().eq_ignore_ascii_case(STAGED_ROOT)
        } else {
            name == STAGED_ROOT
        }
    })
}

pub(super) fn validate_key(key_hex: &str) -> io::Result<()> {
    if !staged_key_supported(key_hex) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "staged artifact key must be a bounded hexadecimal string",
        ));
    }
    Ok(())
}

/// Shared ownership of the staged store while resolved generation paths are
/// being delivered to caller-owned destinations.
///
/// Cleanup and eviction take the same file lock exclusively. Keeping this
/// guard in the typed materialization payload prevents a generation from
/// disappearing between resolution and the final reflink/hardlink/copy.
pub(in crate::daemon::server) struct StagedMaterializationGuard {
    _store_lock: File,
    wait_ns: u64,
    acquired_at: Instant,
}

impl StagedMaterializationGuard {
    pub(in crate::daemon::server) fn timings_ns(&self) -> (u64, u64) {
        (
            self.wait_ns,
            self.acquired_at.elapsed().as_nanos().min(u64::MAX as u128) as u64,
        )
    }
}

fn acquire_staged_materialization_guard(
    artifact_dir: &Path,
) -> io::Result<StagedMaterializationGuard> {
    acquire_staged_materialization_guard_from(artifact_dir, Instant::now())
}

/// Acquire the shared read lock, attributing everything since `wait_started`
/// to the wait.
///
/// #1215: the timer used to start *after* `staged_root` and `open_store_lock`,
/// so `HitStoreLockWait` reported only the `lock_shared` call. Opening the lock
/// file is a real filesystem operation on every staged hit, and the caller's
/// pointer probe is another — excluding both meant the telemetry could not
/// account for guard-acquisition cost, which is exactly what it exists to
/// attribute. Callers that probe first pass their own start instant so their
/// probe lands inside the window.
fn acquire_staged_materialization_guard_from(
    artifact_dir: &Path,
    wait_started: Instant,
) -> io::Result<StagedMaterializationGuard> {
    let root = staged_root(artifact_dir);
    let store_lock = open_store_lock(&root)?;
    fs2::FileExt::lock_shared(&store_lock)?;
    let wait_ns = wait_started.elapsed().as_nanos().min(u64::MAX as u128) as u64;
    Ok(StagedMaterializationGuard {
        _store_lock: store_lock,
        wait_ns,
        acquired_at: Instant::now(),
    })
}

/// Acquire staged-store read ownership only when this key currently has a
/// staged pointer. If no pointer exists, callers deliberately resolve with
/// staged lookup disabled for that attempt: a concurrent publisher may become
/// visible on the next lookup, but no unguarded staged path can escape.
pub(in crate::daemon::server) fn acquire_staged_materialization_guard_if_present(
    artifact_dir: &Path,
    key_hex: &str,
) -> io::Result<Option<StagedMaterializationGuard>> {
    // Start before the pointer probe (#1215): `symlink_metadata` is a stat on
    // every staged hit and belongs to acquisition cost, not to free time.
    let wait_started = Instant::now();
    validate_key(key_hex)?;
    let pointer = pointer_path(artifact_dir, key_hex);
    match fs::symlink_metadata(&pointer) {
        Ok(metadata) if is_staged_link_or_reparse(&metadata) => Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "refusing staged artifact materialization through linked/reparse pointer: {}",
                pointer.display()
            ),
        )),
        Ok(_) => acquire_staged_materialization_guard_from(artifact_dir, wait_started).map(Some),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

pub(in crate::daemon::server) fn acquire_staged_materialization_guard_for_cached_path(
    artifact_dir: &Path,
) -> io::Result<StagedMaterializationGuard> {
    acquire_staged_materialization_guard(artifact_dir)
}

#[cfg(test)]
mod tests {
    use super::*;
    /// The `_if_present` path starts its timer before the pointer probe, so a
    /// key with no staged pointer costs a stat and reports no guard at all.
    #[test]
    fn a_key_without_a_staged_pointer_acquires_no_guard() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(staged_root(dir.path())).unwrap();

        let guard =
            acquire_staged_materialization_guard_if_present(dir.path(), &"a".repeat(64)).unwrap();

        assert!(
            guard.is_none(),
            "no staged pointer means no lock is taken; callers resolve with \
             staged lookup disabled for that attempt"
        );
    }
}
