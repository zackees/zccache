//! macOS path-key normalization: Unicode case folding and the
//! `/private/var` prefix mapping.

use std::os::unix::ffi::OsStringExt;
use std::path::{Path, PathBuf};

/// macOS has no `\\?\` verbatim prefix.
pub fn strip_verbatim_prefix(path: &Path) -> PathBuf {
    path.to_path_buf()
}

/// macOS path comparison folds Unicode case (HFS+/APFS default).
pub fn case_fold(path: &Path) -> PathBuf {
    PathBuf::from(path.to_string_lossy().to_lowercase())
}

/// MSYS paths are a Windows concept.
pub fn from_msys(_path: &Path) -> Option<PathBuf> {
    None
}

/// Maps `/private/var` (and the other `/private` roots) to the canonical
/// spelling so both forms compare equal.
pub fn canonicalize_private_prefix(path: &Path) -> PathBuf {
    let text = path.to_string_lossy();
    const PRIVATE_PREFIXES: &[&str] = &["/private/var", "/private/etc", "/private/tmp"];
    for prefix in PRIVATE_PREFIXES {
        if let Some(rest) = text.strip_prefix(prefix) {
            let canonical = prefix.trim_start_matches("/private");
            return PathBuf::from(format!("{canonical}{rest}"));
        }
    }
    path.to_path_buf()
}

/// macOS has no verbatim (`\\?\`) path form.
pub fn verbatim_path(path: &Path) -> std::io::Result<PathBuf> {
    Ok(path.to_path_buf())
}

pub fn from_raw_bytes(bytes: &[u8]) -> Option<PathBuf> {
    Some(std::ffi::OsString::from_vec(bytes.to_vec()).into())
}

pub fn system_root_candidate(path: &Path) -> Option<PathBuf> {
    Some(Path::new("/").join(path))
}
