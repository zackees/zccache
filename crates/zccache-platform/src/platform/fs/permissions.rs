//! Neutral permission mechanics for cache-owned files and directories.

use std::path::Path;

use crate::platform_imp;

/// Ensures `path` exists and is private to the current user: `0700` on
/// Unix, an owner-only DACL on Windows. Returns `Ok(false)` when already
/// private, `Ok(true)` when tightened, and `Err` when the path is missing
/// or still exposed afterwards — the caller's "tightened / refused"
/// lifecycle contract.
pub fn ensure_dir_private(path: &Path) -> std::io::Result<bool> {
    platform_imp::fs::permissions::ensure_dir_private(path)
}

/// Creates `path` (and parents) with private, user-only permissions from
/// the first `mkdir` call — the Windows variant applies the owner-only
/// DACL at creation time to avoid a post-create window.
pub fn create_dir_all_private(path: &Path) -> std::io::Result<()> {
    platform_imp::fs::permissions::create_dir_all_private(path)
}

/// Sets or clears the read-only attribute/bit without touching unrelated
/// permission bits.
pub fn set_readonly(path: &Path, readonly: bool) -> std::io::Result<()> {
    platform_imp::fs::permissions::set_readonly(path, readonly)
}

/// Makes `path` writable while preserving every unrelated permission bit.
pub fn make_writable(path: &Path) -> std::io::Result<()> {
    platform_imp::fs::permissions::make_writable(path)
}

/// Makes `path` executable, touching only the executable bits.
pub fn make_executable(path: &Path) -> std::io::Result<()> {
    platform_imp::fs::permissions::make_executable(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn private_directories_are_created_and_tightened() {
        let dir = tempfile::tempdir().expect("tempdir");
        let target = dir.path().join("a").join("b");
        create_dir_all_private(&target).expect("create");
        // The path exists and can be re-tightened without error.
        ensure_dir_private(&target).expect("tighten");
        assert!(target.is_dir());
    }

    #[test]
    fn readonly_roundtrip_preserves_writability_of_other_files() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("f");
        let other = dir.path().join("g");
        fs::write(&file, b"data").expect("write");
        fs::write(&other, b"data").expect("write");
        set_readonly(&file, true).expect("readonly");
        make_writable(&file).expect("writable");
        fs::write(&file, b"more").expect("rewrite");
        // The sibling was never touched.
        fs::write(&other, b"more").expect("sibling write");
    }

    #[test]
    fn make_executable_leaves_readonly_files_readable() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("f");
        fs::write(&file, b"data").expect("write");
        make_executable(&file).expect("exec");
        let read = fs::read_to_string(&file).expect("read");
        assert_eq!(read, "data");
    }
}
