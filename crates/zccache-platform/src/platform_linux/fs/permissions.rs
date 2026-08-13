//! Linux permission mechanics: mode-bit based, owner-only = 0700.

use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::Path;

pub fn ensure_dir_private(path: &Path) -> std::io::Result<bool> {
    let metadata = std::fs::metadata(path)?;
    let full_mode = metadata.permissions().mode();
    // Never re-permission a directory zccache does not own. The sticky bit
    // is the OS's marker for a *shared* temp root — `/tmp` and `/var/tmp`
    // are `1777` by design. Tightening one would be wrong for every other
    // process on the machine, and as root it would succeed.
    const STICKY: u32 = 0o1000;
    if full_mode & STICKY != 0 {
        return Ok(false);
    }
    let mode = full_mode & 0o777;
    if mode & 0o022 == 0 {
        return Ok(false);
    }
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    let after = std::fs::metadata(path)?;
    if after.permissions().mode() & 0o022 != 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!(
                "{} is writable by others and could not be tightened",
                path.display()
            ),
        ));
    }
    Ok(true)
}

pub fn create_dir_all_private(path: &Path) -> std::io::Result<()> {
    // Mode at creation time, matching the original unix arm: no window
    // where the directory is live with an inherited mode.
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)
}

pub fn set_readonly(path: &Path, readonly: bool) -> std::io::Result<()> {
    let mut permissions = std::fs::metadata(path)?.permissions();
    let mode = if readonly {
        permissions.mode() & !0o222
    } else {
        permissions.mode() | 0o200
    };
    permissions.set_mode(mode);
    std::fs::set_permissions(path, permissions)
}

pub fn make_writable(path: &Path) -> std::io::Result<()> {
    let mut permissions = std::fs::metadata(path)?.permissions();
    let mode = permissions.mode() | 0o200;
    permissions.set_mode(mode);
    std::fs::set_permissions(path, permissions)
}

pub fn make_executable(path: &Path) -> std::io::Result<()> {
    let mut permissions = std::fs::metadata(path)?.permissions();
    let mode = permissions.mode() | 0o111;
    permissions.set_mode(mode);
    std::fs::set_permissions(path, permissions)
}

pub fn mode(metadata: &std::fs::Metadata) -> u32 {
    metadata.permissions().mode()
}

pub fn apply_mode(path: &Path, mode: u32) -> std::io::Result<()> {
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
}
