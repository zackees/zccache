//! Windows path-key normalization: verbatim-prefix stripping, ASCII case
//! folding, and MSYS `/c/...` conversion.

use std::path::{Path, PathBuf};

/// Strips the `\\?\` extended prefix, if present. `\\?\UNC\` also
/// collapses to `\\`.
pub fn strip_verbatim_prefix(path: &Path) -> PathBuf {
    let text = path.to_string_lossy();
    if let Some(stripped) = text.strip_prefix(r"\\?\") {
        let stripped = if let Some(unc) = stripped.strip_prefix("UNC\\") {
            format!(r"\\{unc}")
        } else {
            stripped.to_string()
        };
        PathBuf::from(stripped)
    } else {
        path.to_path_buf()
    }
}

/// Windows path comparison folds ASCII case.
pub fn case_fold(path: &Path) -> PathBuf {
    PathBuf::from(
        path.to_string_lossy()
            .replace('\\', "/")
            .to_ascii_lowercase(),
    )
}

/// Converts an MSYS `/c/...`-style path (or bare `/c`) to `C:\...`.
pub fn from_msys(path: &Path) -> Option<PathBuf> {
    let text = path.to_str()?;
    let rest = text.strip_prefix('/')?;
    let mut chars = rest.chars();
    let drive = chars.next()?.to_ascii_uppercase();
    let remainder: String = chars.collect();
    if remainder.is_empty() {
        Some(PathBuf::from(format!("{drive}:\\")))
    } else {
        remainder
            .strip_prefix('/')
            .map(|rest| PathBuf::from(format!("{drive}:\\{}", rest.replace('/', "\\"))))
    }
}

/// Windows has no `/private` prefix mapping.
pub fn canonicalize_private_prefix(path: &Path) -> PathBuf {
    path.to_path_buf()
}

/// The verbatim (`\\?\`) form required by manual Win32 calls; the logic
/// lives in `super::verbatim` and is re-exported so the neutral facade can
/// reach it through the `path` module.
pub(crate) use super::verbatim_path;

pub fn from_raw_bytes(bytes: &[u8]) -> Option<PathBuf> {
    std::str::from_utf8(bytes).ok().map(PathBuf::from)
}

pub fn system_root_candidate(_path: &Path) -> Option<PathBuf> {
    None
}
