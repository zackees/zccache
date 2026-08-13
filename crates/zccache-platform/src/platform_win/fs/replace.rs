//! Windows atomic replace: `MoveFileExW` with verbatim paths and the
//! AV-scanner retry ladder.

use std::os::windows::ffi::OsStrExt;
use std::path::Path;

use windows_sys::Win32::Storage::FileSystem::{
    MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
};

use super::verbatim_path;

/// Transient sharing errors an antivirus scanner causes around rename
/// (ERROR_ACCESS_DENIED / ERROR_SHARING_VIOLATION), plus any
/// `PermissionDenied`-shaped error.
pub(crate) fn is_av_scan_transient(err: &std::io::Error) -> bool {
    if err.kind() == std::io::ErrorKind::PermissionDenied {
        return true;
    }
    matches!(err.raw_os_error(), Some(5) | Some(32))
}

/// Retries `op` across the fixed AV-scan delay ladder, five attempts.
pub(crate) fn av_scan_retry<T, F: FnMut() -> std::io::Result<T>>(
    mut op: F,
) -> std::io::Result<T> {
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
#[allow(dead_code)] // consumed by the daemon caller rewiring in this PR
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
#[allow(dead_code)] // consumed by the daemon caller rewiring in this PR
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
