//! Cross-platform hardlink helpers: hardlink-detach (write-without-mutating-cache),
//! link count, file-identity equality, and the Windows file-id query.

use super::*;

#[cfg(test)]
static FAIL_DETACH_REMOVE_PATHS: std::sync::OnceLock<
    std::sync::Mutex<std::collections::HashSet<PathBuf>>,
> = std::sync::OnceLock::new();
#[cfg(test)]
static FAIL_DETACH_RENAME_PATHS: std::sync::OnceLock<
    std::sync::Mutex<std::collections::HashSet<PathBuf>>,
> = std::sync::OnceLock::new();

pub(in crate::daemon::server) fn remove_output_file(path: &Path) -> std::io::Result<()> {
    #[cfg(test)]
    if let Ok(mut injected) = FAIL_DETACH_REMOVE_PATHS
        .get_or_init(|| std::sync::Mutex::new(std::collections::HashSet::new()))
        .lock()
    {
        if injected.remove(path) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "injected detach remove failure",
            ));
        }
    }
    std::fs::remove_file(path)
}

fn rename_detached_output(from: &Path, to: &Path) -> std::io::Result<()> {
    #[cfg(test)]
    if let Ok(mut injected) = FAIL_DETACH_RENAME_PATHS
        .get_or_init(|| std::sync::Mutex::new(std::collections::HashSet::new()))
        .lock()
    {
        if injected.remove(to) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "injected detach rename failure",
            ));
        }
    }
    std::fs::rename(from, to)
}

#[cfg(test)]
pub(in crate::daemon::server) fn fail_detach_remove_for_test(path: &Path) {
    FAIL_DETACH_REMOVE_PATHS
        .get_or_init(|| std::sync::Mutex::new(std::collections::HashSet::new()))
        .lock()
        .expect("detach failure injection lock")
        .insert(path.to_path_buf());
}

#[cfg(test)]
pub(in crate::daemon::server) fn fail_detach_rename_for_test(path: &Path) {
    FAIL_DETACH_RENAME_PATHS
        .get_or_init(|| std::sync::Mutex::new(std::collections::HashSet::new()))
        .lock()
        .expect("detach rename failure injection lock")
        .insert(path.to_path_buf());
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(in crate::daemon::server) struct FileId {
    pub(in crate::daemon::server) volume_serial: u64,
    pub(in crate::daemon::server) identifier: [u8; 16],
}

pub(in crate::daemon::server) fn break_output_hardlink_before_compile(
    path: &Path,
) -> std::io::Result<()> {
    match std::fs::metadata(path) {
        Ok(meta) if meta.is_file() => {}
        Ok(_) => return Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    }

    if hard_link_count(path)? <= 1 {
        make_writable(path)?;
        return Ok(());
    }

    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let file_name = path
        .file_name()
        .unwrap_or_else(|| std::ffi::OsStr::new("output"))
        .to_string_lossy();
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let pid = std::process::id();

    let mut last_err = None;
    for attempt in 0..32 {
        let tmp_path = parent.join(format!(
            ".zccache-detach-{pid}-{nonce}-{attempt}-{file_name}"
        ));
        let copy_result = (|| {
            let mut src = std::fs::File::open(path)?;
            let mut dst = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&tmp_path)?;
            std::io::copy(&mut src, &mut dst)?;
            dst.sync_all()?;
            let permissions = src.metadata()?.permissions();
            std::fs::set_permissions(&tmp_path, permissions)?;
            Ok::<(), std::io::Error>(())
        })();

        match copy_result {
            Ok(()) => {
                let registration = prepare_registered_detach(path);
                if let Err(error) = make_writable(path) {
                    let _ = std::fs::remove_file(&tmp_path);
                    return Err(error);
                }
                if let Err(e) = remove_output_file(path) {
                    if let Some((_, blob_path)) = &registration {
                        let _ = set_readonly(blob_path, readonly_enabled());
                    }
                    let _ = std::fs::remove_file(&tmp_path);
                    return Err(e);
                }
                if let Err(e) = rename_detached_output(&tmp_path, path) {
                    if let Some((id, blob_path)) = &registration {
                        let _ = set_readonly(blob_path, readonly_enabled());
                        commit_registered_detach(*id, path);
                    }
                    let _ = std::fs::remove_file(&tmp_path);
                    return Err(e);
                }
                if let Some((id, _)) = &registration {
                    commit_registered_detach(*id, path);
                }
                make_writable(path)?;
                if let Some((_, blob_path)) = registration {
                    let _ = set_readonly(&blob_path, readonly_enabled());
                }
                return Ok(());
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                last_err = Some(e);
            }
            Err(e) => {
                let _ = std::fs::remove_file(&tmp_path);
                return Err(e);
            }
        }
    }

    Err(last_err.unwrap_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "failed to create hardlink detach temp file",
        )
    }))
}

pub(in crate::daemon::server) fn set_readonly(path: &Path, readonly: bool) -> std::io::Result<()> {
    let mut permissions = std::fs::metadata(path)?.permissions();
    if permissions.readonly() == readonly {
        return Ok(());
    }
    permissions.set_readonly(readonly);
    std::fs::set_permissions(path, permissions)
}

pub(in crate::daemon::server) fn make_writable(path: &Path) -> std::io::Result<()> {
    if path.exists() && std::fs::metadata(path)?.permissions().readonly() {
        set_readonly(path, false)?;
    }
    Ok(())
}

#[cfg(unix)]
pub(in crate::daemon::server) fn hard_link_count(path: &Path) -> std::io::Result<u64> {
    use std::os::unix::fs::MetadataExt;

    Ok(std::fs::metadata(path)?.nlink())
}

#[cfg(windows)]
pub(in crate::daemon::server) fn hard_link_count(path: &Path) -> std::io::Result<u64> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION, FILE_ATTRIBUTE_NORMAL,
        FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    };

    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();

    unsafe {
        let handle = CreateFileW(
            wide.as_ptr(),
            0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            std::ptr::null_mut(),
        );
        if handle == windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE {
            return Err(std::io::Error::last_os_error());
        }

        let mut info: BY_HANDLE_FILE_INFORMATION = std::mem::zeroed();
        let ok = GetFileInformationByHandle(handle, &mut info);
        let close_result = CloseHandle(handle);

        if ok == 0 {
            return Err(std::io::Error::last_os_error());
        }
        if close_result == 0 {
            return Err(std::io::Error::last_os_error());
        }

        Ok(info.nNumberOfLinks as u64)
    }
}

/// Check if two paths refer to the same file (hardlink check).
///
/// Returns `false` if either file doesn't exist or the check fails.
#[cfg(unix)]
pub(in crate::daemon::server) fn same_file(a: &Path, b: &Path) -> bool {
    get_file_id(a)
        .zip(get_file_id(b))
        .is_some_and(|(a, b)| a == b)
}

#[cfg(unix)]
pub(in crate::daemon::server) fn get_file_id(path: &Path) -> Option<FileId> {
    use std::os::unix::fs::MetadataExt;
    let metadata = std::fs::metadata(path).ok()?;
    let mut identifier = [0_u8; 16];
    identifier[..8].copy_from_slice(&metadata.ino().to_ne_bytes());
    Some(FileId {
        volume_serial: metadata.dev(),
        identifier,
    })
}

#[cfg(windows)]
pub(in crate::daemon::server) fn same_file(a: &Path, b: &Path) -> bool {
    get_file_id(a)
        .zip(get_file_id(b))
        .map(|(ia, ib)| ia == ib)
        .unwrap_or(false)
}

/// Returns the volume serial and native 128-bit file ID. ReFS does not
/// guarantee uniqueness for the legacy 64-bit index.
#[cfg(windows)]
pub(in crate::daemon::server) fn get_file_id(path: &Path) -> Option<FileId> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FileIdInfo, GetFileInformationByHandle, GetFileInformationByHandleEx,
        BY_HANDLE_FILE_INFORMATION, FILE_FLAG_BACKUP_SEMANTICS, FILE_ID_INFO, FILE_SHARE_DELETE,
        FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    };

    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();

    unsafe {
        let handle = CreateFileW(
            wide.as_ptr(),
            0, // no access needed, just metadata
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS,
            std::ptr::null_mut(),
        );
        if handle == windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE {
            return None;
        }

        let mut native: FILE_ID_INFO = std::mem::zeroed();
        let native_ok = GetFileInformationByHandleEx(
            handle,
            FileIdInfo,
            (&raw mut native).cast(),
            std::mem::size_of::<FILE_ID_INFO>() as u32,
        );
        if native_ok != 0 {
            CloseHandle(handle);
            return Some(FileId {
                volume_serial: native.VolumeSerialNumber,
                identifier: native.FileId.Identifier,
            });
        }
        let mut legacy: BY_HANDLE_FILE_INFORMATION = std::mem::zeroed();
        let legacy_ok = GetFileInformationByHandle(handle, &mut legacy);
        CloseHandle(handle);
        if legacy_ok == 0 {
            return None;
        }
        let mut identifier = [0_u8; 16];
        identifier[..4].copy_from_slice(&legacy.nFileIndexLow.to_ne_bytes());
        identifier[4..8].copy_from_slice(&legacy.nFileIndexHigh.to_ne_bytes());
        Some(FileId {
            volume_serial: u64::from(legacy.dwVolumeSerialNumber),
            identifier,
        })
    }
}
