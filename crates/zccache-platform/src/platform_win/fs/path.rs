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
    PathBuf::from(path.to_string_lossy().replace('\\', "/").to_ascii_lowercase())
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
    } else if let Some(rest) = remainder.strip_prefix('/') {
        Some(PathBuf::from(format!("{drive}:\\{}", rest.replace('/', "\\"))))
    } else {
        None
    }
}

/// Windows has no `/private` prefix mapping.
pub fn canonicalize_private_prefix(path: &Path) -> PathBuf {
    path.to_path_buf()
}
