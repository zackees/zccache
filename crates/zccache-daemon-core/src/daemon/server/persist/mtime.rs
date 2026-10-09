//! Daemon-side seam over the #1771 materialized-output mtime contract.
//!
//! The policy (object mtime, sibling floor, batch floors, recorded-mtime
//! replay) lives in [`crate::core::mtime`] so the CLI and `zccache-artifact`
//! share it; read that module for the contract. It is re-exported here so the
//! daemon's paths are unchanged. What stays here is the delivery plumbing that
//! needs daemon permissions: raising a possibly sealed cache blob.
//!
//! No production file outside `zccache-core`'s `mtime` module may set a file
//! time (workspace guard: `crates/zccache-core/tests/mtime_owner_workspace.rs`).

use super::*;
use crate::core::mtime::FileTime;

pub(in crate::daemon::server) use crate::core::mtime::{
    resolve_hit_mtime, stamp_mtime, stamp_recorded_mtime, BatchPolicy, SiblingFloorPass,
};

/// Apply `policy` to every output of one hit, raising each through
/// [`set_materialized_mtime`] so a sealed blob keeps its seal.
pub(in crate::daemon::server) fn apply_batch_policy<'a>(
    policy: BatchPolicy,
    output_paths: impl IntoIterator<Item = &'a Path>,
    input_paths: impl IntoIterator<Item = &'a Path>,
    seed: std::time::SystemTime,
) {
    crate::core::mtime::apply_batch_policy_with(
        policy,
        output_paths,
        input_paths,
        seed,
        set_materialized_mtime,
    );
}

/// Floor `path` up to its newest sibling artifact, in place. Test helper for
/// the floor's own contract; production flooring goes through
/// [`resolve_hit_mtime`].
#[cfg(test)]
pub(in crate::daemon::server) fn floor_artifact_mtime_to_sibling_max(
    path: &Path,
) -> std::io::Result<()> {
    let own = FileTime::from_last_modification_time(&std::fs::metadata(path)?);
    if let Some(ft) = crate::core::mtime::sibling_floor_above(path, own)? {
        let _ = set_materialized_mtime(path, ft);
    }
    Ok(())
}

/// Raise a (possibly sealed) materialized file's mtime, restoring its seal.
pub(in crate::daemon::server) fn set_materialized_mtime(
    path: &Path,
    mtime: FileTime,
) -> std::io::Result<()> {
    let readonly = crate::platform::fs::permissions::is_sealed(path)?;
    if readonly {
        crate::platform::fs::permissions::make_writable(path)?;
    }
    let result = stamp_mtime(path, mtime);
    if readonly {
        let restore = crate::platform::fs::permissions::seal_cache_blob(path);
        if result.is_ok() {
            restore?;
        }
    }
    result
}
