//! Neutral directory-durability primitive: fsync a directory so a rename
//! inside it survives power loss.

use std::path::Path;

use crate::platform_imp;

/// Flushes `path`'s directory entry to stable storage (directory fsync).
/// On hosts where directory handles cannot be fsynced (Windows), a no-op.
pub fn sync_directory(path: &Path) -> std::io::Result<()> {
    platform_imp::fs::durability::sync_directory(path)
}
