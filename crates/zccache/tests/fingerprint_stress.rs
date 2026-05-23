mod common;

use tempfile::TempDir;
use zccache::fingerprint::{walk_files, walk_files_glob, CacheDecision, HashCache, TwoLayerCache};

fn setup() -> (TempDir, TempDir) {
    (TempDir::new().unwrap(), TempDir::new().unwrap())
}

// ── Rapid cycles ─────────────────────────────────────────────────

#[test]
#[ignore]
fn rapid_cycles_hash_cache() {
    let (src, cache_dir) = setup();
    for i in 0..10 {
        common::create_file(src.path(), &format!("f{i}.rs"), &format!("init{i}"));
    }

    let cache = HashCache::new(cache_dir.path().join("fp.json"));

    for cycle in 0..20 {
        let files = walk_files(src.path(), &[], &[]).unwrap();
        let d = cache.check(&files).unwrap();
        if cycle == 0 {
            assert!(d.should_run());
        }
        cache.mark_success().unwrap();

        // Modify one file per cycle.
        common::create_file(
            src.path(),
            &format!("f{}.rs", cycle % 10),
            &format!("cycle{cycle}"),
        );
    }
}

#[test]
#[ignore]
fn rapid_cycles_two_layer() {
    let (src, cache_dir) = setup();
    for i in 0..10 {
        common::create_file(src.path(), &format!("f{i}.rs"), &format!("init{i}"));
    }

    let cache = TwoLayerCache::new(cache_dir.path().join("fp.json"));

    for cycle in 0..20 {
        common::wait_for_mtime_change();
        let files = walk_files(src.path(), &[], &[]).unwrap();
        let d = cache.check(&files).unwrap();
        if cycle == 0 {
            assert!(d.should_run());
        }
        cache.mark_success().unwrap();

        common::create_file(
            src.path(),
            &format!("f{}.rs", cycle % 10),
            &format!("cycle{cycle}"),
        );
    }
}

// ── Large file sets ──────────────────────────────────────────────

#[test]
#[ignore]
fn large_file_set_hash_cache() {
    let (src, cache_dir) = setup();
    for i in 0..150 {
        common::create_file(
            src.path(),
            &format!("src/mod_{i:03}.rs"),
            &format!("content {i}"),
        );
    }

    let cache = HashCache::new(cache_dir.path().join("fp.json"));
    let files = walk_files(src.path(), &[], &[]).unwrap();
    assert_eq!(files.len(), 150);

    cache.check(&files).unwrap();
    cache.mark_success().unwrap();

    let d = cache
        .check(&walk_files(src.path(), &[], &[]).unwrap())
        .unwrap();
    assert_eq!(d, CacheDecision::Skip);
}

#[test]
#[ignore]
fn large_file_set_two_layer() {
    let (src, cache_dir) = setup();
    for i in 0..150 {
        common::create_file(
            src.path(),
            &format!("src/mod_{i:03}.rs"),
            &format!("content {i}"),
        );
    }

    let cache = TwoLayerCache::new(cache_dir.path().join("fp.json"));
    let files = walk_files(src.path(), &[], &[]).unwrap();
    assert_eq!(files.len(), 150);

    cache.check(&files).unwrap();
    cache.mark_success().unwrap();

    let d = cache
        .check(&walk_files(src.path(), &[], &[]).unwrap())
        .unwrap();
    assert_eq!(d, CacheDecision::Skip);
}

// ── Large files ──────────────────────────────────────────────────

#[test]
#[ignore]
fn large_files_hash_cache() {
    let (src, cache_dir) = setup();
    for i in 0..5 {
        let content = "x".repeat(10 * 1024); // 10KB
        common::create_file(src.path(), &format!("big_{i}.bin"), &content);
    }

    let cache = HashCache::new(cache_dir.path().join("fp.json"));
    cache
        .check(&walk_files(src.path(), &[], &[]).unwrap())
        .unwrap();
    cache.mark_success().unwrap();

    let d = cache
        .check(&walk_files(src.path(), &[], &[]).unwrap())
        .unwrap();
    assert_eq!(d, CacheDecision::Skip);
}

// ── Many glob patterns ──────────────────────────────────────────

#[test]
#[ignore]
fn many_glob_patterns() {
    let (src, _cache_dir) = setup();
    for i in 0..50 {
        common::create_file(src.path(), &format!("dir_{i:02}/file.rs"), &format!("{i}"));
    }

    let include: Vec<String> = (0..50).map(|i| format!("dir_{i:02}/**")).collect();
    let include_refs: Vec<&str> = include.iter().map(|s| s.as_str()).collect();
    let exclude: Vec<&str> = vec!["dir_00/**", "dir_01/**"];

    let files = walk_files_glob(src.path(), &include_refs, &exclude).unwrap();
    assert_eq!(files.len(), 48);
}

// ── Mixed modifications ─────────────────────────────────────────

#[test]
#[ignore]
fn mixed_modifications_many_cycles() {
    let (src, cache_dir) = setup();
    for i in 0..20 {
        common::create_file(src.path(), &format!("f{i}.rs"), &format!("v0_{i}"));
    }

    let cache = HashCache::new(cache_dir.path().join("fp.json"));

    for cycle in 0..20 {
        let files = walk_files(src.path(), &[], &[]).unwrap();
        cache.check(&files).unwrap();
        cache.mark_success().unwrap();

        // Alternate: edit, add, remove.
        match cycle % 3 {
            0 => {
                common::create_file(
                    src.path(),
                    &format!("f{}.rs", cycle % 20),
                    &format!("edited_{cycle}"),
                );
            }
            1 => {
                common::create_file(
                    src.path(),
                    &format!("new_{cycle}.rs"),
                    &format!("added_{cycle}"),
                );
            }
            2 => {
                let target = src.path().join(format!("f{}.rs", cycle % 20));
                if target.exists() {
                    std::fs::remove_file(target).unwrap();
                }
            }
            _ => unreachable!(),
        }
    }
}
