//! Tests for cache-hit output delivery: `write_cached_output`,
//! `persist_artifact_output`, `persist_artifact_file`, and
//! `break_output_hardlink_before_compile`. Most of these are regression
//! guards for staleness / cache-poisoning / mtime-preservation bugs that
//! had downstream consequences for cargo's incremental fingerprint.

use super::super::*;

// ── write_cached_output staleness tests ────────────────────────────

/// Regression test: write_cached_output must overwrite an existing output
/// file even when the existing file has the same size as the cached data.
///
/// This reproduces the linker staleness bug where a header change produces
/// a .o of the same size but different content — the old size-only check
/// skipped the write, leaving a stale .o on disk with missing symbols.
#[test]
fn write_cached_output_overwrites_same_size_different_content() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("output.o");
    let cache = dir.path().join("cached.o");

    // Simulate: output.o exists from a previous compilation (version A).
    let old_content = b"AAAA_symbols_v1_xxxx";
    std::fs::write(&out, old_content).unwrap();

    // Simulate: cache file has new content (version B) — same size, different bytes.
    let new_content = b"BBBB_symbols_v2_yyyy";
    assert_eq!(
        old_content.len(),
        new_content.len(),
        "test requires same size"
    );
    std::fs::write(&cache, new_content).unwrap();

    // write_cached_output must replace the stale output with the cached content.
    write_cached_output(&out, &cache, new_content).unwrap();

    let result = std::fs::read(&out).unwrap();
    assert_eq!(
        result, new_content,
        "output must contain new content, not stale old content"
    );
}

/// write_cached_output correctly creates the output when it doesn't exist.
#[test]
fn write_cached_output_creates_new_file() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("output.o");
    let cache = dir.path().join("cached.o");

    let content = b"fresh object file data";
    std::fs::write(&cache, content).unwrap();

    write_cached_output(&out, &cache, content).unwrap();

    let result = std::fs::read(&out).unwrap();
    assert_eq!(result, content.as_slice());
}

/// write_cached_output falls back to memory copy when cache file is missing.
#[test]
fn write_cached_output_fallback_to_memory_copy() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("output.o");
    let cache = dir.path().join("nonexistent_cache.o");

    let content = b"data from memory";

    write_cached_output(&out, &cache, content).unwrap();

    let result = std::fs::read(&out).unwrap();
    assert_eq!(result, content.as_slice());
}

/// write_cached_output skips the write when output is already a hardlink
/// to the cache file (same file identity). This is the fast path for
/// repeated cache hits with the same artifact key.
#[test]
fn write_cached_output_skips_when_already_hardlinked() {
    let dir = tempfile::tempdir().unwrap();
    let cache = dir.path().join("cached.o");
    let out = dir.path().join("output.o");

    let content = b"cached artifact content";
    std::fs::write(&cache, content).unwrap();

    // First write: creates hardlink
    write_cached_output(&out, &cache, content).unwrap();
    assert_eq!(std::fs::read(&out).unwrap(), content.as_slice());

    // Verify they are the same file (hardlink).
    assert!(
        same_file(&out, &cache),
        "output should be a hardlink to cache file after first write"
    );

    // Second write: should detect hardlink and skip.
    // (If it didn't skip, it would still produce correct content,
    //  but the test verifies the optimization path exists.)
    write_cached_output(&out, &cache, content).unwrap();
    assert_eq!(std::fs::read(&out).unwrap(), content.as_slice());
}

#[test]
fn persist_artifact_output_does_not_mutate_existing_hardlink() {
    let dir = tempfile::tempdir().unwrap();
    let cache = dir.path().join("artifact-key_0");
    let out = dir.path().join("output.rlib");

    persist_artifact_output(&cache, b"first").unwrap();
    write_cached_output(&out, &cache, b"first").unwrap();
    assert!(
        same_file(&out, &cache),
        "cache hit should initially hardlink output to cache payload"
    );

    persist_artifact_output(&cache, b"second").unwrap();

    assert_eq!(
        std::fs::read(&out).unwrap(),
        b"first",
        "publishing a later cache payload must not mutate existing target outputs"
    );
    assert_eq!(std::fs::read(&cache).unwrap(), b"second");
    assert!(
        !same_file(&out, &cache),
        "cache path replacement should break the hardlink relationship"
    );
}

#[test]
fn persist_artifact_file_reports_hardlink_snapshot_stats() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("libunit.rlib");
    let cache = dir.path().join("artifact-key_0");
    let content = b"compiled rust artifact";
    std::fs::write(&source, content).unwrap();

    let stats = persist_artifact_file(&cache, &source).unwrap();

    assert_eq!(std::fs::read(&cache).unwrap(), content);
    assert!(
        same_file(&source, &cache),
        "same-directory snapshots should use a hardlink"
    );
    assert_eq!(stats.hardlink_count, 1);
    assert_eq!(stats.copy_count, 0);
    assert_eq!(stats.copy_bytes, 0);
}

/// Regression test for issue #197: a cache hit hardlinks the target
/// output to the shared artifact file. Before a later cache miss invokes
/// the compiler for that same target path, zccache must detach the output
/// from the shared cache file so an in-place compiler overwrite cannot
/// mutate the cache artifact used by sibling worktrees.
#[test]
fn break_output_hardlink_before_compile_prevents_cache_poisoning() {
    let dir = tempfile::tempdir().unwrap();
    let cache = dir.path().join("cached.rlib");
    let out = dir.path().join("libapp.rlib");

    let cached_content = b"cached artifact from worktree a";
    let rebuilt_content = b"rebuilt artifact in worktree b";
    std::fs::write(&cache, cached_content).unwrap();

    write_cached_output(&out, &cache, cached_content).unwrap();
    assert!(same_file(&out, &cache), "cache hit should hardlink output");

    break_output_hardlink_before_compile(&out).unwrap();
    assert!(
        !same_file(&out, &cache),
        "compile miss must detach output from cache hardlink first"
    );

    std::fs::write(&out, rebuilt_content).unwrap();

    assert_eq!(
        std::fs::read(&cache).unwrap(),
        cached_content,
        "compiler overwrite of output must not mutate shared cache artifact"
    );
    assert_eq!(std::fs::read(&out).unwrap(), rebuilt_content);
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
    std::fs::write(&cache, content).unwrap();

    let old_time = filetime::FileTime::from_unix_time(1_000_000_000, 0); // 2001-09-09
    filetime::set_file_mtime(&cache, old_time).unwrap();

    write_cached_output(&out, &cache, content).unwrap();

    // Output is a hardlink to cache, so its mtime is the cache mtime.
    // After the iter7 touch_mtime no-op, that mtime is NOT bumped.
    let out_mtime =
        filetime::FileTime::from_last_modification_time(&std::fs::metadata(&out).unwrap());
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
    std::fs::write(&cache, content).unwrap();

    // First delivery: creates hardlink
    write_cached_output(&out, &cache, content).unwrap();

    let old_time = filetime::FileTime::from_unix_time(1_000_000_000, 0);
    filetime::set_file_mtime(&out, old_time).unwrap();

    // Second delivery: same_file path. Iter7 keeps the existing
    // (backdated) mtime instead of stamping `now()`.
    write_cached_output(&out, &cache, content).unwrap();

    let out_mtime =
        filetime::FileTime::from_last_modification_time(&std::fs::metadata(&out).unwrap());
    assert_eq!(
        out_mtime.unix_seconds(),
        old_time.unix_seconds(),
        "mtime must be preserved across repeated cache hits on the same file"
    );
}

/// write_cached_output fallback (fs::write) naturally sets fresh mtime.
#[test]
fn write_cached_output_fallback_has_fresh_mtime() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("output.rlib");
    let cache = dir.path().join("nonexistent_cache.rlib");

    let content = b"data from memory";
    write_cached_output(&out, &cache, content).unwrap();

    let out_mtime =
        filetime::FileTime::from_last_modification_time(&std::fs::metadata(&out).unwrap());
    let now = filetime::FileTime::now();
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
