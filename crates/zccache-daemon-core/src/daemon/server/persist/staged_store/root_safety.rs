//! Exact-root validation and safe link/reparse removal for staged artifacts.

use std::fs;
use std::io;
use std::path::Path;

// Root naming, root validation and lock-file opening are re-exported from
// `zccache-artifact` rather than reimplemented here, so the daemon and the
// in-process `zccache warm` command provably contend on one lock file.
// See `zccache_artifact::staged_lock`.
pub(super) use zccache_artifact::staged_lock::{
    is_staged_link_or_reparse, open_store_lock, staged_root, validate_staged_root_path,
};

pub(super) fn remove_staged_link_or_reparse(
    path: &Path,
    metadata: &fs::Metadata,
) -> io::Result<()> {
    use crate::platform::fs::links::LinkKind;
    if crate::platform::fs::links::classify(path)? != LinkKind::Regular {
        // A reparse point that is a directory (Windows junctions) is removed
        // with `remove_dir`; every other link form is a file to `remove_file`.
        if metadata.is_dir() {
            fs::remove_dir(path)
        } else {
            fs::remove_file(path)
        }
    } else {
        fs::remove_file(path)
    }
}

pub(in crate::daemon::server) fn validate_staged_artifact_root(
    artifact_dir: &Path,
) -> io::Result<bool> {
    validate_staged_root_path(staged_root(artifact_dir).as_path())
}
