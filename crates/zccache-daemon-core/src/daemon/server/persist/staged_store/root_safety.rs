//! Exact-root validation and safe link/reparse removal for staged artifacts.

use crate::core::NormalizedPath;
use std::fs::{self, File};
use std::io;
use std::path::Path;

// Root naming, root validation and lock-file opening are delegated to
// `zccache-artifact` so the daemon and the in-process `zccache warm` command
// contend on one lock. See `zccache_artifact::staged_lock`.
pub(super) use zccache_artifact::staged_lock::{
    is_staged_link_or_reparse, validate_staged_root_path,
};

pub(super) fn staged_root(artifact_dir: &Path) -> NormalizedPath {
    zccache_artifact::staged_lock::staged_root(artifact_dir).into()
}

pub(super) fn open_store_lock(root: &Path) -> io::Result<File> {
    zccache_artifact::staged_lock::open_store_lock(root)
}

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
