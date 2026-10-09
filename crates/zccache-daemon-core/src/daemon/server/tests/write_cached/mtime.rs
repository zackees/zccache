use super::*;

#[cfg(unix)]
#[test]
fn dangling_output_symlink_is_replaced() {
    use std::os::unix::fs::symlink;

    let dir = tempfile::tempdir().unwrap();
    let blob = dir.path().join("blob.rlib");
    let output = dir.path().join("output.rlib");
    seed_persisted_blob(&blob, b"original");
    symlink(dir.path().join("missing"), &output).unwrap();

    write_cached_file(&output, &blob).unwrap();

    assert_eq!(std::fs::read(output).unwrap(), b"original");
}

/// Regression test for issue #15: hardlink delivery must set output mtime
/// to current time. Without this, build systems (cargo, make, ninja) see
/// the output as older than its dependencies and trigger unnecessary rebuilds.
///
/// Root cause: hardlinks share mtime with the cache file, which was created
/// during the original compilation (potentially minutes/hours ago). Cargo
/// checks "is library output older than build script output?" and if the
/// library was hardlinked from an old cache file, the answer is yes → dirty.
#[test]
fn write_cached_output_preserves_cache_mtime_on_hardlink() {
    // Regression guard for iter7: cache hits must keep the cache
    // file's stored mtime, not stamp `now()`. Cargo's incremental
    // fingerprint records the artifact's mtime at first compile;
    // a hit that hardlinks but bumps mtime looks "externally
    // touched" and invalidates downstream — measured as a
    // wall-time regression on the `bin` cell of the
    // cold-tar-untar-warm scenario.
    let dir = tempfile::tempdir().unwrap();
    let cache = dir.path().join("cached.rlib");
    let out = dir.path().join("output.rlib");

    let content = b"cached rlib data";
    seed_persisted_blob(&cache, content);

    let old_time = kernal_api::platform::fs::FileTime::from_unix_time(1_000_000_000, 0); // 2001-09-09
    kernal_api::platform::fs::set_file_mtime(&cache, old_time).unwrap();

    write_cached_output(&out, &cache, content).unwrap();

    // Output is a hardlink to cache, so its mtime is the cache mtime.
    // After the iter7 touch_mtime no-op, that mtime is NOT bumped.
    let out_mtime = kernal_api::platform::fs::FileTime::from_last_modification_time(
        &std::fs::metadata(&out).unwrap(),
    );
    assert_eq!(
        out_mtime.unix_seconds(),
        old_time.unix_seconds(),
        "cache hit must preserve cache file mtime (cargo's fingerprint depends on it); \
         got {out_mtime:?}, expected {old_time:?}"
    );
}

/// Same as above but for the same_file (already hardlinked) path.
#[test]
fn write_cached_output_preserves_mtime_on_existing_hardlink() {
    let dir = tempfile::tempdir().unwrap();
    let cache = dir.path().join("cached.rlib");
    let out = dir.path().join("output.rlib");

    let content = b"cached rlib data";
    seed_persisted_blob(&cache, content);

    // First delivery: creates hardlink
    deliver_linked(&out, &cache);

    let old_time = kernal_api::platform::fs::FileTime::from_unix_time(1_000_000_000, 0);
    set_materialized_mtime(&out, old_time).unwrap();

    // See the comment in write_cached_output_skips_when_already_hardlinked:
    // this must hold in every CI environment this suite runs in — assert it
    // loudly so this test always checks the "mtime preserved on the
    // same-file path" invariant it's named for, instead of silently falling
    // back to checking the reflink-tier formula instead (issue #1042
    // test-coverage regression).
    if !require_hardlink(
        &out,
        &cache,
        "write_cached_output_preserves_mtime_on_existing_hardlink",
    ) {
        return;
    }

    // Second delivery: same_file keeps the linked mtime.
    deliver_linked(&out, &cache);

    let out_mtime = kernal_api::platform::fs::FileTime::from_last_modification_time(
        &std::fs::metadata(&out).unwrap(),
    );
    assert_eq!(
        out_mtime.unix_seconds(),
        old_time.unix_seconds(),
        "mtime must be preserved on the same_file (already-hardlinked) path"
    );
}

/// Regression test: when a sibling-floor mtime bump is required on the
/// same_file (already-hardlinked) path, it must not mutate the shared cache
/// blob's mtime — every other output hardlinked to that blob would see the
/// bump too. The output should instead detach into a private copy that
/// carries the floored mtime, leaving the blob (and any other hardlink to
/// it) untouched.
#[test]
fn write_cached_output_floor_detaches_instead_of_corrupting_shared_blob() {
    let dir = tempfile::tempdir().unwrap();
    let cache = dir.path().join("cached.rlib");
    let out = dir.path().join("output.rlib");
    let sibling = dir.path().join("sibling.rlib");

    let content = b"cached rlib data";
    seed_persisted_blob(&cache, content);

    // First delivery: creates the hardlink (or copy, depending on fs caps).
    write_cached_output(&out, &cache, content).unwrap();
    if !crate::platform::fs::identity::same_file(&out, &cache).unwrap() {
        // This filesystem doesn't support hardlinks — the detach path this
        // test targets never triggers. Nothing to verify.
        return;
    }

    let old_time = kernal_api::platform::fs::FileTime::from_unix_time(1_000_000_000, 0);
    set_materialized_mtime(&out, old_time).unwrap();
    let blob_time_before = kernal_api::platform::fs::FileTime::from_last_modification_time(
        &std::fs::metadata(&cache).unwrap(),
    );

    // A newer sibling artifact in the same directory forces the #466/#467
    // floor to kick in on the next materialization of `out`.
    std::fs::write(&sibling, b"newer sibling").unwrap();
    let newer_time = kernal_api::platform::fs::FileTime::from_unix_time(2_000_000_000, 0);
    kernal_api::platform::fs::set_file_mtime(&sibling, newer_time).unwrap();

    // Second delivery: same_file path, floor must apply.
    write_cached_output(&out, &cache, content).unwrap();

    let out_mtime = kernal_api::platform::fs::FileTime::from_last_modification_time(
        &std::fs::metadata(&out).unwrap(),
    );
    assert_eq!(
        out_mtime.unix_seconds(),
        newer_time.unix_seconds(),
        "output mtime must be floored up to the newer sibling"
    );

    let blob_time_after = kernal_api::platform::fs::FileTime::from_last_modification_time(
        &std::fs::metadata(&cache).unwrap(),
    );
    assert_eq!(
        blob_time_after.unix_seconds(),
        blob_time_before.unix_seconds(),
        "flooring an output must never mutate the shared cache blob's mtime"
    );

    assert!(
        !crate::platform::fs::identity::same_file(&out, &cache).unwrap(),
        "output must be detached from the shared blob once its mtime diverges"
    );
    assert_eq!(std::fs::read(&cache).unwrap(), content);
    assert_eq!(std::fs::read(&out).unwrap(), content);
}

/// write_cached_output fallback (fs::write) naturally sets fresh mtime.
#[test]
fn write_cached_output_fallback_has_fresh_mtime() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("output.rlib");
    let cache = dir.path().join("nonexistent_cache.rlib");

    let content = b"data from memory";
    write_cached_output(&out, &cache, content).unwrap();

    let out_mtime = kernal_api::platform::fs::FileTime::from_last_modification_time(
        &std::fs::metadata(&out).unwrap(),
    );
    let now = kernal_api::platform::fs::FileTime::now();
    let diff = now.unix_seconds() - out_mtime.unix_seconds();

    assert!(
        diff < 5,
        "fallback path should produce fresh mtime — {diff}s old"
    );
}

// ── Issue #490: AV-scanner rename race ─────────────────────────────
//
// On Windows, Defender (MsMpEng) opens just-written files for an inline scan
// with `FILE_SHARE_READ` only — no `FILE_SHARE_DELETE`. While that handle is
// live, `MoveFileExW(MOVEFILE_REPLACE_EXISTING)` and `DeleteFileW` against the
// target file return `ERROR_ACCESS_DENIED` (raw OS error 5) or
// `ERROR_SHARING_VIOLATION` (32). The scan window is short — typically tens to
// hundreds of milliseconds — so a bounded retry absorbs it.
//
// This test simulates the scanner by holding the rename destination open with
// the same restrictive share mode from a separate thread that releases the
// handle shortly. The pre-fix code path fails immediately at the first
// `remove_file` call inside `replace_artifact_cache_file`; the retry must
// outlive the held handle.

#[cfg(windows)]
#[test]
fn replace_artifact_cache_file_retries_through_av_scanner_lock() {
    use std::os::windows::fs::OpenOptionsExt;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("output.obj");
    let tmp = dir.path().join("output.obj.tmp");

    std::fs::write(&target, b"OLD").unwrap();
    std::fs::write(&tmp, b"NEW").unwrap();

    // FILE_SHARE_READ (0x1) only — no SHARE_WRITE, no SHARE_DELETE.
    // This is the exact share mode Defender uses during real-time inline
    // scans, which is why ninja's subsequent remove fails in the wild.
    let handle = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(0x1)
        .open(&target)
        .expect("open lock handle");

    let released = Arc::new(AtomicBool::new(false));
    let released_clone = Arc::clone(&released);
    let worker = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(150));
        drop(handle);
        released_clone.store(true, Ordering::SeqCst);
    });

    // Without the retry, this fails immediately at the inner remove_file with
    // ERROR_ACCESS_DENIED. With the retry, the closure retries past the
    // 150 ms hold and succeeds.
    replace_artifact_cache_file(&tmp, &target).expect("replace must absorb the simulated AV lock");

    worker.join().unwrap();
    assert!(
        released.load(Ordering::SeqCst),
        "worker must have released the lock before the call returned",
    );
    assert_eq!(std::fs::read(&target).unwrap(), b"NEW");
}

// Negative guard: a NotFound (or any other non-transient) error must surface
// immediately rather than burning the retry budget. Without this assertion,
// a misclassified `ErrorKind` could silently inflate every cache-store
// failure path by ~1 s.
#[cfg(windows)]
#[test]
fn replace_artifact_cache_file_does_not_retry_non_transient_errors() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("output.obj");
    let tmp = dir.path().join("missing.obj.tmp"); // tmp does NOT exist

    // Neither file exists; rename returns ENOENT/NotFound — not a share
    // violation. The call must fail promptly, not after the full budget.
    let start = std::time::Instant::now();
    let err = replace_artifact_cache_file(&tmp, &target).expect_err("must propagate NotFound");
    let elapsed = start.elapsed();

    assert!(
        elapsed < std::time::Duration::from_millis(45),
        "non-transient errors must not enter the retry sleep — took {elapsed:?}"
    );
    assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
}
