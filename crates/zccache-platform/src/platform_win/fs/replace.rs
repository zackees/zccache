//! Windows atomic replace: `MoveFileExW` with verbatim paths and the
//! AV-scanner retry ladder.

use std::os::windows::ffi::OsStrExt;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use windows_sys::Win32::Storage::FileSystem::{
    MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
};

use super::verbatim_path;

/// Uniquifies intermediate backup names in `install_directory`.
static DIRECTORY_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Transient sharing errors an antivirus scanner causes around rename
/// (ERROR_ACCESS_DENIED / ERROR_SHARING_VIOLATION), plus any
/// `PermissionDenied`-shaped error.
pub(crate) fn is_av_scan_transient(err: &std::io::Error) -> bool {
    if err.kind() == std::io::ErrorKind::PermissionDenied {
        return true;
    }
    matches!(err.raw_os_error(), Some(5) | Some(32))
}

pub fn is_transient_share_error(error: &std::io::Error) -> bool {
    matches!(error.raw_os_error(), Some(5) | Some(32))
}

pub fn is_lock_contention(error: &std::io::Error) -> bool {
    matches!(error.raw_os_error(), Some(33))
}

/// Retries `op` across the fixed AV-scan delay ladder, five attempts.
pub(crate) fn av_scan_retry<T, F: FnMut() -> std::io::Result<T>>(mut op: F) -> std::io::Result<T> {
    const DELAYS_MS: [u64; 4] = [50, 100, 250, 500];
    let mut last = op();
    for delay in DELAYS_MS {
        match &last {
            Ok(_) => return last,
            Err(err) if !is_av_scan_transient(err) => return last,
            Err(_) => {
                std::thread::sleep(std::time::Duration::from_millis(delay));
                last = op();
            }
        }
    }
    last
}

/// Atomically replaces `destination` with `source` (verbatim long paths,
/// `MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH`, retried past AV
/// scanners). On success `source` no longer exists.
pub fn atomic_replace(source: &Path, destination: &Path) -> std::io::Result<()> {
    let src = verbatim_path(source)?;
    let dst = verbatim_path(destination)?;
    let src_wide: Vec<u16> = src.as_os_str().encode_wide().chain(Some(0)).collect();
    let dst_wide: Vec<u16> = dst.as_os_str().encode_wide().chain(Some(0)).collect();
    av_scan_retry(|| unsafe {
        let ok = MoveFileExW(
            src_wide.as_ptr(),
            dst_wide.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        );
        if ok == 0 {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(())
        }
    })
}

/// Renames `source` to `destination` where the destination must NOT exist
/// (generation rename), retried past AV scanners.
pub(crate) fn rename_without_replace(source: &Path, destination: &Path) -> std::io::Result<()> {
    let src = verbatim_path(source)?;
    let dst = verbatim_path(destination)?;
    let src_wide: Vec<u16> = src.as_os_str().encode_wide().chain(Some(0)).collect();
    let dst_wide: Vec<u16> = dst.as_os_str().encode_wide().chain(Some(0)).collect();
    av_scan_retry(|| unsafe {
        let ok = MoveFileExW(src_wide.as_ptr(), dst_wide.as_ptr(), 0);
        if ok == 0 {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(())
        }
    })
}

/// Replaces `destination` with `source`, falling back to delete-then-rename
/// when a sharing violation keeps the destination pinned (the
/// artifact-store AV path).
pub(crate) fn replace_with_delete_fallback(
    source: &Path,
    destination: &Path,
) -> std::io::Result<()> {
    match atomic_replace(source, destination) {
        Ok(()) => Ok(()),
        Err(_) if destination.exists() => {
            std::fs::remove_file(destination)?;
            rename_without_replace(source, destination)
        }
        Err(err) => Err(err),
    }
}

/// Installs the staged directory tree `staged` over `requested`, which may
/// already exist. Windows has no atomic directory exchange, so the existing
/// tree is first renamed aside to a backup name; on failure it is restored.
pub fn install_directory(staged: &Path, requested: &Path) -> std::io::Result<()> {
    let parent = requested.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)?;
    if !requested.exists() {
        return std::fs::rename(staged, requested);
    }
    let backup = parent.join(format!(
        ".zccache-directory-backup-{}-{}",
        std::process::id(),
        DIRECTORY_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::rename(requested, &backup)?;
    if let Err(error) = std::fs::rename(staged, requested) {
        let _ = std::fs::rename(&backup, requested);
        return Err(error);
    }
    remove_directory_if_present(&backup)
}

fn remove_directory_if_present(path: &Path) -> std::io::Result<()> {
    match std::fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}
