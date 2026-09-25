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
    let report = sweep_retired_version_stores_in(&missing, "v1.0.0", MAX_AGE, SystemTime::now());
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
