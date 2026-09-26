//! Retired `v<VERSION>` cache-store reclamation (issue #1659).
//!
//! A daemon upgrade leaves its old `v<VERSION>/` store behind as a sibling of
//! the current one under the top-level cache root (see [`super::resolve`]'s
//! module docs for the versioned-subdir layout). Nothing reclaimed those
//! stores except an explicit `zccache clear` (`prune_stale_version_dirs`),
//! so a host that never runs that command accumulates one full store per
//! upgrade forever — on a real host, 99.7% of one 499 GB retired store was
//! disk no build tree referenced any more.
//!
//! This module is the daemon-callable sweep. Only versions strictly *older*
//! than the running one are ever swept ([`is_older_version_dir`]): a newer
//! store belongs to a daemon this one cannot reason about, and switching
//! back and forth between versions (A -> B -> A) must not destroy either
//! cache (issue #1673).
//!
//! Liveness is layered:
//! - `.writer.lock` (the same file + lock primitive
//!   `crates/zccache-daemon-core/src/daemon/server/state.rs`'s
//!   `CacheRootWriterLock` uses): a store whose lock is held is untouched.
//! - `.last-active` ([`LAST_ACTIVE_MARKER_FILE`]): the owning daemon stamps
//!   it via [`touch_store_activity_marker`]. In [`RetiredSweepMode::Routine`]
//!   a marker younger than `max_age` skips the whole store
//!   (`stores_recently_active`). A missing marker means a legacy store that
//!   is not active. Only this dedicated marker counts — never directory or
//!   arbitrary file freshness, since any write to an unrelated file would
//!   otherwise pin the whole store forever.
//! - Per file: in `Routine` mode every file must be older than `max_age`
//!   regardless of link count. In [`RetiredSweepMode::Pressure`] mode
//!   (disk pressure), the marker gate is bypassed and `nlink == 1` files
//!   are removed eagerly; shared/unknown-count files still need the age gate.
//!
//! The store directory itself is removed once only `.writer.lock` /
//! `.last-active` remain.
//!
//! Link-count and no-follow-symlink queries go through
//! `kernal_api::platform::fs` (`hard_link_count`, `classify`) rather than a
//! hand-rolled `#[cfg(unix)]` / `#[cfg(windows)]` split — that facade already
//! implements the exact portable primitives this sweep needs on every
//! platform zccache ships (see its own doc comments for the per-platform
//! mechanics).

use std::fs::Metadata;
use std::io;
use std::path::Path;
use std::time::{Duration, SystemTime};

use kernal_api::platform::fs::LinkKind;

/// Name of the lock file a live daemon holds for as long as it writes to a
/// cache root. Matches
/// `zccache-daemon-core`'s `CACHE_ROOT_WRITER_LOCK_FILE`; kept as a literal
/// here rather than a shared constant because `zccache-core` has no
/// dependency on the daemon crate (dependency direction is the other way).
const WRITER_LOCK_FILE_NAME: &str = ".writer.lock";

/// Name of the activity marker the owning daemon stamps in its own store.
/// Its mtime is the store's "last used" time for the routine sweep gate.
pub const LAST_ACTIVE_MARKER_FILE: &str = ".last-active";

/// Create or truncate `<store>/.last-active` and set its mtime to now.
///
/// Only the daemon that owns `store` calls this.
///
/// # Errors
/// Returns any I/O error from opening the marker or setting its mtime.
pub fn touch_store_activity_marker(store: &Path) -> io::Result<()> {
    let file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(store.join(LAST_ACTIVE_MARKER_FILE))?;
    file.set_modified(SystemTime::now())
}

/// How aggressively a retired-store sweep may reclaim.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetiredSweepMode {
    /// Periodic maintenance: honour `.last-active` and age-gate every file.
    Routine,
    /// Disk pressure: skip the `.last-active` gate and remove `nlink == 1`
    /// files eagerly; shared/unknown files are still age-gated.
    Pressure,
}

fn parse_version_triple(name: &str) -> Option<(u64, u64, u64)> {
    let rest = name.strip_prefix('v').unwrap_or(name);
    let mut parts = rest.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some((major, minor, patch))
}

/// True only when `name` (`vX.Y.Z`) is a strictly older version than
/// `current` (`X.Y.Z` or `vX.Y.Z`). Unparseable input on either side is
/// `false` so nothing ambiguous is ever swept.
#[must_use]
pub fn is_older_version_dir(name: &str, current: &str) -> bool {
    if !name.starts_with('v') {
        return false;
    }
    match (parse_version_triple(name), parse_version_triple(current)) {
        (Some(n), Some(c)) => n < c,
        _ => false,
    }
}

/// True when removing `path` provably frees its space: it is the last hard
/// link (`nlink == 1`) and none of its blocks are shared. A reflink restore
/// into `target/` (Windows ReFS, btrfs, XFS, APFS) leaves the cache file at
/// `nlink == 1` while sharing every block, so the link count alone
/// over-counts (#1673). Unknown sharing never counts.
#[must_use]
pub fn file_frees_space_on_removal(path: &Path) -> bool {
    file_link_count(path) == Some(1) && blocks_are_exclusive(path)
}

/// True when removing `path` may free its space, for the disk-pressure
/// *estimate* (#1659): the last hard link whose blocks are not known to be
/// shared. Unlike [`file_frees_space_on_removal`], unknown sharing counts:
/// a volume kernal-api cannot query (network shares, Linux file systems
/// without `FIEMAP`) always reports it, and treating it as shared would zero
/// the estimate there and disable the retired-store pressure valve. Only
/// proven sharing (a btrfs/XFS/APFS/ReFS clone) is excluded.
#[must_use]
pub fn file_may_free_space_on_removal(path: &Path) -> bool {
    file_link_count(path) == Some(1)
        && !matches!(
            kernal_api::platform::fs::extent_sharing(path),
            Ok(kernal_api::platform::fs::ExtentSharing::Shared)
        )
}

fn blocks_are_exclusive(path: &Path) -> bool {
    matches!(
        kernal_api::platform::fs::extent_sharing(path),
        Ok(kernal_api::platform::fs::ExtentSharing::Exclusive)
    )
}

/// Outcome of a retired-store sweep — either [`sweep_retired_version_store`]
/// on one store, or the summed totals from
/// [`sweep_retired_version_stores_in`] across every retired sibling.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct RetiredStoreSweepReport {
    /// Retired stores this pass considered (excludes the kept/current one).
    pub stores_scanned: usize,
    /// Stores whose directory was removed entirely because nothing but
    /// `.writer.lock` remained in them.
    pub stores_removed: usize,
    /// Stores skipped because a live daemon of that version still holds
    /// `.writer.lock`.
    pub stores_live: usize,
    /// Stores skipped in `Routine` mode because their `.last-active` marker
    /// is younger than `max_age` (issue #1673).
    pub stores_recently_active: usize,
    /// Individual files removed, across every store this pass touched.
    pub files_removed: usize,
    /// Bytes freed: only files whose *last* hard link was removed count —
    /// deleting one of several links returns no disk space.
    pub bytes_reclaimed: u64,
    /// Entries a pass could not classify or remove (symlink/reparse
    /// encountered mid-walk, a lock/open/remove call that failed for a
    /// reason other than contention). Left in place; retried next pass.
    pub failed: usize,
}

impl RetiredStoreSweepReport {
    /// Fold `other`'s counters into `self`, field by field.
    pub fn merge(&mut self, other: &Self) {
        self.stores_scanned = self.stores_scanned.saturating_add(other.stores_scanned);
        self.stores_removed = self.stores_removed.saturating_add(other.stores_removed);
        self.stores_live = self.stores_live.saturating_add(other.stores_live);
        self.stores_recently_active = self
            .stores_recently_active
            .saturating_add(other.stores_recently_active);
        self.files_removed = self.files_removed.saturating_add(other.files_removed);
        self.bytes_reclaimed = self.bytes_reclaimed.saturating_add(other.bytes_reclaimed);
        self.failed = self.failed.saturating_add(other.failed);
    }
}

/// Portable hard-link count for `path`.
///
/// Thin wrapper over `kernal_api::platform::fs::hard_link_count`, which
/// already implements this natively for every host zccache ships (Linux
/// `st_nlink`, macOS `st_nlink`, Windows `nNumberOfLinks` via
/// `GetFileInformationByHandle`). `None` means the count is unknown: the path is missing, or the native
/// query failed. Callers must treat `None` as "possibly shared" for safety
/// decisions.
#[must_use]
pub fn file_link_count(path: &Path) -> Option<u64> {
    kernal_api::platform::fs::hard_link_count(path).ok()
}

/// Sweep one retired store directory.
///
/// See the module docs for the overall contract. In order:
/// 1. Refuse (bump `failed`, touch nothing) if `store` is a symlink/reparse
///    point or is not a directory.
/// 2. Claim `<store>/.writer.lock` (if present) with a non-blocking
///    exclusive lock, held for the whole pass. Another live holder makes
///    this store `stores_live` and untouched; a missing lock file means no
///    daemon has ever written here (or none survived), so the sweep
///    proceeds without one.
/// 3. `Routine` mode only: if `<store>/.last-active` exists and is younger
///    than `max_age`, the store is in recent use — count it in
///    `stores_recently_active` and touch nothing. A missing marker is a
///    legacy, inactive store. `Pressure` mode skips this gate.
/// 4. Walk the tree without following symlinks/reparse points; any such
///    entry is unlinked itself (never traversed, its target never touched).
///    Directory mtimes are never consulted.
/// 5. Each regular file other than the root `.writer.lock` / `.last-active`:
///    in `Routine` mode it is removed only once its own mtime is older than
///    `now - max_age`, whatever its link count. In `Pressure` mode
///    `nlink == 1` is removed eagerly; `nlink > 1` or unknown still needs
///    the age gate. Bytes are only credited to `bytes_reclaimed` when
///    removal provably frees space: `nlink == 1` and
///    `kernal_api::platform::fs::extent_sharing` reports `Exclusive` (a
///    reflinked or snapshot-shared file frees nothing).
/// 6. Directories that end up empty are removed bottom-up. If nothing but
///    `.writer.lock` / `.last-active` remains in the store root, the whole
///    store directory is removed (lock released first, since an open handle
///    to it can block deletion on Windows).
#[must_use]
pub fn sweep_retired_version_store(
    store: &Path,
    max_age: Duration,
    now: SystemTime,
    mode: RetiredSweepMode,
) -> RetiredStoreSweepReport {
    let mut report = RetiredStoreSweepReport {
        stores_scanned: 1,
        ..RetiredStoreSweepReport::default()
    };

    match kernal_api::platform::fs::classify(store) {
        Ok(LinkKind::Regular) => {}
        Ok(LinkKind::Symlink | LinkKind::Reparse) | Err(_) => {
            report.failed += 1;
            return report;
        }
    }
    match std::fs::metadata(store) {
        Ok(metadata) if metadata.is_dir() => {}
        _ => {
            report.failed += 1;
            return report;
        }
    }

    let lock_guard = match acquire_store_lock(store) {
        Ok(guard) => guard,
        Err(LockAcquireOutcome::HeldByLiveWriter) => {
            report.stores_live += 1;
            return report;
        }
        Err(LockAcquireOutcome::Failed) => {
            report.failed += 1;
            return report;
        }
    };

    if mode == RetiredSweepMode::Routine {
        let marker_recent = std::fs::metadata(store.join(LAST_ACTIVE_MARKER_FILE))
            .is_ok_and(|metadata| !is_older_than(&metadata, now, max_age));
        if marker_recent {
            report.stores_recently_active += 1;
            return report;
        }
    }

    sweep_directory_contents(store, store, now, max_age, mode, &mut report);

    let only_markers_remain = std::fs::read_dir(store)
        .map(|entries| {
            entries
                .flatten()
                .all(|entry| is_root_marker(&entry.file_name()))
        })
        .unwrap_or(false);

    // Release before removal: an open handle to `.writer.lock` can block
    // deleting the file (and so the directory) on Windows. On Unix this is a
    // no-op beyond dropping the fd.
    drop(lock_guard);

    if only_markers_remain {
        match std::fs::remove_dir_all(store) {
            Ok(()) => report.stores_removed += 1,
            // Best-effort, mirroring `prune_stale_version_dirs_in`: a
            // removal that fails here (most often a lingering Windows
            // handle) is retried on the next pass rather than treated as
            // fatal.
            Err(_) => report.failed += 1,
        }
    }

    report
}

/// Sweep every `v<X.Y.Z>` child of `top_level` strictly older than `keep`.
///
/// Non-version-shaped siblings (`logs`, `vprivate`, ...), `keep` itself
/// (the running version; `"1.2.3"` and `"v1.2.3"` are both accepted), and
/// every *newer* version are never inspected, in either mode. Missing `top_level` is a
/// silent no-op, matching [`super::resolve::prune_stale_version_dirs_in`].
#[must_use]
pub fn sweep_retired_version_stores_in(
    top_level: &Path,
    keep: &str,
    max_age: Duration,
    now: SystemTime,
    mode: RetiredSweepMode,
) -> RetiredStoreSweepReport {
    let mut report = RetiredStoreSweepReport::default();
    let entries = match std::fs::read_dir(top_level) {
        Ok(entries) => entries,
        Err(_) => return report,
    };
    for entry in entries.flatten() {
        if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        if !super::resolve::is_version_dir_name(&name) || !is_older_version_dir(&name, keep) {
            continue;
        }
        report.merge(&sweep_retired_version_store(
            &entry.path(),
            max_age,
            now,
            mode,
        ));
    }
    report
}

enum LockAcquireOutcome {
    HeldByLiveWriter,
    Failed,
}

/// Claim `<store>/.writer.lock` for the duration of one sweep pass.
///
/// `Ok(None)` means no lock file exists yet (no daemon has ever bound this
/// root, or a prior sweep already removed it) — the sweep proceeds without
/// holding anything. The lock primitive is the exact one
/// `CacheRootWriterLock` in `zccache-daemon-core` uses, so a live daemon's
/// claim on this store is observed the same way the daemon itself would
/// refuse a second writer.
fn acquire_store_lock(
    store: &Path,
) -> Result<Option<kernal_api::platform::fs::OwnedFileLock>, LockAcquireOutcome> {
    let lock_path = store.join(WRITER_LOCK_FILE_NAME);
    let file = match std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&lock_path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(LockAcquireOutcome::Failed),
    };
    match kernal_api::platform::fs::try_lock_exclusive_owned(file) {
        Ok(guard) => Ok(Some(guard)),
        Err(error) if kernal_api::platform::fs::is_lock_conflict(&error) => {
            Err(LockAcquireOutcome::HeldByLiveWriter)
        }
        Err(_) => Err(LockAcquireOutcome::Failed),
    }
}

fn is_root_marker(name: &std::ffi::OsStr) -> bool {
    name == WRITER_LOCK_FILE_NAME || name == LAST_ACTIVE_MARKER_FILE
}

/// Recursively sweep `dir`'s entries. `store_root` is passed through so the
/// `.writer.lock` / `.last-active` exemption applies only at the store's top level (nested
/// directories cannot contain the store's own lock file, but comparing
/// explicitly keeps the exemption from ever applying by name collision
/// alone).
fn sweep_directory_contents(
    dir: &Path,
    store_root: &Path,
    now: SystemTime,
    max_age: Duration,
    mode: RetiredSweepMode,
    report: &mut RetiredStoreSweepReport,
) {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(_) => {
            report.failed += 1;
            return;
        }
    };
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => {
                report.failed += 1;
                continue;
            }
        };
        if dir == store_root && is_root_marker(&entry.file_name()) {
            continue;
        }
        let path = entry.path();
        let kind = match kernal_api::platform::fs::classify(&path) {
            Ok(kind) => kind,
            Err(_) => {
                report.failed += 1;
                continue;
            }
        };
        if !matches!(kind, LinkKind::Regular) {
            // Symlink or reparse point: never traversed. Unlink the entry
            // itself (never its target) so it cannot pin the store.
            remove_link_entry(&path, report);
            continue;
        }
        let is_dir = match entry.file_type() {
            Ok(file_type) => file_type.is_dir(),
            Err(_) => {
                report.failed += 1;
                continue;
            }
        };
        if is_dir {
            sweep_directory_contents(&path, store_root, now, max_age, mode, report);
            if is_dir_empty(&path) {
                let _ = std::fs::remove_dir(&path);
            }
            continue;
        }
        sweep_regular_file(&path, now, max_age, mode, report);
    }
}

/// Remove a symlink or reparse-point entry itself. `remove_file` unlinks a
/// Unix symlink (file or directory) and a Windows file symlink; a Windows
/// directory symlink or junction needs `remove_dir`, which removes the link
/// and never the target's contents. No bytes are credited.
fn remove_link_entry(path: &Path, report: &mut RetiredStoreSweepReport) {
    if std::fs::remove_file(path).is_ok() || std::fs::remove_dir(path).is_ok() {
        report.files_removed += 1;
    } else {
        report.failed += 1;
    }
}

fn is_dir_empty(dir: &Path) -> bool {
    std::fs::read_dir(dir)
        .map(|mut entries| entries.next().is_none())
        .unwrap_or(false)
}

/// Apply the per-file eager/expire rule to one regular file and update
/// `report`. See [`sweep_retired_version_store`]'s step 5 for the contract.
fn sweep_regular_file(
    path: &Path,
    now: SystemTime,
    max_age: Duration,
    mode: RetiredSweepMode,
    report: &mut RetiredStoreSweepReport,
) {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_file() => metadata,
        Ok(_) => return,
        Err(_) => {
            report.failed += 1;
            return;
        }
    };
    let len = metadata.len();
    let link_count = file_link_count(path);
    // Observed before removal, while the file still exists.
    let frees_space = link_count == Some(1) && blocks_are_exclusive(path);
    let should_delete = match (mode, link_count) {
        (RetiredSweepMode::Pressure, Some(1)) => true,
        _ => is_older_than(&metadata, now, max_age),
    };
    if !should_delete {
        return;
    }
    match std::fs::remove_file(path) {
        Ok(()) => {
            report.files_removed += 1;
            // Only credit bytes when removal provably frees space: the
            // last link with no shared or unknown-sharing extents.
            if frees_space {
                report.bytes_reclaimed = report.bytes_reclaimed.saturating_add(len);
            }
        }
        Err(_) => report.failed += 1,
    }
}

fn is_older_than(metadata: &Metadata, now: SystemTime, max_age: Duration) -> bool {
    metadata
        .modified()
        .ok()
        .and_then(|modified| now.duration_since(modified).ok())
        .is_some_and(|age| age >= max_age)
}

#[cfg(test)]
#[path = "retired_store_tests.rs"]
mod tests;
