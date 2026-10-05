//! Filesystem verification: the safety net behind a clean fingerprint check.
//!
//! The watcher is best-effort, so `check` re-stats every tracked file to
//! catch an edit whose event was never delivered (#1897). The pass is split
//! into two halves so neither ever needs a DashMap guard on the watch map:
//!
//! * [`collect_verifications`] — the filesystem half. Stats (and hashes the
//!   layer-2 candidates) with **no** watch-map guard of any kind alive, so
//!   the whole window is off the shard (issue #1908, the same class as the
//!   `on_batch` fix for #724).
//! * [`apply_verifications`] — the in-memory half. Takes `&mut WatchState`
//!   and only touches a `HashMap`.
//!
//! **Lock discipline:** no function in this module may stat, `hash_file`,
//! or `canonicalize` while a `DashMap` read or write guard on
//! `FingerprintManager::watches` is alive.

use std::collections::HashMap;

use rayon::prelude::*;

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

/// Stat `path` once and record the signals the layer-1 predicate compares.
///
/// `None` when the file is missing or unreadable, which the caller treats as a
/// change. On Unix the identity lookup is a second `stat`, so this stays at two
/// syscalls per file — the same count as the separate `persist::mtime_ns` and
/// `persist::file_size` calls this replaced.
pub(super) fn observe(path: &std::path::Path) -> Option<FileObservation> {
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

/// One file's post-verification state, computed with NO watch-map guard held.
#[derive(Debug)]
pub(super) enum VerificationUpdate {
    /// Missing or unreadable: treat as changed rather than trusting the
    /// tracked state (#1897). Tracked state is left untouched.
    Missing { rel: String },
    /// A layer-2 re-hash failed (file vanished or read error): reported as
    /// changed, but the tracked `(mtime_ns, size, hash_hex)` must survive
    /// untouched so layer 1 retries the file on the next check instead of
    /// caching the failure as a clean skip.
    HashFailed { rel: String },
    /// Only the cheap stat signals moved; content re-hashed to the same hex.
    /// The tracked entry is refreshed with the new signals.
    SmartTouch {
        rel: String,
        observed: FileObservation,
    },
    /// Content genuinely differs from the tracked hash. The tracked entry
    /// records the new signals and hash.
    ContentChanged {
        rel: String,
        observed: FileObservation,
        hash_hex: String,
    },
}

impl VerificationUpdate {
    pub(super) fn rel(&self) -> &str {
        match self {
            Self::Missing { rel }
            | Self::HashFailed { rel }
            | Self::SmartTouch { rel, .. }
            | Self::ContentChanged { rel, .. } => rel,
        }
    }
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
        Some(id) => tracked.file_id == Some(id),
        None => true,
    }
}

/// Filesystem half of `check`'s verification pass.
///
/// `files` is a caller-owned snapshot of the watch's tracked map taken under a
/// short read guard. Runs with NO guard alive:
///
/// 1. Serial stat of every tracked file. A [`FileObservation`] for which
///    [`unchanged`] holds — all four cheap signals match — is a layer-1 fast
///    skip and emits nothing.
/// 2. The remaining candidates are blake3-hashed through rayon's global pool,
///    so a fully-touched tree (branch switch, codegen, `git checkout`) is
///    hashed concurrently — off the shard, which is the whole point (#1908).
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
pub(super) fn collect_verifications(
    root: &crate::core::NormalizedPath,
    files: &HashMap<String, TrackedFile>,
) -> Vec<VerificationUpdate> {
    #[cfg(test)]
    fire_collect_hook();

    // Pass 1 — serial stats, layer-1 fast skip.
    struct Candidate {
        rel: String,
        abs: crate::core::NormalizedPath,
        observed: FileObservation,
        tracked_hex: String,
    }
    let mut updates: Vec<VerificationUpdate> = Vec::new();
    let mut candidates: Vec<Candidate> = Vec::new();
    for (rel, tracked) in files {
        let abs = root.join(rel);
        let Some(observed) = observe(&abs) else {
            // Missing or unreadable: treat as changed rather than trusting
            // the tracked state (#1897). Tracked state is left untouched.
            updates.push(VerificationUpdate::Missing { rel: rel.clone() });
            continue;
        };
        if unchanged(tracked, &observed) {
            continue; // Layer 1: fast skip
        }
        candidates.push(Candidate {
            rel: rel.clone(),
            abs,
            observed,
            tracked_hex: tracked.hash_hex.clone(),
        });
    }

    // Pass 2 — parallel hashing of the layer-2 candidates, still guard-free.
    let hashed: Vec<VerificationUpdate> = candidates
        .into_par_iter()
        .map(|candidate| {
            let Candidate {
                rel,
                abs,
                observed,
                tracked_hex,
            } = candidate;
            match crate::hash::hash_file(&abs) {
                // A hash failure reports the file as changed but leaves the
                // tracked entry completely untouched (see `HashFailed`).
                Err(_) => VerificationUpdate::HashFailed { rel },
                Ok(h) => {
                    let hash_hex = h.to_hex();
                    if hash_hex == tracked_hex {
                        VerificationUpdate::SmartTouch { rel, observed }
                    } else {
                        VerificationUpdate::ContentChanged {
                            rel,
                            observed,
                            hash_hex,
                        }
                    }
                }
            }
        })
        .collect();
    updates.extend(hashed);
    updates
}

/// In-memory half of `check`'s verification pass.
///
/// `snapshot` is the map [`collect_verifications`] was given. An update is
/// dropped when the live entry no longer equals the snapshot entry it was
/// computed from — a concurrent `on_batch` already refreshed that file while
/// the collect phase ran guard-free, so its newer state must not be clobbered.
///
/// Returns the relative paths whose content genuinely changed.
pub(super) fn apply_verifications(
    watch: &mut WatchState,
    snapshot: &HashMap<String, TrackedFile>,
    updates: &[VerificationUpdate],
) -> Vec<String> {
    let mut changed = Vec::new();
    for update in updates {
        let Some(live) = watch.files.get_mut(update.rel()) else {
            continue;
        };
        let stale = match snapshot.get(update.rel()) {
            Some(snap) => {
                live.mtime_ns != snap.mtime_ns
                    || live.size != snap.size
                    || live.file_id != snap.file_id
                    || live.inode_change_ns != snap.inode_change_ns
                    || live.hash_hex != snap.hash_hex
            }
            // The file was newly tracked while we hashed; nothing to compare
            // against, so leave the newer entry alone.
            None => true,
        };
        if stale {
            continue;
        }
        match update {
            VerificationUpdate::Missing { .. } | VerificationUpdate::HashFailed { .. } => {
                changed.push(update.rel().to_string());
            }
            VerificationUpdate::SmartTouch { observed, .. } => {
                live.refresh(observed);
            }
            VerificationUpdate::ContentChanged {
                observed, hash_hex, ..
            } => {
                live.observe(observed, hash_hex.clone());
                changed.push(update.rel().to_string());
            }
        }
    }
    changed
}

// ── Test seam ───────────────────────────────────────────────────────────────
//
// The regression test for #1908 needs to pause *inside* the verification
// window deterministically so it can prove no watch-map guard is live while
// hashing. Test-only; never called from a production path.
//
// Firing is armed per-thread rather than by a bare `Once`: every test in this
// binary calls `check`, and a process-wide one-shot would be claimed by an
// unrelated test's collect pass before the regression test ever reaches it.
// The thread that calls [`arm_collect_hook`] is the only one that can consume
// the hook, and only once.

#[cfg(test)]
type CollectHook = Box<dyn Fn() + Send + Sync>;

#[cfg(test)]
static COLLECT_HOOK: std::sync::OnceLock<std::sync::Mutex<Option<CollectHook>>> =
    std::sync::OnceLock::new();

#[cfg(test)]
static COLLECT_HOOK_CLAIMED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

#[cfg(test)]
thread_local! {
    static COLLECT_HOOK_ARMED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[cfg(test)]
fn fire_collect_hook() {
    let armed = COLLECT_HOOK_ARMED.with(|a| a.get());
    if !armed {
        return;
    }
    if COLLECT_HOOK_CLAIMED.swap(true, std::sync::atomic::Ordering::SeqCst) {
        return;
    }
    // The hook runs while the mutex is held: it is installed once, before the
    // verification thread starts, and fired at most once per process (the
    // claimed flag above), so no other caller can be waiting on this lock.
    if let Some(hook) = COLLECT_HOOK.get() {
        if let Ok(guard) = hook.lock() {
            if let Some(f) = guard.as_ref() {
                f();
            }
        }
    }
}

/// Install the collect hook. Does not arm it — see [`arm_collect_hook`].
#[cfg(test)]
pub(super) fn set_collect_hook(f: impl Fn() + Send + Sync + 'static) {
    *COLLECT_HOOK
        .get_or_init(|| std::sync::Mutex::new(None))
        .lock()
        .unwrap() = Some(Box::new(f));
}

/// Let the calling thread's next (and only next) [`collect_verifications`]
/// invoke the installed hook. Call from inside the spawned thread so no other
/// test's collect pass can steal it.
#[cfg(test)]
pub(super) fn arm_collect_hook() {
    COLLECT_HOOK_ARMED.with(|a| a.set(true));
}
