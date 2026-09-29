//! Unit tests for the #1771 mtime policy (see `mtime.rs`).

use super::*;
use std::path::PathBuf;

fn at(secs: i64) -> FileTime {
    FileTime::from_unix_time(secs, 0)
}

fn mtime_of(path: &Path) -> FileTime {
    FileTime::from_last_modification_time(&std::fs::metadata(path).unwrap())
}

fn file(dir: &Path, name: &str, secs: i64) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, b"x").unwrap();
    stamp_mtime(&path, at(secs)).unwrap();
    path
}

#[test]
fn recorded_nanos_replay_exactly_and_zero_means_now() {
    // A multiple of 100 ns: Windows `FileTime` has 100 ns resolution, so a
    // finer value would be truncated there (123_456_789 -> 123_456_700).
    let recorded = recorded_mtime_or_now(1_700_000_000_123_456_700);
    assert_eq!(recorded.unix_seconds(), 1_700_000_000);
    assert_eq!(recorded.nanoseconds(), 123_456_700);

    let before = FileTime::from_system_time(SystemTime::now());
    let unset = recorded_mtime_or_now(0);
    assert!(
        unset >= before,
        "a manifest without a recording falls back to now"
    );
}

#[test]
fn stamp_recorded_mtime_rejects_an_unrepresentable_timestamp() {
    let dir = tempfile::tempdir().unwrap();
    let path = file(dir.path(), "out.o", 10);
    let error = stamp_recorded_mtime(&path, u64::MAX, 0).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    assert_eq!(
        mtime_of(&path),
        at(10),
        "a rejected replay leaves the file alone"
    );
}

#[test]
fn touch_cache_object_stamps_a_read_only_object() {
    let dir = tempfile::tempdir().unwrap();
    let object = file(dir.path(), "blob", 10);
    let mut perms = std::fs::metadata(&object).unwrap().permissions();
    perms.set_readonly(true);
    std::fs::set_permissions(&object, perms).unwrap();

    touch_cache_object(&object, at(500)).unwrap();
    assert_eq!(mtime_of(&object), at(500));
}

#[test]
fn hit_mtime_is_the_object_mtime_unless_a_sibling_is_newer() {
    let dir = tempfile::tempdir().unwrap();
    let object = file(dir.path(), "blob", 100);
    let out = dir.path().join("libfoo.rlib");

    let plain = resolve_hit_mtime(&out, &object, SiblingFloorPass::PerFile).unwrap();
    assert_eq!(plain.mtime, at(100));
    assert!(!plain.raised);

    file(dir.path(), "libdep.rlib", 900);
    let floored = resolve_hit_mtime(&out, &object, SiblingFloorPass::PerFile).unwrap();
    assert_eq!(floored.mtime, at(900));
    assert!(floored.raised);

    let batch = resolve_hit_mtime(&out, &object, SiblingFloorPass::BatchFollows).unwrap();
    assert_eq!(
        batch.mtime,
        at(100),
        "the batch floor decides, not the sibling scan"
    );
    assert!(!batch.raised);
}

#[test]
fn batch_policies_differ_only_by_the_recorded_input_floor() {
    let dir = tempfile::tempdir().unwrap();
    let seed = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000);
    let future_input = file(dir.path(), "lib.rs", 5_000);

    let native = file(dir.path(), "native.o", 10);
    apply_batch_policy_with(
        BatchPolicy::NativeFreshHit,
        [native.as_path()],
        [future_input.as_path()],
        seed,
        stamp_mtime,
    );
    assert_eq!(
        mtime_of(&native),
        at(1_000),
        "native hits ignore inputs (#1770)"
    );

    let rustc = file(dir.path(), "out.rlib", 10);
    apply_batch_policy_with(
        BatchPolicy::RustcInputFloor,
        [rustc.as_path()],
        [future_input.as_path()],
        seed,
        stamp_mtime,
    );
    assert_eq!(
        mtime_of(&rustc),
        at(5_000),
        "rustc hits floor to the newest input (#599)"
    );
}
