//! macOS directory durability: open the directory and fsync it.

use std::path::Path;

pub fn sync_directory(path: &Path) -> std::io::Result<()> {
    std::fs::File::open(path)?.sync_all()
}
