//! Neutral atomic-replace primitives.
//!
//! `atomic_replace` atomically replaces `destination` with the content of
//! `source`. The caller owns temp-file naming, retry budgets, and cleanup;
//! the platform owns the native swap semantics (plain `rename` on Unix,
//! `MoveFileExW` with replace on Windows, including verbatim long paths).

use std::path::Path;

use crate::platform_imp;

/// Atomically replaces `destination` with `source`. On success `source` no
/// longer exists; on failure both paths keep their original state where
/// the host can guarantee it.
pub fn atomic_replace(source: &Path, destination: &Path) -> std::io::Result<()> {
    platform_imp::fs::replace::atomic_replace(source, destination)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn replace_swaps_the_destination_atomically() {
        let dir = tempfile::tempdir().expect("tempdir");
        let dst = dir.path().join("dst");
        let src = dir.path().join("src");
        fs::write(&dst, b"old").expect("write dst");
        fs::write(&src, b"new").expect("write src");
        atomic_replace(&src, &dst).expect("replace");
        assert_eq!(fs::read_to_string(&dst).expect("read"), "new");
        assert!(!src.exists(), "source is consumed");
    }

    #[test]
    fn a_failed_replace_leaves_the_destination_intact() {
        let dir = tempfile::tempdir().expect("tempdir");
        let dst = dir.path().join("dst");
        fs::write(&dst, b"old").expect("write dst");
        let missing = dir.path().join("missing");
        assert!(atomic_replace(&missing, &dst).is_err());
        assert_eq!(fs::read_to_string(&dst).expect("read"), "old");
    }
}
