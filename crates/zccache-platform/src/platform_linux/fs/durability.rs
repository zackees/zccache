//! Linux directory durability: open the directory and fsync it.

use std::path::Path;

pub fn open_shared_append(path: &Path) -> std::io::Result<std::fs::File> {
    std::fs::OpenOptions::new().create(true).append(true).open(path)
}

pub fn sync_directory(path: &Path) -> std::io::Result<()> {
    std::fs::File::open(path)?.sync_all()
}
