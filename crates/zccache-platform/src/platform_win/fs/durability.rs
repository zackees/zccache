//! Windows directory durability: there is no directory-fsync primitive, so
//! this is a no-op. `MoveFileExW` with `MOVEFILE_WRITE_THROUGH` covers the
//! rename side.

use std::path::Path;

pub fn open_shared_append(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::windows::fs::OpenOptionsExt;
    let mut options = std::fs::OpenOptions::new();
    options.create(true).append(true).share_mode(0x1 | 0x2 | 0x4);
    options.open(path)
}

pub fn sync_directory(_path: &Path) -> std::io::Result<()> {
    Ok(())
}
