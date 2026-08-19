//! Linux path-key normalization: all no-ops (case-sensitive, no verbatim
//! prefix, no MSYS).

use std::os::unix::ffi::OsStringExt;
use std::path::{Path, PathBuf};

/// Linux has no `\\?\` verbatim prefix.
pub fn strip_verbatim_prefix(path: &Path) -> PathBuf {
    path.to_path_buf()
}

/// Linux paths compare case-sensitively.
pub fn case_fold(path: &Path) -> PathBuf {
    path.to_path_buf()
}

/// MSYS paths are a Windows concept.
pub fn from_msys(_path: &Path) -> Option<PathBuf> {
    None
}

/// Linux has no `/private` prefix mapping.
pub fn canonicalize_private_prefix(path: &Path) -> PathBuf {
    path.to_path_buf()
}

/// Linux has no verbatim (`\\?\`) path form.
pub fn verbatim_path(path: &Path) -> std::io::Result<PathBuf> {
    Ok(path.to_path_buf())
}

pub fn from_raw_bytes(bytes: &[u8]) -> Option<PathBuf> {
    Some(std::ffi::OsString::from_vec(bytes.to_vec()).into())
}
