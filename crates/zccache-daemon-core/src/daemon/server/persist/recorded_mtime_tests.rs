//! #1771 follow-up: the object mtime recorded at store time survives anything
//! that resets the blob's own filesystem mtime (archive restore, copy between
//! cache roots), for every delivery mode.
//!
//! The record lives in the blob's `.cowhash` digest sidecar (see
//! `link_registry.rs`); the first verified hit of a blob replays it onto the
//! blob, so a hardlink and a COPY/REFLINK of the blob show the same value.

use super::staged_store::perf_counters;
use super::*;
use crate::compiler::DeliveryPolicy;
use kernal_api::platform::fs::FileTime;

const MODES: [MaterializationMode; 3] = [
    MaterializationMode::Link,
    MaterializationMode::Reflink,
    MaterializationMode::Copy,
];
/// A whole-second value: aligned to Windows' 100 ns `FileTime` resolution.
const RESET_SECS: i64 = 1_000_000_000;

fn mtime(path: &Path) -> FileTime {
    FileTime::from_last_modification_time(&std::fs::metadata(path).unwrap())
}

/// Store a blob through the real flat writer and return it with its
/// publication mtime, the value the record must reproduce.
fn stored_blob(dir: &Path) -> (PathBuf, FileTime) {
    let blob = dir.join("cache").join("obj-blob");
    persist_artifact_output(&blob, b"cached object bytes").unwrap();
    let recorded = mtime(&blob);
    assert_ne!(recorded, FileTime::from_unix_time(RESET_SECS, 0));
    (blob, recorded)
}

/// What an archive restore or a cache-root copy does to a blob: the bytes and
/// sidecar survive, the filesystem mtime does not.
fn reset_blob_mtime(blob: &Path) {
    set_materialized_mtime(blob, FileTime::from_unix_time(RESET_SECS, 0)).unwrap();
}

fn hit(out: &Path, blob: &Path, mode: MaterializationMode) {
    std::fs::create_dir_all(out.parent().unwrap()).unwrap();
    materialize_cached_file_with_mode(out, blob, DeliveryPolicy::HardlinkEligible, mode, true)
        .unwrap();
}

#[test]
fn restored_blob_hit_carries_the_recorded_mtime_in_every_mode() {
    for mode in MODES {
        let dir = tempfile::tempdir().unwrap();
        let (blob, recorded) = stored_blob(dir.path());
        reset_blob_mtime(&blob);

        let out = dir.path().join("build/main.o");
        hit(&out, &blob, mode);
        assert_eq!(mtime(&out), recorded, "{mode}: hit mtime is not the record");
    }
}

#[test]
fn link_hit_of_a_restored_blob_stays_hardlinked_and_restamps_the_blob_once() {
    let dir = tempfile::tempdir().unwrap();
    let (blob, recorded) = stored_blob(dir.path());
    let probe = dir.path().join("probe");
    if !fs_caps_raw(&blob, &probe).hardlink {
        eprintln!("SKIP: no hardlinks on this volume");
        return;
    }
    reset_blob_mtime(&blob);

    let _counters = perf_counters::guard();
    perf_counters::reset();
    for name in ["a.o", "b.o", "c.o"] {
        let out = dir.path().join("build").join(name);
        hit(&out, &blob, MaterializationMode::Link);
        assert!(crate::platform::fs::identity::same_file(&out, &blob).unwrap());
        assert_eq!(mtime(&out), recorded);
    }
    hit(
        &dir.path().join("build/copy.o"),
        &blob,
        MaterializationMode::Copy,
    );
    assert_eq!(mtime(&blob), recorded);
    assert_eq!(
        perf_counters::blob_mtime_restores(),
        1,
        "the blob is restamped once on its first verified hit, not per hit"
    );
}

#[test]
fn a_blob_whose_mtime_already_matches_is_never_rewritten() {
    let dir = tempfile::tempdir().unwrap();
    let (blob, recorded) = stored_blob(dir.path());

    let _counters = perf_counters::guard();
    perf_counters::reset();
    hit(
        &dir.path().join("build/a.o"),
        &blob,
        MaterializationMode::Copy,
    );
    assert_eq!(mtime(&blob), recorded);
    assert_eq!(perf_counters::blob_mtime_restores(), 0);
}

#[test]
fn legacy_sidecar_without_a_record_keeps_the_blob_filesystem_mtime() {
    for mode in MODES {
        let dir = tempfile::tempdir().unwrap();
        let (blob, _) = stored_blob(dir.path());
        // A pre-record sidecar is exactly the 32-byte digest.
        let sidecar = super::link_registry::digest_path(&blob);
        let bytes = std::fs::read(&sidecar).unwrap();
        std::fs::write(&sidecar, &bytes[..32]).unwrap();
        reset_blob_mtime(&blob);

        let _counters = perf_counters::guard();
        perf_counters::reset();
        let out = dir.path().join("build/main.o");
        hit(&out, &blob, mode);
        assert_eq!(
            mtime(&out),
            FileTime::from_unix_time(RESET_SECS, 0),
            "{mode}: a legacy entry keeps today's behaviour"
        );
        assert_eq!(perf_counters::blob_mtime_restores(), 0);
    }
}

#[test]
fn staged_generation_records_the_object_mtime_it_was_published_with() {
    for mode in MODES {
        let dir = tempfile::tempdir().unwrap();
        let artifact_dir = dir.path().join("artifacts");
        std::fs::create_dir_all(&artifact_dir).unwrap();
        let source = dir.path().join("source.rlib");
        std::fs::write(&source, b"staged immutable payload").unwrap();
        let produced = FileTime::from_unix_time(1_500_000_000, 0);
        set_materialized_mtime(&source, produced).unwrap();

        let key = "b".repeat(64);
        persist_staged_artifact_paths(&artifact_dir, &key, &[source.clone().into()]).unwrap();
        let payloads = load_staged_artifact_paths(&artifact_dir, &key, &[24])
            .unwrap()
            .unwrap();
        let blob = payloads[0].as_path();
        reset_blob_mtime(blob);

        let out = dir.path().join("build/libx.rlib");
        hit(&out, blob, mode);
        assert_eq!(mtime(&out), produced, "{mode}: staged hit mtime");
    }
}
