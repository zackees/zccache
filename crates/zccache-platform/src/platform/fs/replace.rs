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

/// Renames `source` to `destination` where the destination must NOT exist
/// (generation rename). Retried past AV scanners on Windows.
pub fn rename_without_replace(source: &Path, destination: &Path) -> std::io::Result<()> {
    platform_imp::fs::replace::rename_without_replace(source, destination)
}

/// Replaces `destination` with `source`, falling back to delete-then-rename
/// when a sharing violation keeps the destination pinned (the artifact-store
/// AV path).
pub fn replace_with_delete_fallback(source: &Path, destination: &Path) -> std::io::Result<()> {
    platform_imp::fs::replace::replace_with_delete_fallback(source, destination)
}

/// Installs the staged directory tree `staged` over `requested`, which may
/// already exist. The host chooses the strategy: atomic exchange
/// (renameat2 `RENAME_EXCHANGE` / renamex_np `RENAME_SWAP`) where
/// available, an intermediate-backup dance otherwise. On success `staged`
/// no longer exists.
pub fn install_directory(staged: &Path, requested: &Path) -> std::io::Result<()> {
    platform_imp::fs::replace::install_directory(staged, requested)
}

/// Whether a native sharing failure may clear when retried.
#[must_use]
pub fn is_transient_share_error(error: &std::io::Error) -> bool {
    platform_imp::fs::replace::is_transient_share_error(error)
}

/// Whether a non-blocking native file lock reported contention.
#[must_use]
pub fn is_lock_contention(error: &std::io::Error) -> bool {
    platform_imp::fs::replace::is_lock_contention(error)
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
