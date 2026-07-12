//! Disk-budget scanning and eviction for staged v2 artifact generations.

use super::{open_store_lock, pointer_path, remove_staged_tree, staged_key_supported, staged_root};
use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::Path;
use std::time::SystemTime;

pub(crate) struct StagedDiskArtifact {
    pub(crate) key: String,
    pub(crate) total_size: u64,
    pub(crate) mtime: SystemTime,
}

fn staged_tree_stats(path: &Path) -> io::Result<(u64, SystemTime)> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() {
        return Ok((0, SystemTime::UNIX_EPOCH));
    }
    let mut size = metadata.len();
    let mut mtime = metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH);
    if metadata.is_dir() {
        for entry in fs::read_dir(path)?.flatten() {
            let (child_size, child_mtime) = staged_tree_stats(&entry.path())?;
            size = size.saturating_add(child_size);
            mtime = mtime.max(child_mtime);
        }
    }
    Ok((size, mtime))
}

pub(crate) fn scan_staged_disk_artifacts(
    artifact_dir: &Path,
) -> io::Result<Vec<StagedDiskArtifact>> {
    let root = staged_root(artifact_dir);
    if !root.is_dir() {
        return Ok(Vec::new());
    }
    let store_lock = open_store_lock(&root)?;
    fs2::FileExt::lock_shared(&store_lock)?;
    let mut artifacts = Vec::new();
    for entry in fs::read_dir(&root)?.flatten() {
        let key_root = entry.path();
        if !entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            continue;
        }
        let key = entry.file_name().to_string_lossy().into_owned();
        if !staged_key_supported(&key) {
            continue;
        }
        let (mut total_size, mut mtime) = staged_tree_stats(&key_root)?;
        let pointer = pointer_path(artifact_dir, &key);
        if let Ok(metadata) = fs::symlink_metadata(&pointer) {
            total_size = total_size.saturating_add(metadata.len());
            mtime = mtime.max(metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH));
        }
        artifacts.push(StagedDiskArtifact {
            key,
            total_size,
            mtime,
        });
    }
    Ok(artifacts)
}

pub(crate) fn evict_staged_artifact_keys(
    artifact_dir: &Path,
    keys: &HashSet<String>,
) -> io::Result<u64> {
    if keys.is_empty() {
        return Ok(0);
    }
    let root = staged_root(artifact_dir);
    if !root.is_dir() {
        return Ok(0);
    }
    let store_lock = open_store_lock(&root)?;
    fs2::FileExt::lock_exclusive(&store_lock)?;
    let mut bytes_removed: u64 = 0;
    for key in keys.iter().filter(|key| staged_key_supported(key)) {
        bytes_removed = bytes_removed.saturating_add(remove_staged_tree(&root.join(key))?);
        bytes_removed =
            bytes_removed.saturating_add(remove_staged_tree(&pointer_path(artifact_dir, key))?);
    }
    Ok(bytes_removed)
}
