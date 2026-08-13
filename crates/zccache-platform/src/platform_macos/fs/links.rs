//! macOS link counts and classification.

use std::os::unix::fs::MetadataExt;
use std::path::Path;

use crate::platform::fs::LinkKind;

pub fn hard_link_count(path: &Path) -> std::io::Result<u64> {
    Ok(std::fs::metadata(path)?.nlink())
}

pub fn classify(path: &Path) -> std::io::Result<LinkKind> {
    let meta = std::fs::symlink_metadata(path)?;
    Ok(if meta.file_type().is_symlink() {
        LinkKind::Symlink
    } else {
        LinkKind::Regular
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    #[test]
    fn symlinks_classify_as_symlink() {
        let dir = tempfile::tempdir().expect("tempdir");
        let target = dir.path().join("target");
        let link = dir.path().join("link");
        std::fs::write(&target, b"data").expect("write");
        symlink(&target, &link).expect("symlink");
        assert_eq!(classify(&link).expect("classify"), LinkKind::Symlink);
    }
}
