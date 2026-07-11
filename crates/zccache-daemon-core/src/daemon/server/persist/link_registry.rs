//! In-memory ledger for hardlink-tier materializations (#1039).

use super::*;
use std::collections::BTreeSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;

#[derive(Clone, Debug)]
struct LinkRecord {
    blob_path: PathBuf,
    expected_hash: [u8; 32],
    outputs: BTreeSet<PathBuf>,
    suspect: bool,
}

static REGISTRY: OnceLock<dashmap::DashMap<FileId, LinkRecord>> = OnceLock::new();
static OUTPUT_IDS: OnceLock<dashmap::DashMap<PathBuf, FileId>> = OnceLock::new();
static WATCHER_AVAILABLE: AtomicBool = AtomicBool::new(true);

fn registry() -> &'static dashmap::DashMap<FileId, LinkRecord> {
    REGISTRY.get_or_init(dashmap::DashMap::new)
}

fn output_ids() -> &'static dashmap::DashMap<PathBuf, FileId> {
    OUTPUT_IDS.get_or_init(dashmap::DashMap::new)
}

fn hash_file(path: &Path) -> std::io::Result<[u8; 32]> {
    use std::io::Read;

    let mut file = std::fs::File::open(path)?;
    let mut hasher = blake3::Hasher::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(*hasher.finalize().as_bytes())
}

fn digest_path(blob_path: &Path) -> PathBuf {
    let name = blob_path.file_name().unwrap_or_default().to_string_lossy();
    let sidecar_name = format!(".cowhash-{}", blake3::hash(name.as_bytes()).to_hex());
    blob_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(sidecar_name)
}

pub(in crate::daemon::server) fn write_authoritative_blob_digest(
    blob_path: &Path,
) -> std::io::Result<()> {
    std::fs::write(digest_path(blob_path), hash_file(blob_path)?)
}

fn read_authoritative_blob_digest(blob_path: &Path) -> std::io::Result<Option<[u8; 32]>> {
    match std::fs::read(digest_path(blob_path)) {
        Ok(bytes) if bytes.len() == 32 => {
            let mut digest = [0_u8; 32];
            digest.copy_from_slice(&bytes);
            Ok(Some(digest))
        }
        Ok(_) => Ok(None),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

pub(in crate::daemon::server) fn register_hardlink(
    blob_path: &Path,
    output_path: &Path,
) -> std::io::Result<()> {
    let id = prepare_hardlink_registration(blob_path, output_path)?;
    commit_hardlink_registration(id, output_path)
}

/// Establish the authoritative digest before a shared output name is
/// published. Watcher events can then resolve the inode to this record even
/// during the narrow hardlink-creation/commit window.
pub(in crate::daemon::server) fn prepare_hardlink_registration(
    blob_path: &Path,
    output_path: &Path,
) -> std::io::Result<FileId> {
    let id = get_file_id(blob_path).ok_or_else(|| {
        std::io::Error::other(format!(
            "unable to identify cache blob {}",
            blob_path.display()
        ))
    })?;
    match registry().entry(id) {
        dashmap::mapref::entry::Entry::Occupied(mut entry) => {
            if entry.get().blob_path != blob_path {
                // File identifiers may be reused after a prior blob is deleted.
                // A path mismatch means this is a new identity generation, not
                // another output for the stale registry record.
                for stale in &entry.get().outputs {
                    output_ids().remove(stale);
                }
                let expected_hash = hash_file(blob_path)?;
                entry.insert(LinkRecord {
                    blob_path: blob_path.to_path_buf(),
                    expected_hash,
                    outputs: BTreeSet::new(),
                    suspect: false,
                });
            }
        }
        dashmap::mapref::entry::Entry::Vacant(entry) => {
            let expected_hash = hash_file(blob_path)?;
            entry.insert(LinkRecord {
                blob_path: blob_path.to_path_buf(),
                expected_hash,
                outputs: BTreeSet::new(),
                suspect: false,
            });
        }
    }
    output_ids().insert(output_path.to_path_buf(), id);
    Ok(id)
}

pub(in crate::daemon::server) fn cancel_hardlink_registration(id: FileId, output_path: &Path) {
    if output_ids()
        .get(output_path)
        .is_some_and(|entry| *entry == id)
    {
        output_ids().remove(output_path);
    }
}

pub(in crate::daemon::server) fn commit_hardlink_registration(
    id: FileId,
    output_path: &Path,
) -> std::io::Result<()> {
    let Some(mut record) = registry().get_mut(&id) else {
        cancel_hardlink_registration(id, output_path);
        return Err(std::io::Error::other(
            "hardlink registration disappeared before commit",
        ));
    };
    if get_file_id(output_path) != Some(id) {
        record.suspect = true;
        drop(record);
        cancel_hardlink_registration(id, output_path);
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "hardlink output disappeared before registration commit",
        ));
    }
    record.outputs.insert(output_path.to_path_buf());
    drop(record);
    Ok(())
}

pub(in crate::daemon::server) fn prepare_registered_detach(
    path: &Path,
) -> Option<(FileId, PathBuf)> {
    let id = output_ids()
        .get(path)
        .map(|entry| *entry)
        .or_else(|| get_file_id(path))?;
    registry()
        .get(&id)
        .map(|record| (id, record.blob_path.clone()))
}

pub(in crate::daemon::server) fn commit_registered_detach(id: FileId, path: &Path) {
    output_ids().remove(path);
    let remove = if let Some(mut record) = registry().get_mut(&id) {
        record.outputs.remove(path);
        record.outputs.is_empty() && !record.suspect
    } else {
        false
    };
    if remove {
        registry().remove(&id);
    }
}

pub(in crate::daemon::server) fn verify_registered_blob(blob_path: &Path) -> std::io::Result<()> {
    let started = std::time::Instant::now();
    let Some(id) = get_file_id(blob_path) else {
        return Ok(());
    };
    // A prior blob at this same (volume, inode) identity may have been
    // deleted and the identity reused by an unrelated file — ephemeral
    // test fixtures (loop-mounted / disk-image filesystems) reliably
    // reissue low inode numbers after unmount/remount. Trusting a
    // mismatched record would either skip real verification or reject a
    // perfectly valid blob against someone else's expected hash.
    // `prepare_hardlink_registration` already guards this same hazard on
    // the write path; mirror it here on the read/verify path.
    if registry()
        .get(&id)
        .is_some_and(|record| record.blob_path != blob_path)
    {
        registry().remove(&id);
    }
    let Some(record) = registry().get(&id) else {
        // Registry state is process-local, so restart verification must use a
        // digest persisted when the immutable blob was stored. Link count is
        // not evidence: a poisoned alias may have been deleted before restart.
        if let Some(expected_hash) = read_authoritative_blob_digest(blob_path)? {
            if hash_file(blob_path)? == expected_hash {
                registry().insert(
                    id,
                    LinkRecord {
                        blob_path: blob_path.to_path_buf(),
                        expected_hash,
                        outputs: BTreeSet::new(),
                        suspect: false,
                    },
                );
                return Ok(());
            }
        }
        let link_count = hard_link_count(blob_path).unwrap_or_default();
        tracing::warn!(
            event = "cow_unregistered_blob_evicted",
            blob_path = %blob_path.display(),
            link_count,
            "unregistered cache blob failed durable verification; evicting"
        );
        crate::core::lifecycle::write_event(
            "cow_unregistered_blob_evicted",
            serde_json::json!({
                "blob_path": blob_path,
                "link_count": link_count,
            }),
        );
        remove_registered_blob(blob_path)?;
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "unregistered cache blob failed durable verification",
        ));
    };
    if !record.suspect
        && (WATCHER_AVAILABLE.load(Ordering::Acquire)
            || hard_link_count(blob_path).unwrap_or_default() <= 1)
    {
        return Ok(());
    }
    let actual = hash_file(blob_path)?;
    if actual == record.expected_hash {
        let remove = record.outputs.is_empty();
        drop(record);
        if remove {
            registry().remove(&id);
        } else if let Some(mut record) = registry().get_mut(&id) {
            record.suspect = false;
        }
        return Ok(());
    }
    let elapsed_ns = started.elapsed().as_nanos() as u64;
    let link_count = hard_link_count(blob_path).unwrap_or_default();
    let outputs = record.outputs.iter().cloned().collect::<Vec<_>>();
    let cache_key = record
        .blob_path
        .file_name()
        .map_or_else(String::new, |name| name.to_string_lossy().into_owned());
    tracing::warn!(
        event = "cow_blob_corruption_detected",
        cache_key,
        blob_path = %record.blob_path.display(),
        expected_hash = %blake3::Hash::from_bytes(record.expected_hash),
        actual_hash = %blake3::Hash::from_bytes(actual),
        link_count,
        outputs = ?outputs,
        elapsed_ns,
        "suspect hardlinked cache blob failed verification; refusing to serve it"
    );
    crate::core::lifecycle::write_event(
        "cow_blob_corruption_detected",
        serde_json::json!({
            "blob_path": record.blob_path,
            "cache_key": cache_key,
            "expected_hash": blake3::Hash::from_bytes(record.expected_hash).to_hex().to_string(),
            "actual_hash": blake3::Hash::from_bytes(actual).to_hex().to_string(),
            "link_count": link_count,
            "registered_outputs": outputs,
            "elapsed_ns": elapsed_ns,
        }),
    );
    drop(record);
    remove_registered_blob(blob_path)?;
    Err(std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        format!(
            "cache blob {} was modified through a hardlink",
            blob_path.display()
        ),
    ))
}

pub(in crate::daemon::server) fn set_registry_watcher_available(available: bool) {
    WATCHER_AVAILABLE.store(available, Ordering::Release);
}

pub(in crate::daemon::server) fn mark_registered_links_suspect<'a>(
    paths: impl IntoIterator<Item = &'a Path>,
) {
    for path in paths {
        let Some(id) = get_file_id(path) else {
            continue;
        };
        if let Some(mut record) = registry().get_mut(&id) {
            record.suspect = true;
        }
    }
}

/// A remove event may arrive after the directory entry is already gone, so
/// native file identity is no longer queryable. The reverse index preserves
/// enough identity to mark the blob suspect before forgetting the output.
pub(in crate::daemon::server) fn mark_removed_links_suspect<'a>(
    paths: impl IntoIterator<Item = &'a Path>,
) {
    for path in paths {
        let Some((_, id)) = output_ids().remove(path) else {
            continue;
        };
        if let Some(mut record) = registry().get_mut(&id) {
            record.suspect = true;
            record.outputs.remove(path);
        }
    }
}

pub(in crate::daemon::server) fn mark_all_registered_links_suspect() {
    for mut record in registry().iter_mut() {
        record.suspect = true;
    }
}

pub(in crate::daemon::server) fn registered_blob_id(blob_path: &Path) -> Option<FileId> {
    let id = get_file_id(blob_path)?;
    registry().contains_key(&id).then_some(id)
}

pub(in crate::daemon::server) fn unregister_blob_id(id: FileId) {
    if let Some((_, record)) = registry().remove(&id) {
        for output in record.outputs {
            output_ids().remove(&output);
        }
    }
}

pub(in crate::daemon::server) fn remove_registered_blob(blob_path: &Path) -> std::io::Result<()> {
    let registration = registered_blob_id(blob_path);
    let was_readonly = std::fs::metadata(blob_path)
        .map(|metadata| metadata.permissions().readonly())
        .unwrap_or(false);
    let restore_readonly = was_readonly || readonly_enabled();
    make_writable(blob_path)?;
    if let Err(error) = remove_output_file(blob_path) {
        if restore_readonly {
            let _ = set_readonly(blob_path, true);
        }
        return Err(error);
    }
    if let Some(id) = registration {
        unregister_blob_id(id);
    }
    let _ = std::fs::remove_file(digest_path(blob_path));
    Ok(())
}

#[cfg(test)]
pub(in crate::daemon::server) fn registered_output_count(blob_path: &Path) -> usize {
    get_file_id(blob_path)
        .and_then(|id| registry().get(&id).map(|record| record.outputs.len()))
        .unwrap_or(0)
}

#[cfg(test)]
pub(in crate::daemon::server) fn is_file_id_registered(id: FileId) -> bool {
    registry().contains_key(&id)
}

#[cfg(test)]
pub(in crate::daemon::server) fn forget_blob_registration_for_restart_test(blob_path: &Path) {
    let Some(id) = get_file_id(blob_path) else {
        return;
    };
    if let Some((_, record)) = registry().remove(&id) {
        for output in record.outputs {
            output_ids().remove(&output);
        }
    }
}
