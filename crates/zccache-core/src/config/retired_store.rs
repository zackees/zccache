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
//! This module is the daemon-callable sweep: it removes files a retired
//! store solely owns immediately, expires files still hard-linked into a
//! `target/` tree once they age out, and removes the store directory itself
//! once nothing remains. Liveness comes from the store's own
//! `.writer.lock` (the same file + lock primitive
//! `crates/zccache-daemon-core/src/daemon/server/state.rs`'s
//! `CacheRootWriterLock` uses) — never from directory or file freshness,
//! since any write to an unrelated file would otherwise pin the whole store
//! forever.
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
/// 3. Walk the tree without following symlinks/reparse points; any such
///    entry is unlinked itself (never traversed, its target never touched).
///    Directory mtimes are never consulted.
/// 4. Each regular file other than `.writer.lock`: `nlink == 1` is removed
///    eagerly with no age gate (nothing else can reference it). `nlink > 1`
///    or unknown is removed only once its own mtime is older than
///    `now - max_age`; bytes are only credited to `bytes_reclaimed` when
///    removal provably frees space (`nlink == 1`).
/// 5. Directories that end up empty are removed bottom-up. If nothing but
///    `.writer.lock` remains in the store root, the whole store directory is
///    removed (lock released first, since an open handle to it can block
///    deletion on Windows).
#[must_use]
pub fn sweep_retired_version_store(
    store: &Path,
    max_age: Duration,
    now: SystemTime,
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

    sweep_directory_contents(store, store, now, max_age, &mut report);

    let only_lock_remains = std::fs::read_dir(store)
        .map(|entries| {
            entries
                .flatten()
                .all(|entry| entry.file_name() == WRITER_LOCK_FILE_NAME)
        })
        .unwrap_or(false);

    // Release before removal: an open handle to `.writer.lock` can block
    // deleting the file (and so the directory) on Windows. On Unix this is a
    // no-op beyond dropping the fd.
    drop(lock_guard);

    if only_lock_remains {
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

/// Sweep every `v<X.Y.Z>` child of `top_level` except `keep`.
///
/// Non-version-shaped siblings (`logs`, `vprivate`, ...) and `keep` itself
/// (the running version; `"1.2.3"` and `"v1.2.3"` are both accepted) are
/// never inspected. Missing `top_level` is a
/// silent no-op, matching [`super::resolve::prune_stale_version_dirs_in`].
#[must_use]
pub fn sweep_retired_version_stores_in(
    top_level: &Path,
    keep: &str,
    max_age: Duration,
    now: SystemTime,
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
        let keep_name = if keep.starts_with('v') {
            keep.to_owned()
        } else {
            format!("v{keep}")
        };
        if name == keep_name || !super::resolve::is_version_dir_name(&name) {
            continue;
        }
        report.merge(&sweep_retired_version_store(&entry.path(), max_age, now));
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

/// Recursively sweep `dir`'s entries. `store_root` is passed through so the
/// `.writer.lock` exemption applies only at the store's top level (nested
/// directories cannot contain the store's own lock file, but comparing
/// explicitly keeps the exemption from ever applying by name collision
/// alone).
fn sweep_directory_contents(
    dir: &Path,
    store_root: &Path,
    now: SystemTime,
    max_age: Duration,
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
        if dir == store_root && entry.file_name() == WRITER_LOCK_FILE_NAME {
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
            sweep_directory_contents(&path, store_root, now, max_age, report);
            if is_dir_empty(&path) {
                let _ = std::fs::remove_dir(&path);
            }
            continue;
        }
        sweep_regular_file(&path, now, max_age, report);
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
/// `report`. See [`sweep_retired_version_store`]'s step 4 for the contract.
fn sweep_regular_file(
    path: &Path,
    now: SystemTime,
    max_age: Duration,
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
    let should_delete = match link_count {
        Some(1) => true,
        Some(_) | None => is_older_than(&metadata, now, max_age),
    };
    if !should_delete {
        return;
    }
    match std::fs::remove_file(path) {
        Ok(()) => {
            report.files_removed += 1;
            // Only credit bytes when removal provably frees space: the
            // last link (nlink == 1). Shared or unknown counts free nothing
            // we can prove, so they are never counted.
            let frees_space = link_count == Some(1);
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
mod tests {
    use super::*;
    use std::time::{Duration, SystemTime};

    const HOUR: Duration = Duration::from_secs(3600);
    const DAY: Duration = Duration::from_secs(24 * 3600);
    /// 72h, matches the daemon default.
    const MAX_AGE: Duration = Duration::from_secs(3 * 24 * 3600);

    fn write_file(path: &Path, contents: &[u8]) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }

    fn age_file(path: &Path, age: Duration) {
        let stamp = SystemTime::now() - age;
        let file = std::fs::File::options().write(true).open(path).unwrap();
        file.set_modified(stamp).unwrap();
    }

    /// 1. 1,000 fresh nlink==1 artifacts + 50 hard-linked into a simulated
    ///    `target/`: one sweep removes all 1,000; the 50 and their target
    ///    links survive, and `bytes_reclaimed` is exactly the 1,000's bytes.
    #[test]
    fn removes_fresh_unlinked_artifacts_eagerly_and_spares_linked_ones() {
        let tmp = tempfile::tempdir().unwrap();
        let store = tmp.path().join("v1.0.0");
        let target = tmp.path().join("target");
        std::fs::create_dir_all(&store).unwrap();
        std::fs::create_dir_all(&target).unwrap();

        for i in 0..1000 {
            write_file(
                &store.join(format!("unlinked-{i}.bin")),
                b"unlinked-artifact",
            );
        }
        for i in 0..50 {
            let cached = store.join(format!("linked-{i}.bin"));
            write_file(&cached, b"linked-artifact-bytes");
            std::fs::hard_link(&cached, target.join(format!("linked-{i}.bin"))).unwrap();
        }

        let report = sweep_retired_version_store(&store, MAX_AGE, SystemTime::now());

        assert_eq!(report.stores_scanned, 1);
        assert_eq!(report.files_removed, 1000);
        assert_eq!(
            report.bytes_reclaimed,
            1000 * "unlinked-artifact".len() as u64
        );
        assert_eq!(report.stores_removed, 0, "50 linked artifacts remain");
        assert_eq!(report.stores_live, 0);
        assert_eq!(report.failed, 0);

        for i in 0..1000 {
            assert!(!store.join(format!("unlinked-{i}.bin")).exists());
        }
        for i in 0..50 {
            assert!(store.join(format!("linked-{i}.bin")).exists());
            assert!(target.join(format!("linked-{i}.bin")).exists());
        }
    }

    /// 2. The same store, but the 50 linked artifacts have aged past
    ///    `max_age`: they expire per file (their target links stay intact,
    ///    and their bytes are excluded from `bytes_reclaimed`), and the
    ///    store directory is then removed since nothing remains.
    #[test]
    fn expires_aged_linked_artifacts_then_removes_the_empty_store() {
        let tmp = tempfile::tempdir().unwrap();
        let store = tmp.path().join("v1.0.0");
        let target = tmp.path().join("target");
        std::fs::create_dir_all(&store).unwrap();
        std::fs::create_dir_all(&target).unwrap();

        let mut linked_bytes = 0u64;
        for i in 0..50 {
            let cached = store.join(format!("linked-{i}.bin"));
            let contents = b"linked-artifact-bytes";
            write_file(&cached, contents);
            std::fs::hard_link(&cached, target.join(format!("linked-{i}.bin"))).unwrap();
            age_file(&cached, 10 * DAY);
            linked_bytes += contents.len() as u64;
        }

        let report = sweep_retired_version_store(&store, MAX_AGE, SystemTime::now());

        assert_eq!(report.files_removed, 50);
        assert_eq!(
            report.bytes_reclaimed, 0,
            "each linked file still has a surviving target/ link, so deleting it frees nothing"
        );
        assert_eq!(report.stores_removed, 1);
        assert_eq!(report.failed, 0);
        assert!(!store.exists());
        for i in 0..50 {
            assert!(
                target.join(format!("linked-{i}.bin")).exists(),
                "target/ link must survive"
            );
        }
        let _ = linked_bytes;
    }

    /// 3. A `.writer.lock` held by a live process makes the store
    ///    `stores_live`, untouched; after the holder drops it, the next
    ///    sweep reclaims it.
    #[test]
    fn a_held_writer_lock_protects_the_whole_store_until_released() {
        let tmp = tempfile::tempdir().unwrap();
        let store = tmp.path().join("v1.0.0");
        std::fs::create_dir_all(&store).unwrap();
        let victim = store.join("unlinked.bin");
        write_file(&victim, b"unlinked-artifact");

        let lock_file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(store.join(WRITER_LOCK_FILE_NAME))
            .unwrap();
        let held = kernal_api::platform::fs::try_lock_exclusive_owned(lock_file).unwrap();

        let report = sweep_retired_version_store(&store, MAX_AGE, SystemTime::now());
        assert_eq!(report.stores_live, 1);
        assert_eq!(report.files_removed, 0);
        assert!(victim.exists());

        drop(held);

        let report = sweep_retired_version_store(&store, MAX_AGE, SystemTime::now());
        assert_eq!(report.stores_live, 0);
        assert_eq!(report.files_removed, 1);
        assert!(!victim.exists());
    }

    /// 4. A fresh write into the store (directory touch, a fresh
    ///    `index.bin`, a present-but-unheld `.writer.lock`) does not protect
    ///    any *other* file: an aged linked artifact is still removed on its
    ///    own mtime.
    #[test]
    fn a_fresh_sibling_write_does_not_protect_an_unrelated_aged_file() {
        let tmp = tempfile::tempdir().unwrap();
        let store = tmp.path().join("v1.0.0");
        let target = tmp.path().join("target");
        std::fs::create_dir_all(&store).unwrap();
        std::fs::create_dir_all(&target).unwrap();

        // A live-looking, but unheld, `.writer.lock` plus a freshly written
        // `index.bin` right beside the aged file.
        write_file(&store.join(WRITER_LOCK_FILE_NAME), b"12345\n");
        write_file(&store.join("index.bin"), b"fresh-index-bytes");

        let cached = store.join("linked.bin");
        write_file(&cached, b"linked-artifact-bytes");
        std::fs::hard_link(&cached, target.join("linked.bin")).unwrap();
        age_file(&cached, 10 * DAY);

        let report = sweep_retired_version_store(&store, MAX_AGE, SystemTime::now());

        assert!(!cached.exists(), "aged linked artifact must still expire");
        assert!(target.join("linked.bin").exists());
        assert!(
            !store.join("index.bin").exists(),
            "index.bin is nlink==1 and has no age gate"
        );
        assert_eq!(report.files_removed, 2); // index.bin + linked.bin
        assert_eq!(report.stores_removed, 1, "only .writer.lock remains");
    }

    /// 5. `sweep_retired_version_stores_in` never touches `keep` or a
    ///    non-version-shaped sibling.
    #[test]
    fn sweep_in_skips_keep_and_non_version_siblings() {
        let tmp = tempfile::tempdir().unwrap();
        let top = tmp.path();
        for name in ["v1.0.0", "v1.1.0", "logs", "vprivate"] {
            std::fs::create_dir_all(top.join(name)).unwrap();
            write_file(&top.join(name).join("artifact.bin"), b"artifact");
        }

        let report = sweep_retired_version_stores_in(top, "v1.1.0", MAX_AGE, SystemTime::now());

        assert_eq!(
            report.stores_scanned, 1,
            "only v1.0.0 is a retired version dir"
        );
        assert!(
            top.join("v1.1.0/artifact.bin").exists(),
            "kept version untouched"
        );
        assert!(
            top.join("logs/artifact.bin").exists(),
            "non-version dir untouched"
        );
        assert!(
            top.join("vprivate/artifact.bin").exists(),
            "non-version dir untouched"
        );
        assert!(
            !top.join("v1.0.0").exists(),
            "retired sibling fully reclaimed"
        );
    }

    /// 6. Symlinks inside a retired store are never followed: the link entries
    ///    themselves are unlinked, their targets outside the store (a file and
    ///    a populated directory) survive untouched, and the store is removed.
    #[cfg(unix)]
    #[test]
    fn a_symlink_inside_the_store_is_unlinked_but_never_followed() {
        let tmp = tempfile::tempdir().unwrap();
        let store = tmp.path().join("v1.0.0");
        std::fs::create_dir_all(store.join("nested")).unwrap();
        let outside_file = tmp.path().join("outside-target.bin");
        write_file(&outside_file, b"outside-bytes");
        let outside_dir = tmp.path().join("outside-dir");
        write_file(&outside_dir.join("precious.bin"), b"precious");

        let file_link = store.join("escape-link.bin");
        let dir_link = store.join("nested").join("escape-dir");
        std::os::unix::fs::symlink(&outside_file, &file_link).unwrap();
        std::os::unix::fs::symlink(&outside_dir, &dir_link).unwrap();

        let report = sweep_retired_version_store(&store, MAX_AGE, SystemTime::now());

        assert_eq!(report.failed, 0);
        assert_eq!(
            report.bytes_reclaimed, 0,
            "unlinking a symlink frees no target bytes"
        );
        assert!(outside_file.exists(), "symlink file target must survive");
        assert!(
            outside_dir.join("precious.bin").exists(),
            "symlinked directory contents must never be swept"
        );
        assert!(std::fs::symlink_metadata(&file_link).is_err());
        assert!(std::fs::symlink_metadata(&dir_link).is_err());
        assert_eq!(report.stores_removed, 1);
        assert!(!store.exists());
    }

    /// 7. `file_link_count` returns `Some(1)` for a new file and `Some(2)`
    ///    after a hard link.
    #[test]
    fn file_link_count_reflects_real_hard_links() {
        let tmp = tempfile::tempdir().unwrap();
        let original = tmp.path().join("a.bin");
        write_file(&original, b"payload");
        assert_eq!(file_link_count(&original), Some(1));

        let linked = tmp.path().join("b.bin");
        std::fs::hard_link(&original, &linked).unwrap();
        assert_eq!(file_link_count(&original), Some(2));
        assert_eq!(file_link_count(&linked), Some(2));
    }

    #[test]
    fn sweep_in_accepts_a_bare_current_version() {
        let tmp = tempfile::tempdir().unwrap();
        let top = tmp.path();
        write_file(&top.join("v2.0.0/artifact.bin"), b"current");
        write_file(&top.join("v1.0.0/artifact.bin"), b"retired");

        let report = sweep_retired_version_stores_in(top, "2.0.0", MAX_AGE, SystemTime::now());

        assert_eq!(report.stores_scanned, 1);
        assert!(top.join("v2.0.0/artifact.bin").exists());
        assert!(!top.join("v1.0.0").exists());
    }

    #[test]
    fn sweep_in_is_noop_on_missing_top_level() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("does-not-exist");
        let report =
            sweep_retired_version_stores_in(&missing, "v1.0.0", MAX_AGE, SystemTime::now());
        assert_eq!(report, RetiredStoreSweepReport::default());
    }

    #[test]
    fn refuses_a_non_directory_store() {
        let tmp = tempfile::tempdir().unwrap();
        let not_a_dir = tmp.path().join("v1.0.0");
        write_file(&not_a_dir, b"not a directory");
        let report = sweep_retired_version_store(&not_a_dir, MAX_AGE, SystemTime::now());
        assert_eq!(report.failed, 1);
        assert!(not_a_dir.exists());
    }

    #[test]
    fn refuses_a_symlinked_store() {
        let tmp = tempfile::tempdir().unwrap();
        let real = tmp.path().join("real-store");
        std::fs::create_dir_all(&real).unwrap();
        write_file(&real.join("artifact.bin"), b"payload");
        let store = tmp.path().join("v1.0.0");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&real, &store).unwrap();
        #[cfg(windows)]
        std::os::windows::fs::symlink_dir(&real, &store).unwrap();

        let report = sweep_retired_version_store(&store, MAX_AGE, SystemTime::now());
        assert_eq!(report.failed, 1);
        assert!(real.join("artifact.bin").exists());
    }

    #[test]
    fn merge_sums_every_field() {
        let mut a = RetiredStoreSweepReport {
            stores_scanned: 1,
            stores_removed: 1,
            stores_live: 0,
            files_removed: 10,
            bytes_reclaimed: 100,
            failed: 2,
        };
        let b = RetiredStoreSweepReport {
            stores_scanned: 2,
            stores_removed: 0,
            stores_live: 1,
            files_removed: 5,
            bytes_reclaimed: 50,
            failed: 1,
        };
        a.merge(&b);
        assert_eq!(
            a,
            RetiredStoreSweepReport {
                stores_scanned: 3,
                stores_removed: 1,
                stores_live: 1,
                files_removed: 15,
                bytes_reclaimed: 150,
                failed: 3,
            }
        );
    }

    #[test]
    fn eager_removal_ignores_freshness() {
        let tmp = tempfile::tempdir().unwrap();
        let store = tmp.path().join("v1.0.0");
        std::fs::create_dir_all(&store).unwrap();
        let fresh_unlinked = store.join("fresh.bin");
        write_file(&fresh_unlinked, b"payload");
        // Explicitly re-stamp "now" so the test does not depend on the
        // filesystem's write-then-read mtime granularity.
        age_file(&fresh_unlinked, HOUR);

        let report = sweep_retired_version_store(&store, MAX_AGE, SystemTime::now());
        assert_eq!(report.files_removed, 1);
        assert!(!fresh_unlinked.exists());
    }
}
