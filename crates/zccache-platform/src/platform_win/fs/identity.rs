//! Windows file identity: volume serial + native 128-bit file ID.
//! ReFS does not guarantee uniqueness for the legacy 64-bit index, so the
//! FileIdInfo path is preferred with a BY_HANDLE_FILE_INFORMATION fallback.

use std::os::windows::ffi::OsStrExt;
use std::path::Path;

use windows_sys::Win32::Foundation::CloseHandle;
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FileIdInfo, GetFileInformationByHandle, GetFileInformationByHandleEx,
    BY_HANDLE_FILE_INFORMATION, FILE_FLAG_BACKUP_SEMANTICS, FILE_ID_INFO, FILE_SHARE_DELETE,
    FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};

/// Windows file identity — the volume serial and 128-bit file ID.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct RawFileIdentity {
    pub(crate) volume_serial: u64,
    pub(crate) identifier: [u8; 16],
}

/// Windows USN journal change marker — the file's USN sequence.
///
/// `ChangeTime` is deliberately not a fallback: `SetFileTime` can restore
/// it along with mtime and hide an ABA mutation. Callers disable
/// publication when the filesystem has no USN.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct RawChangeMarker(pub(crate) i128);

pub fn file_identity(path: &Path) -> std::io::Result<RawFileIdentity> {
    get_file_id(path).ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("cannot obtain file identity for {}", path.display()),
        )
    })
}

pub fn same_file(a: &Path, b: &Path) -> std::io::Result<bool> {
    Ok(get_file_id(a)
        .zip(get_file_id(b))
        .map(|(ia, ib)| ia == ib)
        .unwrap_or(false))
}

pub fn change_marker(path: &Path) -> Option<RawChangeMarker> {
    use windows_sys::Win32::Foundation::{GENERIC_READ, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Ioctl::{
        FSCTL_READ_FILE_USN_DATA, READ_FILE_USN_DATA, USN_RECORD_V2, USN_RECORD_V3,
    };
    use windows_sys::Win32::System::IO::DeviceIoControl;

    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    unsafe {
        let handle = CreateFileW(
            wide.as_ptr(),
            GENERIC_READ,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS,
            std::ptr::null_mut(),
        );
        if handle == INVALID_HANDLE_VALUE {
            return None;
        }
        let query = READ_FILE_USN_DATA {
            MinMajorVersion: 2,
            MaxMajorVersion: 4,
        };
        let mut record = [0_u8; 512];
        let mut returned = 0_u32;
        let usn_ok = DeviceIoControl(
            handle,
            FSCTL_READ_FILE_USN_DATA,
            (&raw const query).cast(),
            std::mem::size_of::<READ_FILE_USN_DATA>() as u32,
            record.as_mut_ptr().cast(),
            record.len() as u32,
            &raw mut returned,
            std::ptr::null_mut(),
        );
        if usn_ok != 0 && returned >= 8 {
            let major = u16::from_ne_bytes([record[4], record[5]]);
            let usn = match major {
                2 if returned as usize >= std::mem::size_of::<USN_RECORD_V2>() => {
                    Some(std::ptr::read_unaligned(record.as_ptr().cast::<USN_RECORD_V2>()).Usn)
                }
                3 if returned as usize >= std::mem::size_of::<USN_RECORD_V3>() => {
                    Some(std::ptr::read_unaligned(record.as_ptr().cast::<USN_RECORD_V3>()).Usn)
                }
                _ => None,
            };
            if let Some(usn) = usn {
                let _ = CloseHandle(handle);
                return Some(RawChangeMarker(i128::from(usn)));
            }
        }
        let _ = CloseHandle(handle);
        None
    }
}

fn get_file_id(path: &Path) -> Option<RawFileIdentity> {
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
            return Some(RawFileIdentity {
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
        Some(RawFileIdentity {
            volume_serial: u64::from(legacy.dwVolumeSerialNumber),
            identifier,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_identity_preserves_the_native_128_bit_identifier() {
        let dir = tempfile::tempdir().expect("tempdir");
        let a = dir.path().join("a");
        let b = dir.path().join("b");
        std::fs::write(&a, b"data").expect("write");
        std::fs::hard_link(&a, &b).expect("link");
        let ia = file_identity(&a).expect("identity");
        let ib = file_identity(&b).expect("identity");
        assert_eq!(ia, ib);
        // The 128-bit identifier is preserved verbatim (not truncated to
        // the legacy 64-bit index) on filesystems that expose FileIdInfo.
        assert_ne!(ia.identifier, [0_u8; 16]);
    }
}
