//! Disk-budget scanning and eviction for staged v2 artifact generations.

use super::{
    is_staged_link_or_reparse, open_store_lock, pointer_path, remove_registered_blob,
    remove_staged_link_or_reparse, staged_key_supported, staged_root,
    validate_staged_artifact_root, STORE_LOCK,
};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io;
use std::path::Path;
use std::time::SystemTime;

pub(crate) struct StagedDiskArtifact {
    pub(crate) key: String,
    #[cfg(test)]
    pub(crate) total_size: u64,
    pub(crate) mtime: SystemTime,
    /// Committed generation observed while holding the shared store lock.
    pub(crate) generation: Option<String>,
}

fn staged_tree_stats(path: &Path) -> io::Result<(u64, SystemTime)> {
    let metadata = fs::symlink_metadata(path)?;
    if is_staged_link_or_reparse(&metadata) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "refusing staged artifact scan through linked/reparse path: {}",
                path.display()
            ),
        ));
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
    if !validate_staged_artifact_root(artifact_dir)? {
        return Ok(Vec::new());
    }
    let store_lock = open_store_lock(&root)?;
    fs2::FileExt::lock_shared(&store_lock)?;
    scan_staged_disk_artifacts_locked(artifact_dir, root.as_path())
}

fn scan_staged_disk_artifacts_locked(
    artifact_dir: &Path,
    root: &Path,
) -> io::Result<Vec<StagedDiskArtifact>> {
    let mut artifacts = Vec::new();
    for entry in fs::read_dir(root)?.flatten() {
        let key_root = entry.path();
        let key_metadata = fs::symlink_metadata(&key_root)?;
        if is_staged_link_or_reparse(&key_metadata) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "refusing staged artifact scan through linked/reparse key root: {}",
                    key_root.display()
                ),
            ));
        }
        if !key_metadata.is_dir() {
            continue;
        }
        let key = entry.file_name().to_string_lossy().into_owned();
        if !staged_key_supported(&key) {
            continue;
        }
        let (mut _total_size, mut mtime) = staged_tree_stats(&key_root)?;
        let pointer = pointer_path(artifact_dir, &key);
        let generation = if let Ok(metadata) = fs::symlink_metadata(&pointer) {
            if is_staged_link_or_reparse(&metadata) {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!(
                        "refusing staged artifact scan through linked/reparse pointer: {}",
                        pointer.display()
                    ),
                ));
            }
            _total_size = _total_size.saturating_add(metadata.len());
            mtime = mtime.max(metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH));
            fs::read_to_string(&pointer)
                .ok()
                .map(|value| value.trim().to_string())
        } else {
            None
        };
        artifacts.push(StagedDiskArtifact {
            key,
            #[cfg(test)]
            total_size: _total_size,
            mtime,
            generation,
        });
    }
    Ok(artifacts)
}

#[cfg(test)]
pub(crate) fn evict_staged_artifact_keys(
    artifact_dir: &Path,
    keys: &HashSet<String>,
) -> io::Result<u64> {
    if keys.is_empty() {
        return Ok(0);
    }
    let root = staged_root(artifact_dir);
    if !validate_staged_artifact_root(artifact_dir)? {
        return Ok(0);
    }
    let store_lock = open_store_lock(&root)?;
    fs2::FileExt::lock_exclusive(&store_lock)?;
    evict_staged_artifact_keys_locked(artifact_dir, root.as_path(), keys)
}

/// Delete only staged generations that still match the maintenance snapshot.
/// The exclusive lock makes compare-and-delete atomic with respect to staged
/// publication without blocking writers during legacy scans or planning.
pub(crate) fn evict_staged_artifact_keys_if_unchanged(
    artifact_dir: &Path,
    expected: &HashMap<String, Option<String>>,
) -> io::Result<HashSet<String>> {
    if expected.is_empty() {
        return Ok(HashSet::new());
    }
    let root = staged_root(artifact_dir);
    if !validate_staged_artifact_root(artifact_dir)? {
        return Ok(HashSet::new());
    }
    let store_lock = open_store_lock(&root)?;
    #[cfg(test)]
    super::hook::pause(
        artifact_dir,
        super::StagedHookPoint::MaintenanceStoreLockPending,
    );
    fs2::FileExt::lock_exclusive(&store_lock)?;
    let mut removed = HashSet::new();
    for (key, expected_generation) in expected.iter().filter(|(key, _)| staged_key_supported(key)) {
        let pointer = pointer_path(artifact_dir, key);
        let current = match fs::symlink_metadata(&pointer) {
            Ok(metadata) if is_staged_link_or_reparse(&metadata) => {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!(
                        "refusing staged artifact eviction through linked/reparse pointer: {}",
                        pointer.display()
                    ),
                ));
            }
            Ok(_) => fs::read_to_string(&pointer)
                .ok()
                .map(|value| value.trim().to_string()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => return Err(error),
        };
        if &current != expected_generation {
            continue;
        }
        remove_staged_tree(&root.join(key))?;
        remove_staged_tree(&pointer_path(artifact_dir, key))?;
        removed.insert(key.clone());
    }
    Ok(removed)
}

#[cfg(test)]
fn evict_staged_artifact_keys_locked(
    artifact_dir: &Path,
    root: &Path,
    keys: &HashSet<String>,
) -> io::Result<u64> {
    let mut bytes_removed: u64 = 0;
    for key in keys.iter().filter(|key| staged_key_supported(key)) {
        bytes_removed = bytes_removed.saturating_add(remove_staged_tree(&root.join(key))?);
        bytes_removed =
            bytes_removed.saturating_add(remove_staged_tree(&pointer_path(artifact_dir, key))?);
    }
    Ok(bytes_removed)
}

pub(super) fn remove_staged_tree(path: &Path) -> io::Result<u64> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(error),
    };
    if is_staged_link_or_reparse(&metadata) {
        remove_staged_link_or_reparse(path, &metadata)?;
        return Ok(0);
    }
    if metadata.is_dir() {
        let mut removed = 0;
        for entry in fs::read_dir(path)?.flatten() {
            removed += remove_staged_tree(&entry.path())?;
        }
        fs::remove_dir(path)?;
        return Ok(removed);
    }
    let size = metadata.len();
    remove_registered_blob(path)?;
    Ok(size)
}

pub(in crate::daemon::server) fn clear_staged_artifacts(artifact_dir: &Path) -> io::Result<u64> {
    let root = staged_root(artifact_dir);
    if !validate_staged_artifact_root(artifact_dir)? {
        return Ok(0);
    }
    let store_lock = open_store_lock(&root)?;
    #[cfg(test)]
    super::hook::pause(
        artifact_dir,
        super::StagedHookPoint::MaintenanceStoreLockPending,
    );
    fs2::FileExt::lock_exclusive(&store_lock)?;
    let mut bytes_removed = 0;
    for entry in fs::read_dir(&root)?.flatten() {
        if entry.file_name() == STORE_LOCK {
            continue;
        }
        bytes_removed += remove_staged_tree(&entry.path())?;
    }
    Ok(bytes_removed)
}
