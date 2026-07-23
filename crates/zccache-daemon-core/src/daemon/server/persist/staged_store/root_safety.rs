//! Exact-root validation and safe link/reparse removal for staged artifacts.

use super::{STAGED_ROOT, STORE_LOCK};
use crate::core::NormalizedPath;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::Path;

pub(super) fn staged_root(artifact_dir: &Path) -> NormalizedPath {
    artifact_dir.join(STAGED_ROOT).into()
}

pub(super) fn open_store_lock(root: &Path) -> io::Result<File> {
    if !validate_staged_root_path(root)? {
        fs::create_dir_all(root)?;
        if !validate_staged_root_path(root)? {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("staged artifact root disappeared: {}", root.display()),
            ));
        }
    }
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(root.join(STORE_LOCK))
}

#[cfg(windows)]
pub(super) fn is_staged_link_or_reparse(metadata: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(windows))]
pub(super) fn is_staged_link_or_reparse(metadata: &fs::Metadata) -> bool {
    metadata.file_type().is_symlink()
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

pub(super) fn validate_staged_root_path(root: &Path) -> io::Result<bool> {
    let metadata = match fs::symlink_metadata(root) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    if is_staged_link_or_reparse(&metadata) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "refusing staged artifact access through linked/reparse root: {}",
                root.display()
            ),
        ));
    }
    if !metadata.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "staged artifact root is not a directory: {}",
                root.display()
            ),
        ));
    }
    Ok(true)
}

pub(in crate::daemon::server) fn validate_staged_artifact_root(
    artifact_dir: &Path,
) -> io::Result<bool> {
    validate_staged_root_path(staged_root(artifact_dir).as_path())
}
