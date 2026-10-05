//! Filesystem verification: the safety net behind a clean fingerprint check.
//!
//! The watcher is best-effort, so `verify_filesystem` re-stats every tracked
//! file to catch an edit whose event was never delivered (#1897).

use std::path::Path;

use super::{TrackedFile, WatchState};

/// Cheap stat signals for one tracked file, captured in a single pass.
///
/// mtime and size alone cannot distinguish "untouched" from "replaced with an
/// equally long file whose mtime was restored", so the observation also carries
/// the file identity and the Unix `ctime`.
#[derive(Debug, Clone, Default)]
pub(super) struct FileObservation {
    pub(super) mtime_ns: u64,
    pub(super) size: u64,
    pub(super) file_id: Option<crate::platform::fs::identity::FileIdentity>,
    /// Unix `ctime` in nanoseconds, or `0` where the platform exposes none.
    pub(super) inode_change_ns: u64,
}

/// Stat `path` once and record the signals `verify_filesystem` compares.
///
/// `None` when the file is missing or unreadable, which the caller treats as a
/// change. On Unix the identity lookup is a second `stat`, so this stays at two
/// syscalls per file — the same count as the separate `persist::mtime_ns` and
/// `persist::file_size` calls this replaced.
pub(super) fn observe(path: &Path) -> Option<FileObservation> {
    let metadata = std::fs::metadata(path).ok()?;
    let mtime_ns = metadata
        .modified()
        .ok()
        .map(|modified| {
            modified
                .duration_since(std::time::SystemTime::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos() as u64
        })
        .unwrap_or(0);
    Some(FileObservation {
        mtime_ns,
        size: metadata.len(),
        file_id: crate::platform::fs::identity::file_identity(path).ok(),
        inode_change_ns: crate::platform::fs::inode_change_ns(&metadata),
    })
}

/// Re-stat all tracked files and return relative paths where content changed.
///
/// Layer 1 skips a file only when *all four* cheap signals match: mtime, size,
/// file identity, and Unix `ctime`. mtime and size can both be preserved by a
/// build tool that restores them after writing, which is how a stale cache hit
/// ("skip") survived a real content change; identity catches a replacement of
/// the file object at the path, and `ctime` catches an in-place edit because
/// the kernel advances it on every write and `utimensat` cannot put it back
/// (#1897). Any difference falls through to layer 2, which re-hashes and
/// separates a real change from a smart touch.
///
/// The residual gap is the one only hashing can close: on Linux the kernel
/// derives both `mtime` and `ctime` from the same coarse clock, so an in-place
/// edit that the daemon never observes *and* that shares a clock tick with the
/// last recorded observation is still skipped. Closing that would mean hashing
/// every tracked file on every clean check, which is the cost this two-layer
/// split exists to avoid.
///
/// On a platform with neither signal — `inode_change_ns == 0` (no Unix `ctime`
/// in `std::fs::Metadata`) and no file identity — layer 1 degrades to the old
/// mtime-and-size comparison, so a mtime- and size-preserving edit is only
/// caught when the watcher happens to deliver its event.
pub(super) fn verify_filesystem(watch: &mut WatchState) -> Vec<String> {
    let mut changed = Vec::new();
    let root = watch.root.clone();
    for (rel_path, tracked) in watch.files.iter_mut() {
        let abs = root.join(rel_path);
        let Some(observed) = observe(&abs) else {
            // Missing or unreadable: treat as changed rather than trusting the
            // tracked state (#1897).
            changed.push(rel_path.clone());
            continue;
        };
        if unchanged(tracked, &observed) {
            continue; // Layer 1: fast skip
        }
        // Layer 2: a cheap signal moved — re-hash to confirm.
        let hash_hex = match crate::hash::hash_file(&abs) {
            Ok(h) => h.to_hex(),
            Err(_) => {
                changed.push(rel_path.clone());
                continue;
            }
        };
        if hash_hex != tracked.hash_hex {
            // Content genuinely changed.
            tracked.observe(&observed, hash_hex);
            changed.push(rel_path.clone());
        } else {
            // Smart touch — only the stat signals changed, content same.
            tracked.refresh(&observed);
        }
    }
    changed
}

/// Layer-1 predicate: every cheap signal the platform can give us matches.
///
/// An unavailable signal is treated as "carries no information" rather than as
/// a difference. `file_identity` can fail transiently — most often on Windows,
/// where it opens the file with all sharing modes — and a `None` compared
/// against a tracked `Some` would otherwise manufacture a mismatch, send the
/// file to layer 2, and store the `None` back, pinning that file to a re-hash
/// on every subsequent check until an identity lookup happens to succeed. It
/// can only cost a missed detection, never a false "run", because layer 2 is
/// still the arbiter of whether content actually changed.
fn unchanged(tracked: &TrackedFile, observed: &FileObservation) -> bool {
    if observed.mtime_ns != tracked.mtime_ns || observed.size != tracked.size {
        return false;
    }
    if observed.inode_change_ns != tracked.inode_change_ns {
        return false;
    }
    match observed.file_id {
        Some(ref id) => id == &tracked.file_id,
        None => true,
    }
}
