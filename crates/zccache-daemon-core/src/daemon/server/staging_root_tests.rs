//! `StagingRoot` and startup-sweep tests (split from `state.rs`).

use super::*;

use std::time::Duration;

/// Backdate a directory so the age gate sees it as debris without the
/// test having to sleep.
fn backdate(path: &Path, by: Duration) {
    let when = std::time::SystemTime::now() - by;
    kernal_api::platform::fs::set_file_mtime(
        path,
        kernal_api::platform::fs::FileTime::from_system_time(when),
    )
    .unwrap();
}

#[test]
fn embedded_host_can_place_private_staging_outside_a_deep_cache_root() {
    let cache = tempfile::tempdir().unwrap();
    let staging = tempfile::tempdir().unwrap();

    let root = StagingRoot::new(cache.path(), Some(staging.path()), 7).unwrap();

    assert!(root.path().starts_with(staging.path()));
    assert!(root
        .path()
        .starts_with(staging.path().join(CONFIGURED_STAGING_CHILD)));
    assert!(!root.path().starts_with(cache.path()));
    assert!(root.path().join(STAGING_LOCK_FILE).is_file());
}

#[test]
fn configured_staging_cleanup_is_bounded_to_the_owned_child() {
    let cache = tempfile::tempdir().unwrap();
    let configured = tempfile::tempdir().unwrap();
    let unrelated = configured.path().join("unrelated-user-data");
    std::fs::create_dir_all(&unrelated).unwrap();
    std::fs::write(unrelated.join("keep.txt"), b"keep").unwrap();

    let debris = configured
        .path()
        .join(CONFIGURED_STAGING_CHILD)
        .join("abandoned");
    std::fs::create_dir_all(&debris).unwrap();
    std::fs::write(debris.join("orphan.o"), b"orphan").unwrap();
    backdate(&debris, Duration::from_secs(3600));

    let cleaner = StagingRoot::new(cache.path(), Some(configured.path()), 1).unwrap();
    assert_eq!(cleaner.cleanup_abandoned().unwrap(), 1);
    assert!(unrelated.join("keep.txt").is_file());
    assert!(!debris.exists());
}

#[test]
fn explicit_staging_roots_remain_independent_in_one_process() {
    let cache_a = tempfile::tempdir().unwrap();
    let cache_b = tempfile::tempdir().unwrap();
    let staging_a = tempfile::tempdir().unwrap();
    let staging_b = tempfile::tempdir().unwrap();

    let root_a = StagingRoot::new(cache_a.path(), Some(staging_a.path()), 1).unwrap();
    let root_b = StagingRoot::new(cache_b.path(), Some(staging_b.path()), 2).unwrap();

    assert!(root_a.path().starts_with(staging_a.path()));
    assert!(root_b.path().starts_with(staging_b.path()));
    assert!(!root_a.path().starts_with(staging_b.path()));
    assert!(!root_b.path().starts_with(staging_a.path()));
}

#[test]
fn abandoned_cleanup_preserves_live_roots_and_removes_crash_debris() {
    let temp = tempfile::tempdir().unwrap();
    let live_a = StagingRoot::new(temp.path(), None, 1).unwrap();
    let live_b = StagingRoot::new(temp.path(), None, 2).unwrap();
    std::fs::write(live_b.path().join("active.o"), b"active").unwrap();

    let abandoned = temp.path().join("staging").join("abandoned");
    std::fs::create_dir_all(&abandoned).unwrap();
    std::fs::write(abandoned.join("orphan.o"), b"orphan").unwrap();
    // Debris now has to look old. This assertion used to pass on a
    // freshly created directory, which is exactly what made the race in
    // soldr#1250 reachable.
    backdate(&abandoned, Duration::from_secs(3600));

    assert_eq!(live_a.cleanup_abandoned().unwrap(), 1);
    assert!(live_b.path().join("active.o").exists());
    assert!(!abandoned.exists());
}

// soldr#1250: the window in `StagingRoot::new` between `create_dir_all`
// and opening the lock.

#[test]
fn a_staging_root_being_born_survives_a_concurrent_cleaner() {
    let temp = tempfile::tempdir().unwrap();
    let cleaner = StagingRoot::new(temp.path(), None, 1).unwrap();

    // Exactly the on-disk state `StagingRoot::new` leaves behind after
    // `create_dir_all` and before it opens `.active.lock`.
    let being_born = temp.path().join("staging").join("999-0-12345");
    std::fs::create_dir_all(&being_born).unwrap();

    assert_eq!(
        cleaner.cleanup_abandoned().unwrap(),
        0,
        "a lockless directory that is seconds old is a root being born, not debris"
    );
    assert!(
        being_born.exists(),
        "deleting this is what makes the creating daemon fail ENOENT on its own lock"
    );
}

#[test]
fn the_cleaner_does_not_create_the_lock_file_it_tests_for() {
    let temp = tempfile::tempdir().unwrap();
    let cleaner = StagingRoot::new(temp.path(), None, 1).unwrap();
    let being_born = temp.path().join("staging").join("999-0-12345");
    std::fs::create_dir_all(&being_born).unwrap();

    let _ = cleaner.cleanup_abandoned().unwrap();

    // Survival first. Without this line the assertion below passes
    // vacuously under the original bug -- the directory is gone, so of
    // course its lock file is absent. Confirmed by re-introducing
    // `create(true)`: this test passed while the other two failed.
    assert!(being_born.exists(), "precondition: the root must survive");
    // The original bug: `create(true)` manufactured the lock file, so the
    // cleaner then found its own new file unlocked and judged the root
    // abandoned. Absence must stay absence.
    assert!(
        !being_born.join(STAGING_LOCK_FILE).exists(),
        "the cleaner must not fabricate the artifact whose absence protects the root"
    );
}

#[test]
fn lockless_debris_is_still_reclaimed_once_it_is_old_enough() {
    let temp = tempfile::tempdir().unwrap();
    let cleaner = StagingRoot::new(temp.path(), None, 1).unwrap();

    let debris = temp.path().join("staging").join("dead-0-1");
    std::fs::create_dir_all(&debris).unwrap();
    std::fs::write(debris.join("orphan.o"), b"orphan").unwrap();
    backdate(&debris, Duration::from_secs(3600));

    assert_eq!(cleaner.cleanup_abandoned().unwrap(), 1);
    assert!(
        !debris.exists(),
        "the age gate must not turn a leak into permanent debris"
    );
}

#[test]
fn the_age_gate_is_what_decides_the_lockless_case() {
    let temp = tempfile::tempdir().unwrap();
    let cleaner = StagingRoot::new(temp.path(), None, 1).unwrap();
    let young = temp.path().join("staging").join("young-0-1");
    std::fs::create_dir_all(&young).unwrap();

    // Same directory, same run: preserved under a real threshold, taken
    // when the threshold is zero. Nothing else distinguishes the two.
    assert_eq!(
        cleaner
            .cleanup_abandoned_older_than(Duration::from_secs(60))
            .unwrap(),
        0
    );
    assert!(young.exists());
    assert_eq!(
        cleaner
            .cleanup_abandoned_older_than(Duration::ZERO)
            .unwrap(),
        1
    );
    assert!(!young.exists());
}

#[test]
fn a_held_lock_still_protects_a_live_root_regardless_of_age() {
    let temp = tempfile::tempdir().unwrap();
    let cleaner = StagingRoot::new(temp.path(), None, 1).unwrap();
    let live = StagingRoot::new(temp.path(), None, 2).unwrap();
    std::fs::write(live.path().join("active.o"), b"active").unwrap();

    // Age must never override a held lock: a long-running daemon is old.
    backdate(live.path(), Duration::from_secs(3600));

    assert_eq!(cleaner.cleanup_abandoned().unwrap(), 0);
    assert!(live.path().join("active.o").exists());
}

/// Backdate every entry under `root`, root included.
fn backdate_tree(root: &Path, by: Duration) {
    for entry in std::fs::read_dir(root).unwrap().flatten() {
        let path = entry.path();
        if path.is_dir() {
            backdate_tree(&path, by);
        } else {
            backdate(&path, by);
        }
    }
    backdate(root, by);
}

/// #1796: a released lock proves only that the owning daemon is gone, not
/// that nothing still writes beneath it (an orphaned compile child outlived
/// its daemon and failed with `couldn't create a temp dir … (os error 2)`).
/// An unlocked root survives the sweep until its whole tree has been quiet.
#[test]
fn an_unlocked_root_with_recent_writes_survives_the_sweep() {
    let temp = tempfile::tempdir().unwrap();
    let cleaner = StagingRoot::new(temp.path(), None, 1).unwrap();
    // A dead daemon's root: the lock file exists but nobody holds it.
    let dead = temp.path().join("staging").join("4242-0-1");
    let compile = dead.join(".compile-4242-7");
    std::fs::create_dir_all(&compile).unwrap();
    std::fs::write(dead.join(STAGING_LOCK_FILE), b"4242\n").unwrap();
    std::fs::write(compile.join("partial.rmeta"), b"still being written").unwrap();
    // The root itself looks old; only the compile output is fresh.
    backdate(&dead, Duration::from_secs(2 * 3600));

    assert_eq!(cleaner.cleanup_abandoned().unwrap(), 0);
    assert!(compile.join("partial.rmeta").exists());

    // Once every entry has been quiet past the period it is crash debris.
    backdate_tree(&dead, Duration::from_secs(2 * 3600));
    assert_eq!(cleaner.cleanup_abandoned().unwrap(), 1);
    assert!(!dead.exists());
}
