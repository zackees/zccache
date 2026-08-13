//! Neutral host-executable naming, discovery, and comparison primitives.

use std::ffi::{OsStr, OsString};
use std::io;
use std::path::{Path, PathBuf};

/// Returns `stem` with the host's native executable suffix when needed.
#[must_use]
pub fn native_name(stem: &OsStr) -> OsString {
    crate::platform_imp::executable::native_name(stem)
}

/// Returns `stem` with the host's native dynamic-library suffix.
#[must_use]
pub fn native_library_name(stem: &OsStr) -> OsString {
    crate::platform_imp::executable::native_library_name(stem)
}

/// Conventional host locations for the libclang shared library.
#[must_use]
pub fn clang_library_candidates() -> Vec<PathBuf> {
    crate::platform_imp::executable::clang_library_candidates()
}

/// Finds a runnable host image in an explicit ordered directory list.
#[must_use]
pub fn find_in_paths(name: &OsStr, directories: &[PathBuf]) -> Option<PathBuf> {
    crate::platform_imp::executable::find_in_paths(name, directories)
}

/// Finds a runnable host image using the process `PATH`.
#[must_use]
pub fn find_on_path(name: &OsStr) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    let directories: Vec<_> = std::env::split_paths(&path).collect();
    find_in_paths(name, &directories)
}

/// Returns the current process image path.
pub fn current_image() -> io::Result<PathBuf> {
    std::env::current_exe()
}

/// Reports whether two paths identify the same executable image.
pub fn images_equal(left: &Path, right: &Path) -> io::Result<bool> {
    crate::fs::identity::same_file(left, right)
}

/// Compares a host executable's file stem with a product stem.
#[must_use]
pub fn stem_matches(path: &OsStr, expected: &str) -> bool {
    crate::platform_imp::executable::stem_matches(path, expected)
}

/// Relocates a running image when the host locks executable files.
pub fn unlock_for_replacement(image: &Path) -> io::Result<bool> {
    crate::platform_imp::executable::unlock_for_replacement(image)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_name_and_current_image_are_host_runnable() {
        let name = native_name(OsStr::new("zccache-probe"));
        assert!(!name.is_empty());
        assert!(!native_library_name(OsStr::new("libclang")).is_empty());

        let image = current_image().expect("current executable image");
        assert!(image.is_file());
        assert!(images_equal(&image, &image).expect("same-image comparison"));
    }

    #[test]
    fn explicit_path_lookup_finds_the_current_image() {
        let image = current_image().expect("current executable image");
        let directory = image.parent().expect("image directory").to_path_buf();
        let name = image.file_name().expect("image name");
        assert_eq!(find_in_paths(name, &[directory]), Some(image));
    }

    #[test]
    fn replacement_unlock_matches_host_locking_semantics() {
        let directory = tempfile::tempdir().expect("tempdir");
        let image = directory.path().join(native_name(OsStr::new("probe")));
        std::fs::write(&image, b"image").expect("write image");
        let relocated = unlock_for_replacement(&image).expect("unlock");
        assert_eq!(relocated, cfg!(windows));
        assert!(image.is_file());
    }
}
