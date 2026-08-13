//! Neutral volume identity and stateless capability facts.

use std::path::Path;

use crate::platform_imp;

/// Identity of the volume hosting a path, opaque and comparable. Two paths
/// on the same volume share an identity; distinct volumes differ.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct VolumeIdentity(pub(crate) platform_imp::fs::volume::RawVolumeIdentity);

/// The identity of the volume hosting `path`.
pub fn volume_identity(path: &Path) -> std::io::Result<VolumeIdentity> {
    platform_imp::fs::volume::volume_identity(path).map(VolumeIdentity)
}

/// The host's file-id width in bits (e.g. 128 on Windows NTFS/ReFS,
/// 64 on Unix dev+ino pairs). Used by callers to size identity hashes.
pub fn file_id_width() -> u32 {
    platform_imp::fs::volume::file_id_width()
}

/// The host's native hard-link limit (e.g. 1023 on NTFS, 65535 elsewhere).
pub fn hard_link_limit() -> u64 {
    platform_imp::fs::volume::hard_link_limit()
}

/// The volume identity of `path` as a plain `u128` (volume serial on
/// Windows, `st_dev` elsewhere), for callers that key on the raw value.
/// `None` when the path cannot be statted.
pub fn volume_identity_u128(path: &Path) -> Option<u128> {
    platform_imp::fs::volume::volume_identity_u128(path)
}

/// The disk space `metadata`-described file actually occupies (compressed
/// size on Windows, `blocks * 512` elsewhere), falling back to the logical
/// length when the host cannot report it.
pub fn allocated_bytes(path: &Path, metadata: &std::fs::Metadata) -> u64 {
    platform_imp::fs::volume::allocated_bytes(path, metadata)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn paths_on_one_volume_share_an_identity() {
        let dir = tempfile::tempdir().expect("tempdir");
        let a = dir.path().join("a");
        let b = dir.path().join("b");
        fs::write(&a, b"x").expect("write a");
        fs::write(&b, b"y").expect("write b");
        assert_eq!(
            volume_identity(&a).expect("identity a"),
            volume_identity(&b).expect("identity b")
        );
    }

    #[test]
    fn capability_facts_are_nonzero() {
        assert!(file_id_width() >= 64);
        assert!(hard_link_limit() >= 1);
    }
}
