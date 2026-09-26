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

/// Whether a fresh file in `dir` is reported `Exclusive`, i.e. whether this
/// volume can prove that removing an `nlink == 1` file frees its space.
fn volume_proves_exclusive(dir: &Path) -> bool {
    let probe = dir.join("exclusive-probe.bin");
    std::fs::write(&probe, b"probe").unwrap();
    let exclusive = matches!(
        kernal_api::platform::fs::extent_sharing(&probe),
        Ok(kernal_api::platform::fs::ExtentSharing::Exclusive)
    );
    std::fs::remove_file(&probe).unwrap();
    exclusive
}

fn age_file(path: &Path, age: Duration) {
    let stamp = SystemTime::now() - age;
    let file = std::fs::File::options().write(true).open(path).unwrap();
    file.set_modified(stamp).unwrap();
}

/// 1. 1,000 fresh nlink==1 artifacts + 50 hard-linked into a simulated
///    `target/`: a Routine sweep keeps everything (#1673: fresh entries
///    may be in use again); a Pressure sweep removes all 1,000, the 50 and
///    their target links survive, and `bytes_reclaimed` is exactly the
///    1,000's bytes.
#[test]
fn removes_fresh_unlinked_artifacts_eagerly_only_under_pressure_and_spares_linked_ones() {
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

    let routine = sweep_retired_version_store(
        &store,
        MAX_AGE,
        SystemTime::now(),
        RetiredSweepMode::Routine,
    );
    assert_eq!(
        routine.files_removed, 0,
        "fresh files survive a routine sweep"
    );
    for i in 0..1000 {
        assert!(store.join(format!("unlinked-{i}.bin")).exists());
    }

    let report = sweep_retired_version_store(
        &store,
        MAX_AGE,
        SystemTime::now(),
        RetiredSweepMode::Pressure,
    );

    assert_eq!(report.stores_scanned, 1);
    assert_eq!(report.files_removed, 1000);
    // Bytes are credited only where the volume proves the blocks exclusive
    // (#1673); a volume reporting unknown sharing credits nothing.
    let expected = if volume_proves_exclusive(tmp.path()) {
        1000 * "unlinked-artifact".len() as u64
    } else {
        0
    };
    assert_eq!(report.bytes_reclaimed, expected);
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

    let report = sweep_retired_version_store(
        &store,
        MAX_AGE,
        SystemTime::now(),
        RetiredSweepMode::Routine,
    );

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

    let report = sweep_retired_version_store(
        &store,
        MAX_AGE,
        SystemTime::now(),
        RetiredSweepMode::Routine,
    );
    assert_eq!(report.stores_live, 1);
    assert_eq!(report.files_removed, 0);
    assert!(victim.exists());

    drop(held);

    let report = sweep_retired_version_store(
        &store,
        MAX_AGE,
        SystemTime::now(),
        RetiredSweepMode::Pressure,
    );
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

    let report = sweep_retired_version_store(
        &store,
        MAX_AGE,
        SystemTime::now(),
        RetiredSweepMode::Pressure,
    );

    assert!(!cached.exists(), "aged linked artifact must still expire");
    assert!(target.join("linked.bin").exists());
    assert!(
        !store.join("index.bin").exists(),
        "index.bin is nlink==1 and has no age gate under pressure"
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

    let report = sweep_retired_version_stores_in(
        top,
        "v1.1.0",
        MAX_AGE,
        SystemTime::now(),
        RetiredSweepMode::Pressure,
    );

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

    let report = sweep_retired_version_store(
        &store,
        MAX_AGE,
        SystemTime::now(),
        RetiredSweepMode::Routine,
    );

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

    let report = sweep_retired_version_stores_in(
        top,
        "2.0.0",
        MAX_AGE,
        SystemTime::now(),
        RetiredSweepMode::Pressure,
    );

    assert_eq!(report.stores_scanned, 1);
    assert!(top.join("v2.0.0/artifact.bin").exists());
    assert!(!top.join("v1.0.0").exists());
}

#[test]
fn sweep_in_is_noop_on_missing_top_level() {
    let tmp = tempfile::tempdir().unwrap();
    let missing = tmp.path().join("does-not-exist");
    let report = sweep_retired_version_stores_in(
        &missing,
        "v1.0.0",
        MAX_AGE,
        SystemTime::now(),
        RetiredSweepMode::Routine,
    );
    assert_eq!(report, RetiredStoreSweepReport::default());
}

#[test]
fn refuses_a_non_directory_store() {
    let tmp = tempfile::tempdir().unwrap();
    let not_a_dir = tmp.path().join("v1.0.0");
    write_file(&not_a_dir, b"not a directory");
    let report = sweep_retired_version_store(
        &not_a_dir,
        MAX_AGE,
        SystemTime::now(),
        RetiredSweepMode::Routine,
    );
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

    let report = sweep_retired_version_store(
        &store,
        MAX_AGE,
        SystemTime::now(),
        RetiredSweepMode::Routine,
    );
    assert_eq!(report.failed, 1);
    assert!(real.join("artifact.bin").exists());
}

#[test]
fn merge_sums_every_field() {
    let mut a = RetiredStoreSweepReport {
        stores_scanned: 1,
        stores_removed: 1,
        stores_live: 0,
        stores_recently_active: 1,
        files_removed: 10,
        bytes_reclaimed: 100,
        failed: 2,
    };
    let b = RetiredStoreSweepReport {
        stores_scanned: 2,
        stores_removed: 0,
        stores_live: 1,
        stores_recently_active: 2,
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
            stores_recently_active: 3,
            files_removed: 15,
            bytes_reclaimed: 150,
            failed: 3,
        }
    );
}

/// A fresh nlink==1 file survives a Routine sweep (#1673) and is only
/// removed eagerly under Pressure.
#[test]
fn eager_removal_ignores_freshness_only_under_pressure() {
    let tmp = tempfile::tempdir().unwrap();
    let store = tmp.path().join("v1.0.0");
    std::fs::create_dir_all(&store).unwrap();
    let fresh_unlinked = store.join("fresh.bin");
    write_file(&fresh_unlinked, b"payload");
    // Explicitly re-stamp "now" so the test does not depend on the
    // filesystem's write-then-read mtime granularity.
    age_file(&fresh_unlinked, HOUR);

    let report = sweep_retired_version_store(
        &store,
        MAX_AGE,
        SystemTime::now(),
        RetiredSweepMode::Routine,
    );
    assert_eq!(report.files_removed, 0);
    assert!(fresh_unlinked.exists());

    let report = sweep_retired_version_store(
        &store,
        MAX_AGE,
        SystemTime::now(),
        RetiredSweepMode::Pressure,
    );
    assert_eq!(report.files_removed, 1);
    assert!(!fresh_unlinked.exists());
}

/// (a) #1673: A -> B -> A within the grace window. The store carries a
/// fresh `.last-active`, so even an aged nlink==1 file is left alone.
#[test]
fn a_to_b_to_a_within_grace_keeps_store_intact() {
    let tmp = tempfile::tempdir().unwrap();
    let store = tmp.path().join("v1.0.0");
    std::fs::create_dir_all(&store).unwrap();
    touch_store_activity_marker(&store).unwrap();
    let aged = store.join("aged.bin");
    write_file(&aged, b"payload");
    age_file(&aged, 10 * DAY);

    let report = sweep_retired_version_store(
        &store,
        MAX_AGE,
        SystemTime::now(),
        RetiredSweepMode::Routine,
    );

    assert_eq!(report.stores_recently_active, 1);
    assert_eq!(report.files_removed, 0);
    assert_eq!(report.stores_removed, 0);
    assert!(aged.exists());
    assert!(store.join(LAST_ACTIVE_MARKER_FILE).exists());
}

/// (b) #1673: an older daemon never sweeps a newer version's store.
#[test]
fn newer_store_is_never_swept_by_older_daemon() {
    let tmp = tempfile::tempdir().unwrap();
    let top = tmp.path();
    let file = top.join("v2.0.0/file.bin");
    write_file(&file, b"newer");
    age_file(&file, 10 * DAY);

    for mode in [RetiredSweepMode::Routine, RetiredSweepMode::Pressure] {
        let report =
            sweep_retired_version_stores_in(top, "v1.0.0", MAX_AGE, SystemTime::now(), mode);
        assert_eq!(report.stores_scanned, 0, "{mode:?}");
        assert!(file.exists(), "{mode:?}");
    }
}

/// (c) #1673: a recently used nlink==1 entry with no marker survives a
/// Routine sweep.
#[test]
fn recently_used_unlinked_entry_survives_routine_sweep() {
    let tmp = tempfile::tempdir().unwrap();
    let store = tmp.path().join("v1.0.0");
    let fresh = store.join("entry.bin");
    write_file(&fresh, b"payload");

    let report = sweep_retired_version_store(
        &store,
        MAX_AGE,
        SystemTime::now(),
        RetiredSweepMode::Routine,
    );

    assert_eq!(report.files_removed, 0);
    assert_eq!(report.stores_removed, 0);
    assert!(fresh.exists());
}

/// (d) An aged marker plus an aged file: the file is reclaimed and the
/// store, now holding only the marker, is removed.
#[test]
fn aged_store_without_marker_is_reclaimed() {
    let tmp = tempfile::tempdir().unwrap();
    let store = tmp.path().join("v1.0.0");
    let aged = store.join("aged.bin");
    write_file(&aged, b"payload");
    age_file(&aged, 10 * DAY);
    touch_store_activity_marker(&store).unwrap();
    age_file(&store.join(LAST_ACTIVE_MARKER_FILE), 10 * DAY);

    let report = sweep_retired_version_store(
        &store,
        MAX_AGE,
        SystemTime::now(),
        RetiredSweepMode::Routine,
    );

    assert_eq!(report.stores_recently_active, 0);
    assert_eq!(report.files_removed, 1);
    assert_eq!(report.stores_removed, 1);
    assert!(!store.exists());
}

/// (e) `is_older_version_dir` compares numerically and fails closed.
#[test]
fn is_older_version_dir_compares_numerically() {
    assert!(is_older_version_dir("v1.9.0", "v1.10.0"));
    assert!(is_older_version_dir("v1.9.0", "1.10.0"));
    assert!(!is_older_version_dir("v1.10.0", "v1.9.0"));
    assert!(!is_older_version_dir("v1.2.3", "v1.2.3"));
    assert!(!is_older_version_dir("v1.2.3", "1.2.3"));
    assert!(!is_older_version_dir("v2.0.0", "v1.0.0"));
    assert!(!is_older_version_dir("vprivate", "v1.0.0"));
    assert!(!is_older_version_dir("v1.0.0", "garbage"));
    assert!(!is_older_version_dir("v1.0", "v2.0.0"));
    assert!(!is_older_version_dir("v1.0.0.0", "v2.0.0"));
}

/// #1673: a cache file restored into `target/` by reflink has `nlink == 1`
/// but shares its blocks with the restored copy, so removing it frees
/// nothing. The sweep removes it and credits no bytes. Runs its assertion
/// only where the temp volume can reflink (btrfs, XFS, APFS, ReFS).
#[test]
fn issue_1673_reflinked_entry_credits_no_reclaimed_bytes() {
    let tmp = tempfile::tempdir().unwrap();
    let store = tmp.path().join("v1.0.0");
    let cached = store.join("artifacts/a.bin");
    write_file(&cached, &[7_u8; 256 * 1024]);
    let restored = tmp.path().join("target/a.bin");
    std::fs::create_dir_all(restored.parent().unwrap()).unwrap();
    if kernal_api::platform::fs::reflink_file(&cached, &restored).is_err() {
        return;
    }
    age_file(&cached, 30 * DAY);

    let report = sweep_retired_version_store(
        &store,
        MAX_AGE,
        SystemTime::now(),
        RetiredSweepMode::Routine,
    );
    assert_eq!(report.files_removed, 1);
    assert_eq!(report.bytes_reclaimed, 0);
    assert_eq!(std::fs::read(&restored).unwrap().len(), 256 * 1024);
}

/// #1673/#1659: the pressure estimate counts an `nlink == 1` file on every
/// volume (unknown sharing still counts, or an unqueryable volume would
/// never see retired bytes), but excludes one proven to share blocks.
#[test]
fn issue_1673_pressure_estimate_counts_unknown_but_not_proven_shared() {
    let tmp = tempfile::tempdir().unwrap();
    let cached = tmp.path().join("store/a.bin");
    write_file(&cached, &[5_u8; 256 * 1024]);
    assert!(file_may_free_space_on_removal(&cached));

    let restored = tmp.path().join("target/a.bin");
    std::fs::create_dir_all(restored.parent().unwrap()).unwrap();
    if kernal_api::platform::fs::reflink_file(&cached, &restored).is_ok()
        && kernal_api::platform::fs::extent_sharing(&cached)
            .is_ok_and(|s| s == kernal_api::platform::fs::ExtentSharing::Shared)
    {
        assert!(!file_may_free_space_on_removal(&cached));
        assert!(!file_frees_space_on_removal(&cached));
    }
}

/// #1687: the single rule both eviction planners use. `estimate` is the
/// pressure/planning view (unknown sharing may free space); the strict view
/// credits only provably exclusive blocks. Proven sharing (a reflink clone in
/// `target/`) never counts, and the sharing probe runs only for the last link.
#[test]
fn issue_1687_removal_frees_space_truth_table() {
    use kernal_api::platform::fs::ExtentSharing::{self, Exclusive, Shared, Unknown};
    // `None` sharing = the probe itself failed (no FIEMAP, network share).
    let rows: [(&str, Option<u64>, Option<ExtentSharing>, bool, bool); 7] = [
        ("last link, exclusive", Some(1), Some(Exclusive), true, true),
        (
            "last link, reflink-shared",
            Some(1),
            Some(Shared),
            false,
            false,
        ),
        (
            "last link, unknown sharing",
            Some(1),
            Some(Unknown),
            true,
            false,
        ),
        ("last link, probe failed", Some(1), None, true, false),
        (
            "hard-linked elsewhere",
            Some(2),
            Some(Exclusive),
            false,
            false,
        ),
        ("many links, shared", Some(40), Some(Shared), false, false),
        ("unknown link count", None, Some(Exclusive), false, false),
    ];
    for (name, links, sharing, estimate, strict) in rows {
        let probe = || sharing.ok_or_else(|| std::io::Error::other("probe failed"));
        assert_eq!(
            removal_frees_space(links, probe, true),
            estimate,
            "{name}: estimate"
        );
        assert_eq!(
            removal_frees_space(links, probe, false),
            strict,
            "{name}: strict"
        );
    }
    let mut probed = false;
    let multi_linked = removal_frees_space(
        Some(2),
        || {
            probed = true;
            Ok(Exclusive)
        },
        true,
    );
    assert!(!multi_linked);
    assert!(
        !probed,
        "the sharing probe must not run for a multi-linked file"
    );
}
