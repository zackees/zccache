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

#[cfg(windows)]
pub(super) fn remove_staged_link_or_reparse(
    path: &Path,
    metadata: &fs::Metadata,
) -> io::Result<()> {
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;
    if metadata.file_attributes() & FILE_ATTRIBUTE_DIRECTORY != 0 {
        fs::remove_dir(path)
    } else {
        fs::remove_file(path)
    }
}

#[cfg(not(windows))]
pub(super) fn remove_staged_link_or_reparse(
    path: &Path,
    _metadata: &fs::Metadata,
) -> io::Result<()> {
    fs::remove_file(path)
}

pub(in crate::daemon::server) fn validate_staged_artifact_root(
    artifact_dir: &Path,
) -> io::Result<bool> {
    validate_staged_root_path(staged_root(artifact_dir).as_path())
}
