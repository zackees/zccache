//! Neutral link counting and link/reparse classification.

use std::path::Path;

use crate::platform_imp;

/// What kind of directory entry a path is, without exposing native
/// attribute bits (Windows reparse attributes, Unix mode bits).
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum LinkKind {
    /// An ordinary file or directory.
    Regular,
    /// A symbolic link (Unix) or a reparse point that is a name-surrogate
    /// symlink (Windows).
    Symlink,
    /// A reparse point that is not a symlink (Windows junctions, mount
    /// points, cloud placeholders, …). Callers must refuse to walk these.
    Reparse,
}

/// Number of hard links to the file at `path`.
pub fn hard_link_count(path: &Path) -> std::io::Result<u64> {
    platform_imp::fs::links::hard_link_count(path)
}

/// Classifies the entry at `path`.
pub fn classify(path: &Path) -> std::io::Result<LinkKind> {
    platform_imp::fs::links::classify(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn hard_link_count_grows_with_each_link() {
        let dir = tempfile::tempdir().expect("tempdir");
        let a = dir.path().join("a");
        let b = dir.path().join("b");
        fs::write(&a, b"data").expect("write");
        let before = hard_link_count(&a).expect("count");
        fs::hard_link(&a, &b).expect("link");
        let after = hard_link_count(&a).expect("count");
        assert_eq!(after, before + 1);
        assert_eq!(after, hard_link_count(&b).expect("count"));
    }

    #[test]
    fn ordinary_files_classify_as_regular() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("f");
        fs::write(&file, b"data").expect("write");
        assert_eq!(classify(&file).expect("classify"), LinkKind::Regular);
    }
}
