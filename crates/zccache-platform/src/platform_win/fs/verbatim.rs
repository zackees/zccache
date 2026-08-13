//! Verbatim (`\\?\`) path construction for manual Win32 calls.
//!
//! Rust's std::fs produces verbatim paths internally, but direct
//! `MoveFileExW`/`GetCompressedFileSizeW` callers otherwise retain the
//! legacy MAX_PATH limit.

use std::path::{Path, PathBuf};

/// Convert a cache file path to the verbatim absolute form required by
/// manual Win32 calls.
pub(crate) fn verbatim_path(path: &Path) -> std::io::Result<PathBuf> {
    let file_name = path.file_name().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("cache path has no filename: {}", path.display()),
        )
    })?;
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    Ok(std::fs::canonicalize(parent)?.join(file_name))
}
