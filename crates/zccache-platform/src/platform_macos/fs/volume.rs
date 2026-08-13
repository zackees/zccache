//! macOS volume identity: the device number.

use std::os::unix::fs::MetadataExt;
use std::path::Path;

/// macOS volume identity — the st_dev device number.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct RawVolumeIdentity(pub(crate) u64);

pub fn volume_identity(path: &Path) -> std::io::Result<RawVolumeIdentity> {
    Ok(RawVolumeIdentity(std::fs::metadata(path)?.dev()))
}

/// macOS file identity is the (dev, ino) pair — 128 bits of identity,
/// but the width callers use to size identity hashes is 64 bits of inode.
pub fn file_id_width() -> u32 {
    64
}

/// The conventional Unix hard-link ceiling (fs_caps.rs's
/// UNIX_HARDLINK_LIMIT).
pub fn hard_link_limit() -> u64 {
    65_000
}
