//! #1771: a materialized output carries the mtime of the cache object it
//! references, identical for LINK, REFLINK and COPY. The mode may change the
//! syscall, never the resulting mtime.
//!
//! Pinned elsewhere, and unchanged by this contract: #599 (a rustc hit is
//! freshened, `tests.rs::batch_floor_freshens_*`) and iter7 (no `now()` on the
//! single-file hardlink fast path,
//! `tests/write_cached.rs::write_cached_output_preserves_cache_mtime_on_hardlink`
//! and `..._preserves_mtime_on_existing_hardlink`).

use super::*;
use crate::compiler::DeliveryPolicy;
use kernal_api::platform::fs::FileTime;
use std::time::{Duration, SystemTime};

const MODES: [MaterializationMode; 3] = [
    MaterializationMode::Link,
    MaterializationMode::Reflink,
    MaterializationMode::Copy,
];
const OBJECT_SECS: i64 = 1_000_000_000;

fn at(secs: i64) -> FileTime {
    FileTime::from_unix_time(secs, 0)
}

fn secs_to_time(secs: i64) -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(secs as u64)
}

fn mtime(path: &Path) -> FileTime {
    FileTime::from_last_modification_time(&std::fs::metadata(path).unwrap())
}

/// A registered cache blob with an old, recognisable mtime.
fn blob(dir: &Path, name: &str) -> PathBuf {
    let cache = dir.join("cache").join(name);
    std::fs::create_dir_all(cache.parent().unwrap()).unwrap();
    std::fs::write(&cache, b"cached object bytes").unwrap();
    write_authoritative_blob_digest(&cache).unwrap();
    kernal_api::platform::fs::set_file_mtime(&cache, at(OBJECT_SECS)).unwrap();
    cache
}

fn with_mtime(path: &Path, seconds: i64) {
    std::fs::write(path, b"sibling").unwrap();
    kernal_api::platform::fs::set_file_mtime(path, at(seconds)).unwrap();
}

/// Run `deliver` once per mode in a fresh directory and require every mode to
/// leave the same mtimes, equal to `expected`.
fn assert_mode_invariant(
    case: &str,
    expected: FileTime,
    deliver: impl Fn(&Path, MaterializationMode) -> Vec<FileTime>,
) {
    let mut per_mode = Vec::new();
    for mode in MODES {
        let dir = tempfile::tempdir().unwrap();
        per_mode.push((mode, deliver(dir.path(), mode)));
    }
    for (mode, mtimes) in &per_mode {
        assert!(!mtimes.is_empty(), "{case}/{mode}: nothing delivered");
        for observed in mtimes {
            assert_eq!(
                *observed, expected,
                "{case}: {mode} output mtime differs from the contract"
            );
        }
    }
}

/// Single-file C hit (`handle_compile` -> batch materializer, no recorded
/// inputs): `NativeFreshHit`, the `now()` seed. The seed is fixed here.
#[test]
fn single_file_c_hit_has_one_mtime_in_every_mode() {
    let seed = secs_to_time(2_000_000_000);
    assert_mode_invariant("single-file C", at(2_000_000_000), |dir, mode| {
        let cache = blob(dir, "obj-cache");
        let out = dir.join("build/main.o");
        write_payloads_par_with_mtime_floor_and_policies_observed_at(
            &[&out],
            &[CachedPayload::File(cache.into())],
            &Vec::<PathBuf>::new(),
            &[DeliveryPolicy::HardlinkEligible],
            mode,
            seed,
        )
        .unwrap();
        vec![mtime(&out)]
    });
}

/// Multi-source C hit (`handle_compile_multi` -> per-file delivery, no batch
/// floor): `ObjectMtime`. The cache object is not modified either.
#[test]
fn multi_source_c_hit_has_the_object_mtime_in_every_mode() {
    assert_mode_invariant("multi-source C", at(OBJECT_SECS), |dir, mode| {
        let cache = blob(dir, "obj-cache");
        let outs: Vec<PathBuf> = (0..3).map(|i| dir.join(format!("build/u{i}.o"))).collect();
        let payloads = vec![CachedPayload::File(cache.clone().into()); outs.len()];
        write_payloads_par_with_delivery(&outs, &payloads, mode, |_| {
            DeliveryPolicy::HardlinkEligible
        })
        .unwrap();
        assert_eq!(
            mtime(&cache),
            at(OBJECT_SECS),
            "{mode}: cache object touched"
        );
        outs.iter().map(|out| mtime(out)).collect()
    });
}

/// Rustc hit: `RustcInputFloor`. The seed is fixed below the newest recorded
/// input, so the input floor decides.
#[test]
fn rustc_hit_has_one_mtime_in_every_mode() {
    let seed = secs_to_time(2_000_000_000);
    assert_mode_invariant("rustc", at(2_000_000_100), |dir, mode| {
        let cache = blob(dir, "rlib-cache");
        let input = dir.join("src/lib.rs");
        std::fs::create_dir_all(input.parent().unwrap()).unwrap();
        with_mtime(&input, 2_000_000_100);
        let out = dir.join("target/debug/deps/libcrate.rlib");
        write_payloads_par_with_mtime_floor_and_policies_observed_at(
            &[&out],
            &[CachedPayload::File(cache.into())],
            &[input],
            &[DeliveryPolicy::HardlinkEligible],
            mode,
            seed,
        )
        .unwrap();
        vec![mtime(&out)]
    });
}

/// `SiblingFloor` on a per-file hit: the floor raises the output identically
/// in every mode, and never by rewriting the shared cache object (RED before
/// #1771 under LINK, where the freshly created hardlink was floored in place).
#[test]
fn sibling_floor_raises_every_mode_without_touching_the_cache_object() {
    assert_mode_invariant("sibling floor", at(1_500_000_000), |dir, mode| {
        let cache = blob(dir, "rlib-cache");
        let out = dir.join("target/debug/deps/libdependent.rlib");
        std::fs::create_dir_all(out.parent().unwrap()).unwrap();
        with_mtime(&out.with_file_name("libdep.rlib"), 1_500_000_000);
        materialize_cached_file_with_mode(
            &out,
            &cache,
            DeliveryPolicy::HardlinkEligible,
            mode,
            true,
        )
        .unwrap();
        assert_eq!(
            mtime(&cache),
            at(OBJECT_SECS),
            "{mode}: the sibling floor rewrote the cache object"
        );
        // A raised hit is independent and writable in every mode, so rustc's
        // read-only refusal (#1791) cannot apply to it either.
        assert!(
            !crate::platform::fs::identity::same_file(&out, &cache).unwrap(),
            "{mode}: a floor-raised output must not share the cache inode"
        );
        vec![mtime(&out)]
    });
}

/// With no floor in play every mode keeps the object's own mtime (iter7).
#[test]
fn per_file_hit_without_a_floor_keeps_the_object_mtime_in_every_mode() {
    assert_mode_invariant("object mtime", at(OBJECT_SECS), |dir, mode| {
        let cache = blob(dir, "rlib-cache");
        let out = dir.join("target/debug/deps/libonly.rlib");
        std::fs::create_dir_all(out.parent().unwrap()).unwrap();
        materialize_cached_file_with_mode(
            &out,
            &cache,
            DeliveryPolicy::HardlinkEligible,
            mode,
            true,
        )
        .unwrap();
        vec![mtime(&out)]
    });
}

/// A batch hit (compile hit, rustc or C) keeps its hardlink under LINK even
/// when a newer sibling exists: the batch floor stamps the output afterwards
/// anyway, so the per-file floor is skipped (`SiblingFloorPass::BatchFollows`)
/// instead of demoting the hit to a copy. Measured on the rustc bench, the
/// demotion cut warm LINK hardlinks from 500 to 5 of 750 deliveries.
#[test]
fn batch_hit_keeps_its_hardlink_when_a_newer_sibling_exists() {
    let dir = tempfile::tempdir().unwrap();
    let cache = blob(dir.path(), "rlib-cache");
    let out = dir.path().join("target/debug/deps/libdependent.rlib");
    std::fs::create_dir_all(out.parent().unwrap()).unwrap();
    if !fs_caps_raw(&cache, &out).hardlink {
        eprintln!("SKIP: no hardlinks on this volume");
        return;
    }
    with_mtime(&out.with_file_name("libdep.rlib"), 1_500_000_000);
    let seed = secs_to_time(2_000_000_000);
    write_payloads_par_with_mtime_floor_and_policies_observed_at(
        &[&out],
        &[CachedPayload::File(cache.clone().into())],
        &Vec::<PathBuf>::new(),
        &[DeliveryPolicy::HardlinkEligible],
        MaterializationMode::Link,
        seed,
    )
    .unwrap();
    assert!(
        crate::platform::fs::identity::same_file(&out, &cache).unwrap(),
        "a batch hit must stay hardlinked under LINK"
    );
    assert_eq!(mtime(&out), at(2_000_000_000));
}
