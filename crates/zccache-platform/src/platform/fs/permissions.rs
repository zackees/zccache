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
    // A materialization destination can be a dangling link.  Callers remove
    // that directory entry immediately after this step, so there is no
    // referent whose permissions could be changed. Treat that specific
    // entry as writable; an actually absent path remains an error.
    match platform_imp::fs::permissions::make_writable(path) {
        Ok(()) => Ok(()),
        Err(error)
            if error.kind() == std::io::ErrorKind::NotFound
                && std::fs::symlink_metadata(path)
                    .map(|metadata| metadata.file_type().is_symlink())
                    .unwrap_or(false) =>
        {
            Ok(())
        }
        Err(error) => Err(error),
    }
}

/// Makes `path` executable, touching only the executable bits.
pub fn make_executable(path: &Path) -> std::io::Result<()> {
    platform_imp::fs::permissions::make_executable(path)
}

/// The host's mode representation of `metadata`: unix mode bits on Unix,
/// the `0`/`1` readonly attribute on Windows.
pub fn mode(metadata: &std::fs::Metadata) -> u32 {
    platform_imp::fs::permissions::mode(metadata)
}

/// Applies a mode previously read with [`mode`] back to `path`.
pub fn apply_mode(path: &Path, mode: u32) -> std::io::Result<()> {
    platform_imp::fs::permissions::apply_mode(path, mode)
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

    #[cfg(unix)]
    #[test]
    fn dangling_symlink_is_removable_without_a_permission_change() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().expect("tempdir");
        let link = dir.path().join("output");
        symlink(dir.path().join("missing"), &link).expect("symlink");

        make_writable(&link).expect("dangling symlink is removable");
        fs::remove_file(link).expect("remove dangling symlink");
    }

    #[cfg(unix)]
    #[test]
    fn valid_symlink_makes_its_referent_writable() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().expect("tempdir");
        let target = dir.path().join("target");
        let link = dir.path().join("output");
        fs::write(&target, b"data").expect("write");
        set_readonly(&target, true).expect("readonly target");
        symlink(&target, &link).expect("symlink");

        make_writable(&link).expect("make referent writable");
        assert!(!fs::metadata(target).expect("target metadata").permissions().readonly());
    }
}
