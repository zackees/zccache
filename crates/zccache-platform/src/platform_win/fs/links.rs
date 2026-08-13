//! Windows link counts and reparse classification.

use std::os::windows::ffi::OsStrExt;
use std::path::Path;

use windows_sys::Win32::Foundation::CloseHandle;
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION, FILE_ATTRIBUTE_NORMAL,
    FILE_ATTRIBUTE_REPARSE_POINT, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    OPEN_EXISTING,
};

use crate::platform::fs::LinkKind;

/// NTFS reparse tag for name-surrogate symbolic links.
const IO_REPARSE_TAG_SYMLINK: u32 = 0xA000_000C;

pub fn hard_link_count(path: &Path) -> std::io::Result<u64> {
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

/// Classifies `path` by its metadata attributes: name-surrogate symlink
/// reparse points are `Symlink`; any other reparse point is `Reparse`.
pub fn classify(path: &Path) -> std::io::Result<LinkKind> {
    use std::os::windows::fs::MetadataExt;
    let meta = std::fs::symlink_metadata(path)?;
    if meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT == 0 {
        return Ok(LinkKind::Regular);
    }
    Ok(reparse_tag(path).map_or(LinkKind::Reparse, |tag| {
        if tag == IO_REPARSE_TAG_SYMLINK {
            LinkKind::Symlink
        } else {
            LinkKind::Reparse
        }
    }))
}

/// Reads the reparse tag of `path`, which must be a reparse point. The tag
/// is the first u32 of every reparse data buffer.
fn reparse_tag(path: &Path) -> Option<u32> {
    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
    };
    use windows_sys::Win32::System::IO::DeviceIoControl;
    use windows_sys::Win32::System::Ioctl::FSCTL_GET_REPARSE_POINT;

    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    unsafe {
        let handle = CreateFileW(
            wide.as_ptr(),
            0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
            std::ptr::null_mut(),
        );
        if handle == INVALID_HANDLE_VALUE {
            return None;
        }
        let mut buffer = [0_u8; 16];
        let mut returned = 0_u32;
        let ok = DeviceIoControl(
            handle,
            FSCTL_GET_REPARSE_POINT,
            std::ptr::null_mut(),
            0,
            buffer.as_mut_ptr().cast(),
            buffer.len() as u32,
            &raw mut returned,
            std::ptr::null_mut(),
        );
        CloseHandle(handle);
        if ok == 0 || returned < 4 {
            return None;
        }
        Some(u32::from_ne_bytes([buffer[0], buffer[1], buffer[2], buffer[3]]))
    }
}
