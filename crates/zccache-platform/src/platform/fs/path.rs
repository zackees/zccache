//! Host path-key normalization primitives consumed by zccache-core's
//! `NormalizedPath`. Only the OS-dependent mechanics live here; lexical
//! normalization, hashing, and cache-key policy stay with the caller.

use std::path::{Path, PathBuf};

use crate::platform_imp;

/// Strips the Windows extended-length (`\\?\`) and verbatim (`\\?\UNC\`)
/// prefixes. A no-op on hosts where those prefixes do not exist.
pub fn strip_verbatim_prefix(path: &Path) -> PathBuf {
    platform_imp::fs::path::strip_verbatim_prefix(path)
}

/// Returns the host's canonical casing for `path` when the host treats
/// paths case-insensitively; on case-sensitive hosts this is a no-op.
pub fn case_fold(path: &Path) -> PathBuf {
    platform_imp::fs::path::case_fold(path)
}

/// Converts an MSYS/`/c/...`-style path to its native Windows form; a
/// no-op on non-Windows hosts.
pub fn from_msys(path: &Path) -> Option<PathBuf> {
    platform_imp::fs::path::from_msys(path)
}

/// On macOS, maps the `/private/var` (and `/private/etc`, `/private/tmp`)
/// prefix to its canonical `/var` form so two spellings of the same
/// directory compare equal; a no-op elsewhere.
pub fn canonicalize_private_prefix(path: &Path) -> PathBuf {
    platform_imp::fs::path::canonicalize_private_prefix(path)
}

/// Returns `path` in the host's verbatim (extended-length) form, when the
/// host requires one for manual Win32 calls. On hosts without verbatim
/// paths this is an identity return.
pub fn verbatim_path(path: &Path) -> std::io::Result<PathBuf> {
    platform_imp::fs::path::verbatim_path(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_verbatim_prefix_is_a_noop_for_plain_paths() {
        let plain = std::path::Path::new("C:/cache/artifact");
        assert_eq!(strip_verbatim_prefix(plain), plain);
    }

    #[test]
    fn case_fold_is_stable() {
        let dir = tempfile::tempdir().expect("tempdir");
        let p = dir.path().join("mixed").join("Case");
        let once = case_fold(&p);
        let twice = case_fold(&p);
        assert_eq!(once, twice);
    }
}
