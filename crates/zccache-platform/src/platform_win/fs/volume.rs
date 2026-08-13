//! Windows volume identity: the volume serial from the file ID.

use std::path::Path;

use super::identity::file_identity;

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
