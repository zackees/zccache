//! Linux permission mechanics: mode-bit based, owner-only = 0700.

use std::os::unix::fs::PermissionsExt;
use std::path::Path;

pub fn ensure_dir_private(path: &Path) -> std::io::Result<bool> {
    let metadata = std::fs::metadata(path)?;
    if metadata.permissions().mode() & 0o022 == 0 {
        return Ok(false);
    }
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    let after = std::fs::metadata(path)?;
    if after.permissions().mode() & 0o022 != 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!("{} is writable by others and could not be tightened", path.display()),
        ));
    }
    Ok(true)
}

pub fn create_dir_all_private(path: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(path)?;
    ensure_dir_private(path)
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
