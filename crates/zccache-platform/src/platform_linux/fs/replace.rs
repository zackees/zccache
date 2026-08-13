//! Linux atomic replace: rename(2) is atomic on a single filesystem.

use std::path::Path;

pub fn is_transient_share_error(_error: &std::io::Error) -> bool {
    false
}

pub fn is_lock_contention(_error: &std::io::Error) -> bool { false }

pub fn atomic_replace(source: &Path, destination: &Path) -> std::io::Result<()> {
    std::fs::rename(source, destination)
}

pub fn rename_without_replace(source: &Path, destination: &Path) -> std::io::Result<()> {
    std::fs::rename(source, destination)
}

pub fn replace_with_delete_fallback(source: &Path, destination: &Path) -> std::io::Result<()> {
    std::fs::rename(source, destination)
}

/// Installs `staged` over `requested`, exchanging the two trees in place
/// (renameat2 `RENAME_EXCHANGE`) when `requested` already exists, so the
/// old tree is never missing under the requested path.
pub fn install_directory(staged: &Path, requested: &Path) -> std::io::Result<()> {
    let parent = requested.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)?;
    if !requested.exists() {
        return std::fs::rename(staged, requested);
    }
    atomic_exchange_directories(staged, requested)?;
    remove_directory_if_present(staged)
}

fn atomic_exchange_directories(left: &Path, right: &Path) -> std::io::Result<()> {
    use std::os::unix::ffi::OsStrExt;

    let left = std::ffi::CString::new(left.as_os_str().as_bytes()).map_err(invalid_path)?;
    let right = std::ffi::CString::new(right.as_os_str().as_bytes()).map_err(invalid_path)?;
    // SAFETY: both pointers come from live CStrings, and renameat2 does not
    // retain them.
    let result = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            libc::AT_FDCWD,
            left.as_ptr(),
            libc::AT_FDCWD,
            right.as_ptr(),
            libc::RENAME_EXCHANGE,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

fn remove_directory_if_present(path: &Path) -> std::io::Result<()> {
    match std::fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn invalid_path(error: impl std::fmt::Display) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, error.to_string())
}
