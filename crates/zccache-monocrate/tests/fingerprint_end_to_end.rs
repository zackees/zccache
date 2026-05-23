mod common;

use tempfile::TempDir;
use zccache_fingerprint::{
    walk_files, walk_files_glob, CacheDecision, HashCache, RunReason, TwoLayerCache,
};

fn setup() -> (TempDir, TempDir) {
    (TempDir::new().unwrap(), TempDir::new().unwrap())
}

// ── HashCache lifecycle ──────────────────────────────────────────

#[test]
fn hash_cache_full_lifecycle() {
    let (src, cache_dir) = setup();
    common::create_file(src.path(), "a.rs", "v1");

    let cache = HashCache::new(cache_dir.path().join("fp.json"));
    let files = walk_files(src.path(), &[], &[]).unwrap();

    // First run: NoCacheFile.
    let d = cache.check(&files).unwrap();
    assert_eq!(d, CacheDecision::Run(RunReason::NoCacheFile));
    cache.mark_success().unwrap();

    // No changes: Skip.
    let files = walk_files(src.path(), &[], &[]).unwrap();
    let d = cache.check(&files).unwrap();
    assert_eq!(d, CacheDecision::Skip);

    // Modify file: ContentChanged.
    common::create_file(src.path(), "a.rs", "v2");
    let files = walk_files(src.path(), &[], &[]).unwrap();
    let d = cache.check(&files).unwrap();
    assert_eq!(d, CacheDecision::Run(RunReason::ContentChanged));
    cache.mark_success().unwrap();

    // After mark_success: Skip again.
    let files = walk_files(src.path(), &[], &[]).unwrap();
    let d = cache.check(&files).unwrap();
    assert_eq!(d, CacheDecision::Skip);
}

#[test]
fn two_layer_full_lifecycle() {
    let (src, cache_dir) = setup();
    common::create_file(src.path(), "a.rs", "v1");

    let cache = TwoLayerCache::new(cache_dir.path().join("fp.json"));
    let files = walk_files(src.path(), &[], &[]).unwrap();

    let d = cache.check(&files).unwrap();
    assert_eq!(d, CacheDecision::Run(RunReason::NoCacheFile));
    cache.mark_success().unwrap();

    let files = walk_files(src.path(), &[], &[]).unwrap();
    let d = cache.check(&files).unwrap();
    assert_eq!(d, CacheDecision::Skip);

    common::wait_for_mtime_change();
    common::create_file(src.path(), "a.rs", "v2");
    let files = walk_files(src.path(), &[], &[]).unwrap();
    let d = cache.check(&files).unwrap();
    assert_eq!(d, CacheDecision::Run(RunReason::ContentChanged));
    cache.mark_success().unwrap();

    let files = walk_files(src.path(), &[], &[]).unwrap();
    let d = cache.check(&files).unwrap();
    assert_eq!(d, CacheDecision::Skip);
}

// ── Failure recovery ─────────────────────────────────────────────

#[test]
fn hash_cache_failure_recovery() {
    let (src, cache_dir) = setup();
    common::create_file(src.path(), "a.rs", "a");

    let cache = HashCache::new(cache_dir.path().join("fp.json"));
    let files = walk_files(src.path(), &[], &[]).unwrap();

    cache.check(&files).unwrap();
    cache.mark_failure().unwrap();

    let d = cache.check(&files).unwrap();
    assert_eq!(d, CacheDecision::Run(RunReason::PreviousFailure));
    cache.mark_success().unwrap();

    let d = cache.check(&files).unwrap();
    assert_eq!(d, CacheDecision::Skip);
}

#[test]
fn two_layer_failure_recovery() {
    let (src, cache_dir) = setup();
    common::create_file(src.path(), "a.rs", "a");

    let cache = TwoLayerCache::new(cache_dir.path().join("fp.json"));
    let files = walk_files(src.path(), &[], &[]).unwrap();

    cache.check(&files).unwrap();
    cache.mark_failure().unwrap();

    let files = walk_files(src.path(), &[], &[]).unwrap();
    let d = cache.check(&files).unwrap();
    assert_eq!(d, CacheDecision::Run(RunReason::PreviousFailure));
    cache.mark_success().unwrap();

    let files = walk_files(src.path(), &[], &[]).unwrap();
    let d = cache.check(&files).unwrap();
    assert_eq!(d, CacheDecision::Skip);
}

// ── Touch optimization ───────────────────────────────────────────

#[test]
fn touch_optimization_two_layer() {
    let (src, cache_dir) = setup();
    common::create_file(src.path(), "a.rs", "stable");

    let cache = TwoLayerCache::new(cache_dir.path().join("fp.json"));
    cache
        .check(&walk_files(src.path(), &[], &[]).unwrap())
        .unwrap();
    cache.mark_success().unwrap();

    // Touch file (same content, new mtime).
    common::wait_for_mtime_change();
    common::create_file(src.path(), "a.rs", "stable");

    let d = cache
        .check(&walk_files(src.path(), &[], &[]).unwrap())
        .unwrap();
    assert_eq!(d, CacheDecision::Skip);
}

// ── Crash consistency ────────────────────────────────────────────

#[test]
fn crash_consistency_hash_cache() {
    let (src, cache_dir) = setup();
    common::create_file(src.path(), "a.rs", "v1");

    let cache = HashCache::new(cache_dir.path().join("fp.json"));
    let files = walk_files(src.path(), &[], &[]).unwrap();

    cache.check(&files).unwrap();
    cache.mark_success().unwrap();

    // Check without mark_success (simulates crash).
    cache.check(&files).unwrap();
    // Don't call mark_success!

    // Should still skip (reads main cache from previous successful cycle).
    let d = cache.check(&files).unwrap();
    assert_eq!(d, CacheDecision::Skip);
}

#[test]
fn crash_consistency_two_layer() {
    let (src, cache_dir) = setup();
    common::create_file(src.path(), "a.rs", "v1");

    let cache = TwoLayerCache::new(cache_dir.path().join("fp.json"));
    let files = walk_files(src.path(), &[], &[]).unwrap();

    cache.check(&files).unwrap();
    cache.mark_success().unwrap();

    cache.check(&files).unwrap();

    let d = cache.check(&files).unwrap();
    assert_eq!(d, CacheDecision::Skip);
}

// ── File added/removed during lifecycle ──────────────────────────

#[test]
fn file_added_during_lifecycle() {
    let (src, cache_dir) = setup();
    common::create_file(src.path(), "a.rs", "a");

    let cache = HashCache::new(cache_dir.path().join("fp.json"));
    cache
        .check(&walk_files(src.path(), &[], &[]).unwrap())
        .unwrap();
    cache.mark_success().unwrap();

    common::create_file(src.path(), "b.rs", "b");
    let d = cache
        .check(&walk_files(src.path(), &[], &[]).unwrap())
        .unwrap();
    assert_eq!(d, CacheDecision::Run(RunReason::ContentChanged));
}

#[test]
fn file_removed_during_lifecycle() {
    let (src, cache_dir) = setup();
    common::create_file(src.path(), "a.rs", "a");
    common::create_file(src.path(), "b.rs", "b");

    let cache = TwoLayerCache::new(cache_dir.path().join("fp.json"));
    cache
        .check(&walk_files(src.path(), &[], &[]).unwrap())
        .unwrap();
    cache.mark_success().unwrap();

    std::fs::remove_file(src.path().join("b.rs")).unwrap();
    let d = cache
        .check(&walk_files(src.path(), &[], &[]).unwrap())
        .unwrap();
    assert_eq!(d, CacheDecision::Run(RunReason::ContentChanged));
}

// ── Glob + cache integration ─────────────────────────────────────

#[test]
fn glob_with_hash_cache() {
    let (src, cache_dir) = setup();
    common::create_file(src.path(), "src/a.rs", "r");
    common::create_file(src.path(), "src/b.py", "p");
    common::create_file(src.path(), "Cargo.toml", "t");

    let cache = HashCache::new(cache_dir.path().join("fp.json"));
    let files = walk_files_glob(src.path(), &["src/**/*.rs"], &[]).unwrap();
    assert_eq!(files.len(), 1);

    cache.check(&files).unwrap();
    cache.mark_success().unwrap();

    // Modify Python file (not in glob set) — should still skip.
    common::create_file(src.path(), "src/b.py", "changed");
    let files = walk_files_glob(src.path(), &["src/**/*.rs"], &[]).unwrap();
    let d = cache.check(&files).unwrap();
    assert_eq!(d, CacheDecision::Skip);

    // Modify Rust file — should detect change.
    common::create_file(src.path(), "src/a.rs", "changed");
    let files = walk_files_glob(src.path(), &["src/**/*.rs"], &[]).unwrap();
    let d = cache.check(&files).unwrap();
    assert_eq!(d, CacheDecision::Run(RunReason::ContentChanged));
}

#[test]
fn glob_with_two_layer() {
    let (src, cache_dir) = setup();
    common::create_file(src.path(), "src/a.rs", "r");
    common::create_file(src.path(), "tests/b.rs", "t");

    let cache = TwoLayerCache::new(cache_dir.path().join("fp.json"));
    let files = walk_files_glob(src.path(), &["src/**"], &[]).unwrap();

    cache.check(&files).unwrap();
    cache.mark_success().unwrap();

    // Modify file outside glob scope — skip.
    common::wait_for_mtime_change();
    common::create_file(src.path(), "tests/b.rs", "changed");
    let files = walk_files_glob(src.path(), &["src/**"], &[]).unwrap();
    let d = cache.check(&files).unwrap();
    assert_eq!(d, CacheDecision::Skip);
}

// ── Invalidate + full cycle ──────────────────────────────────────

#[test]
fn invalidate_then_full_cycle() {
    let (src, cache_dir) = setup();
    common::create_file(src.path(), "a.rs", "a");

    let cache = HashCache::new(cache_dir.path().join("fp.json"));
    let files = walk_files(src.path(), &[], &[]).unwrap();

    cache.check(&files).unwrap();
    cache.mark_success().unwrap();

    cache.invalidate().unwrap();

    let d = cache.check(&files).unwrap();
    assert_eq!(d, CacheDecision::Run(RunReason::NoCacheFile));
    cache.mark_success().unwrap();

    let d = cache.check(&files).unwrap();
    assert_eq!(d, CacheDecision::Skip);
}

// ── Empty file set stability ─────────────────────────────────────

#[test]
fn empty_file_set_stability() {
    let (_src, cache_dir) = setup();
    let empty = TempDir::new().unwrap();

    let cache = HashCache::new(cache_dir.path().join("fp.json"));
    let files = walk_files(empty.path(), &[], &[]).unwrap();
    assert!(files.is_empty());

    cache.check(&files).unwrap();
    cache.mark_success().unwrap();

    let d = cache
        .check(&walk_files(empty.path(), &[], &[]).unwrap())
        .unwrap();
    assert_eq!(d, CacheDecision::Skip);
}
