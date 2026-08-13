//! macOS atomic replace: rename(2) is atomic on a single filesystem.

use std::path::Path;

pub fn atomic_replace(source: &Path, destination: &Path) -> std::io::Result<()> {
    std::fs::rename(source, destination)
}
