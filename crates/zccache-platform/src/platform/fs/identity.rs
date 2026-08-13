//! Neutral file identity and change markers.
//!
//! `FileIdentity` compares equal for two paths to the same file (hard
//! links, case aliases, `..`/symlink aliases on hosts that resolve them)
//! and unequal for distinct files. `ChangeMarker` is `Some` only when the
//! host can prove a file changed since the marker was taken (e.g. the
//! Windows USN journal); `None` means "cannot prove anything", and callers
//! must treat the file as possibly changed.

use std::hash::Hash;
use std::path::Path;

use crate::platform_imp;

/// Identity of a file on the local filesystem, opaque and comparable.
///
/// Native representations stay private; the concrete trees construct
/// values, callers only compare/hash them.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct FileIdentity(pub(crate) platform_imp::fs::identity::RawFileIdentity);

/// A host-provable file-change marker. `None` when the host cannot prove
/// change since the marker was captured.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct ChangeMarker(pub(crate) Option<platform_imp::fs::identity::RawChangeMarker>);

/// Returns the identity of `path`, or an error if it cannot be read.
pub fn file_identity(path: &Path) -> std::io::Result<FileIdentity> {
    platform_imp::fs::identity::file_identity(path).map(FileIdentity)
}

/// Whether `a` and `b` refer to the same underlying file.
pub fn same_file(a: &Path, b: &Path) -> std::io::Result<bool> {
    platform_imp::fs::identity::same_file(a, b)
}

/// Captures a change marker for `path`. `None` on hosts without a proof
/// primitive; never an error, matching the caller contract that an
/// unprovable marker means "possibly changed".
pub fn change_marker(path: &Path) -> Option<ChangeMarker> {
    Some(ChangeMarker(
        platform_imp::fs::identity::change_marker(path),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn temp_dir() -> tempfile::TempDir {
        tempfile::tempdir().expect("tempdir")
    }

    #[test]
    fn hard_links_share_an_identity() {
        let dir = temp_dir();
        let a = dir.path().join("a");
        let b = dir.path().join("b");
        fs::write(&a, b"data").expect("write a");
        fs::hard_link(&a, &b).expect("hard link");
        assert_eq!(file_identity(&a).expect("identity a"), file_identity(&b).expect("identity b"));
        assert!(same_file(&a, &b).expect("same_file"));
    }

    #[test]
    fn a_byte_copy_is_a_distinct_file() {
        let dir = temp_dir();
        let a = dir.path().join("a");
        let b = dir.path().join("b");
        fs::write(&a, b"data").expect("write a");
        fs::copy(&a, &b).expect("copy");
        assert_ne!(file_identity(&a).expect("identity a"), file_identity(&b).expect("identity b"));
        assert!(!same_file(&a, &b).expect("same_file"));
    }

    #[test]
    fn missing_file_is_an_error_not_an_identity() {
        let dir = temp_dir();
        assert!(file_identity(&dir.path().join("gone")).is_err());
    }

    #[test]
    fn change_marker_is_optional_but_stable() {
        let dir = temp_dir();
        let file = dir.path().join("f");
        fs::write(&file, b"v1").expect("write");
        let first = change_marker(&file);
        // Whatever the host proves, two markers of an untouched file must
        // compare equal (None == None included).
        let second = change_marker(&file);
        assert_eq!(first, second);
    }
}
