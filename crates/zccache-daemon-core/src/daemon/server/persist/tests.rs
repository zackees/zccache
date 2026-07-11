//! Tests for `floor_artifact_mtime_to_sibling_max` (issues #466 / #467) and
//! the batch-floor materializer (issue #599).
//!
//! These exercise the dep-mtime-ordering fix in isolation, without
//! standing up a full daemon. The functions are private; the tests live
//! in the persist module so they can call them directly.

use super::*;
use std::time::{Duration, SystemTime};

fn write_with_mtime(path: &Path, contents: &[u8], mtime: SystemTime) {
    std::fs::write(path, contents).unwrap();
    let ft = filetime::FileTime::from_system_time(mtime);
    filetime::set_file_mtime(path, ft).unwrap();
}

fn mtime_of(path: &Path) -> SystemTime {
    std::fs::metadata(path).unwrap().modified().unwrap()
}

fn epoch_plus(secs: u64) -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(secs)
}

#[test]
fn floor_noop_when_target_dir_is_empty() {
    // Single artifact, no siblings — mtime must be preserved (iter7
    // invariant). The floor must not invent a value out of thin air.
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("only.rlib");
    let before = epoch_plus(1_000_000);
    write_with_mtime(&target, b"x", before);

    floor_artifact_mtime_to_sibling_max(&target).unwrap();

    assert_eq!(mtime_of(&target), before);
}

#[test]
fn floor_noop_when_already_newest() {
    // Target artifact already has the highest mtime among siblings —
    // floor must not lower it (this is the "fresh build" case).
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("newer.rlib");
    let older = dir.path().join("older.rlib");
    write_with_mtime(&target, b"t", epoch_plus(2_000_000));
    write_with_mtime(&older, b"o", epoch_plus(1_000_000));

    floor_artifact_mtime_to_sibling_max(&target).unwrap();

    assert_eq!(mtime_of(&target), epoch_plus(2_000_000));
}

#[test]
fn floor_bumps_when_sibling_is_newer() {
    // The "cache hit out of order" case: zccache materialised the
    // dependent first (older cache mtime), the dep second (newer cache
    // mtime). Cargo's strict `dep_mtime > my_mtime → stale` would fire.
    // After floor, `my_mtime == dep_mtime`, satisfying `dep > my == false`.
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("dependent.rlib");
    let dep = dir.path().join("dep.rlib");
    write_with_mtime(&target, b"t", epoch_plus(1_000_000));
    write_with_mtime(&dep, b"d", epoch_plus(2_000_000));

    floor_artifact_mtime_to_sibling_max(&target).unwrap();

    // Floored UP to the dep's mtime — cargo's check passes.
    assert_eq!(mtime_of(&target), epoch_plus(2_000_000));
    // Dep was not touched.
    assert_eq!(mtime_of(&dep), epoch_plus(2_000_000));
}

#[test]
fn floor_ignores_non_artifact_files() {
    // Cargo's StaleDependency check looks at output artifacts only
    // (rlib/rmeta/so/dylib/dll/exe/a/lib). The floor must skip
    // depfiles (.d), fingerprint state, JSON sidecars, etc., so a
    // newer .d file doesn't artificially bump the artifact's mtime.
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("art.rlib");
    let dep_file = dir.path().join("dep.d");
    let json_sidecar = dir.path().join("meta.json");
    write_with_mtime(&target, b"t", epoch_plus(1_000_000));
    write_with_mtime(&dep_file, b"d", epoch_plus(5_000_000));
    write_with_mtime(&json_sidecar, b"j", epoch_plus(5_000_000));

    floor_artifact_mtime_to_sibling_max(&target).unwrap();

    // .d and .json are filtered out — target mtime stays at its
    // original value.
    assert_eq!(mtime_of(&target), epoch_plus(1_000_000));
}

#[test]
fn floor_idempotent_under_repeated_application() {
    // Subsequent cache hits for the same artifact must converge to a
    // stable mtime — otherwise cargo's "externally modified" check
    // (the original iter7 concern) would fire on repeat builds.
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("art.rlib");
    let dep = dir.path().join("dep.rlib");
    write_with_mtime(&target, b"t", epoch_plus(1_000_000));
    write_with_mtime(&dep, b"d", epoch_plus(2_000_000));

    floor_artifact_mtime_to_sibling_max(&target).unwrap();
    let first = mtime_of(&target);
    floor_artifact_mtime_to_sibling_max(&target).unwrap();
    let second = mtime_of(&target);
    floor_artifact_mtime_to_sibling_max(&target).unwrap();
    let third = mtime_of(&target);

    assert_eq!(first, epoch_plus(2_000_000));
    assert_eq!(second, first);
    assert_eq!(third, first);
}

#[test]
fn batch_floor_bumps_build_script_output_to_extern_mtime() {
    // Issue #599: build-script binaries live in target/debug/build/*,
    // while their rustc extern dependencies live in target/debug/deps.
    // The same-directory floor never saw those extern artifacts.
    let dir = tempfile::tempdir().unwrap();
    let build_dir = dir.path().join("target/debug/build/blake3-abc");
    let deps_dir = dir.path().join("target/debug/deps");
    std::fs::create_dir_all(&build_dir).unwrap();
    std::fs::create_dir_all(&deps_dir).unwrap();

    let cache = dir.path().join("cache/build-script-cache");
    std::fs::create_dir_all(cache.parent().unwrap()).unwrap();
    std::fs::write(&cache, b"build script exe").unwrap();
    write_authoritative_blob_digest(&cache).unwrap();
    let old_time = filetime::FileTime::from_unix_time(1_000_000, 0);
    filetime::set_file_mtime(&cache, old_time).unwrap();

    let extern_dep = deps_dir.join("libcc-new.rlib");
    write_with_mtime(
        &extern_dep,
        b"cc rlib",
        SystemTime::UNIX_EPOCH + Duration::new(2_000_000, 123_456_700),
    );
    let dep_mtime = mtime_of(&extern_dep);

    let output = build_dir.join("build-script-build");
    let targets = vec![(output.clone(), cache.clone())];
    let payloads = vec![CachedPayload::File(cache.clone().into())];
    let floor_paths = vec![extern_dep.clone()];

    assert!(write_payloads_par_with_mtime_floor(
        &targets,
        &payloads,
        &floor_paths,
    ));

    let output_mtime = mtime_of(&output);
    assert!(
        output_mtime >= dep_mtime,
        "extensionless build-script output must be at least as new as extern dependency; \
         output={output_mtime:?}, dep={dep_mtime:?}",
    );
}

#[test]
fn batch_floor_freshens_materialized_outputs_without_floor_paths() {
    // Issue #599: a compile cache hit is still a rustc invocation from
    // Cargo's perspective. If zccache hardlinks an old cache artifact and
    // preserves that old mtime, Cargo records stale output mtimes and the
    // next no-op build recompiles the graph. The batch materializer uses
    // one fresh floor for all outputs from that hit.
    let dir = tempfile::tempdir().unwrap();
    let cache = dir.path().join("cache/libcrate-cache.rlib");
    std::fs::create_dir_all(cache.parent().unwrap()).unwrap();
    std::fs::write(&cache, b"rlib").unwrap();
    write_authoritative_blob_digest(&cache).unwrap();
    let old_mtime = epoch_plus(1_000_000);
    filetime::set_file_mtime(&cache, filetime::FileTime::from_system_time(old_mtime)).unwrap();

    let output = dir.path().join("target/debug/deps/libcrate.rlib");
    let targets = vec![(output.clone(), cache.clone())];
    let payloads = vec![CachedPayload::File(cache.clone().into())];
    let floor_paths: Vec<PathBuf> = Vec::new();

    assert!(write_payloads_par_with_mtime_floor(
        &targets,
        &payloads,
        &floor_paths,
    ));

    let output_mtime = mtime_of(&output);
    assert!(
        output_mtime > old_mtime,
        "compile-hit output must not inherit the stale cache mtime; \
         output={output_mtime:?}, old_cache={old_mtime:?}",
    );
}

#[test]
fn fs_caps_stays_correct_across_many_distinct_destination_dirs() {
    // Issue #1042 finding #6: the `VolumePair` cache key includes a
    // destination-parent `PathBuf` (fix for the ephemeral-device-id-reuse
    // bug), so a long-running daemon servicing many distinct build-output
    // directories accumulates one cache entry per distinct destination
    // parent with no eviction. `CAPS_CACHE_LIMIT` (4096) is too large to
    // exhaustively exercise here, so this regression test instead asserts
    // the practical invariant: probing many distinct destination
    // directories must keep returning correct, consistent capabilities
    // and must never panic, regardless of how large the cache grows.
    let src_dir = tempfile::tempdir().unwrap();
    let src = src_dir.path().join("src.rlib");
    std::fs::write(&src, b"source artifact").unwrap();

    let root = tempfile::tempdir().unwrap();
    let mut first_caps = None;
    for index in 0..100 {
        let dst_dir = root.path().join(format!("dest-{index}"));
        std::fs::create_dir_all(&dst_dir).unwrap();
        let dst = dst_dir.join("out.rlib");

        let caps = fs_caps(&src, &dst);
        // Calling again for the same destination must be idempotent
        // (cache hit or a fresh, consistent re-probe after an eviction).
        let caps_again = fs_caps(&src, &dst);
        assert_eq!(
            caps, caps_again,
            "fs_caps must return consistent capabilities for the same destination on repeat calls"
        );

        if let Some(first) = first_caps {
            assert_eq!(
                caps, first,
                "same src/dst volume pair on the same filesystem should probe identical capabilities"
            );
        } else {
            first_caps = Some(caps);
        }
    }
}

/// Function-level warm-hit budget for #1039. The generous 2-second ceiling is
/// over 3x the observed Windows/NTFS debug-build time for 128 deliveries while
/// still catching accidental per-hit hashing or probe-cache regressions.
#[test]
fn perf_cow_materialization_128_hits_under_two_seconds() {
    let dir = tempfile::tempdir().unwrap();
    let cache = dir.path().join("cache/blob.rlib");
    std::fs::create_dir_all(cache.parent().unwrap()).unwrap();
    std::fs::write(&cache, vec![0x5a; 256 * 1024]).unwrap();
    write_authoritative_blob_digest(&cache).unwrap();
    let started = std::time::Instant::now();
    for index in 0..128 {
        let output = dir.path().join(format!("target/output-{index}.rlib"));
        std::fs::create_dir_all(output.parent().unwrap()).unwrap();
        write_cached_file(&output, &cache).unwrap();
        make_writable(&output).unwrap();
    }
    let elapsed = started.elapsed();
    make_writable(&cache).unwrap();
    assert!(
        elapsed < Duration::from_secs(2),
        "128 capability-driven materializations took {elapsed:?}"
    );
}
