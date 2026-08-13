//! Linux path-key normalization: all no-ops (case-sensitive, no verbatim
//! prefix, no MSYS).

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
