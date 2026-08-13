//! Linux file identity: (st_dev, st_ino).

use std::os::unix::fs::MetadataExt;
use std::path::Path;

/// Linux file identity — the (device, inode) pair.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct RawFileIdentity {
    pub(crate) dev: u64,
    pub(crate) ino: u64,
}

/// Linux has no journal-guaranteed change marker; callers treat files as
/// possibly changed.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct RawChangeMarker;

pub fn file_identity(path: &Path) -> std::io::Result<RawFileIdentity> {
    let meta = std::fs::metadata(path)?;
    Ok(RawFileIdentity {
        dev: meta.dev(),
        ino: meta.ino(),
    })
}

pub fn same_file(a: &Path, b: &Path) -> std::io::Result<bool> {
    Ok(file_identity(a)? == file_identity(b)?)
}

pub fn change_marker(_path: &Path) -> Option<RawChangeMarker> {
    None
}
