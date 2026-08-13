//! Windows directory durability: there is no directory-fsync primitive, so
//! this is a no-op. `MoveFileExW` with `MOVEFILE_WRITE_THROUGH` covers the
//! rename side.

use std::path::Path;

pub fn sync_directory(_path: &Path) -> std::io::Result<()> {
    Ok(())
}
