//! Windows volume identity: the volume serial from the file ID.

use std::os::windows::ffi::OsStrExt;
use std::path::Path;

use windows_sys::Win32::Foundation::{GetLastError, SetLastError};
use windows_sys::Win32::Storage::FileSystem::GetCompressedFileSizeW;

use super::identity::file_identity;
use super::verbatim_path;

/// Windows volume identity — the volume serial number.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct RawVolumeIdentity(pub(crate) u64);

pub fn volume_identity(path: &Path) -> std::io::Result<RawVolumeIdentity> {
    Ok(RawVolumeIdentity(file_identity(path)?.volume_serial))
}

/// NTFS/ReFS expose 128-bit file IDs; legacy callers use the 64-bit index.
pub fn file_id_width() -> u32 {
    128
}

/// NTFS hard-link ceiling (fs_caps.rs's WINDOWS_HARDLINK_LIMIT).
pub fn hard_link_limit() -> u64 {
    1023
}

pub fn volume_identity_u128(path: &Path) -> Option<u128> {
    volume_identity(path)
        .ok()
        .map(|RawVolumeIdentity(serial)| u128::from(serial))
}

/// The compressed size of `path` (`GetCompressedFileSizeW`), falling back
/// to the logical length when the volume does not track it.
pub fn allocated_bytes(path: &Path, metadata: &std::fs::Metadata) -> u64 {
    let verbatim = verbatim_path(path).unwrap_or_else(|_| path.to_path_buf());
    let wide: Vec<u16> = verbatim.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut high = 0_u32;
    unsafe {
        SetLastError(0);
        let low = GetCompressedFileSizeW(wide.as_ptr(), &mut high);
        windows_allocated_size_result(low, high, GetLastError(), metadata.len())
    }
}

/// Combines the high/low words `GetCompressedFileSizeW` reports, falling
/// back to `fallback` when the low word is `u32::MAX` (INVALID_FILE_SIZE)
/// and `error` is nonzero.
pub(crate) fn windows_allocated_size_result(low: u32, high: u32, error: u32, fallback: u64) -> u64 {
    if low == u32::MAX && error != 0 {
        fallback
    } else {
        (u64::from(high) << 32) | u64::from(low)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allocated_size_combines_high_and_low_words_and_falls_back() {
        assert_eq!(
            windows_allocated_size_result(7, 1, 0, 99),
            (1_u64 << 32) | 7
        );
        assert_eq!(windows_allocated_size_result(u32::MAX, 0, 5, 99), 99);
    }
}
