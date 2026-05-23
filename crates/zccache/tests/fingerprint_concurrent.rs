mod common;

use std::sync::Arc;
use tempfile::TempDir;
use zccache::fingerprint::{walk_files, CacheDecision, HashCache, TwoLayerCache};

// ── Independent caches in parallel (no locking needed) ───────────

#[test]
fn independent_caches_parallel() {
    let src = TempDir::new().unwrap();
    common::create_file(src.path(), "a.rs", "a");

    let src_path = src.path().to_path_buf();

    let handles: Vec<_> = (0..4)
        .map(|i| {
            let src = src_path.clone();
            std::thread::spawn(move || {
                let cache_dir = TempDir::new().unwrap();
                let cache = HashCache::new(cache_dir.path().join(format!("c{i}.json")));
                let files = walk_files(&src, &[], &[]).unwrap();

                let d = cache.check(&files).unwrap();
                assert!(d.should_run());
                cache.mark_success().unwrap();

                let d = cache.check(&walk_files(&src, &[], &[]).unwrap()).unwrap();
                assert_eq!(d, CacheDecision::Skip);
            })
        })
        .collect();

    for h in handles {
        h.join().unwrap();
    }
}

// ── Parallel check on same cache (requires file locking) ─────────

#[test]
#[ignore]
fn parallel_check_same_hash_cache() {
    let src = TempDir::new().unwrap();
    for i in 0..10 {
        common::create_file(src.path(), &format!("f{i}.rs"), &format!("{i}"));
    }

    let cache_dir = TempDir::new().unwrap();
    let cache_path = cache_dir.path().join("shared.json");

    // Initialize cache.
    let cache = HashCache::new(cache_path.clone());
    cache
        .check(&walk_files(src.path(), &[], &[]).unwrap())
        .unwrap();
    cache.mark_success().unwrap();

    let src_path = Arc::new(src.path().to_path_buf());
    let cache_path = Arc::new(cache_path);

    let handles: Vec<_> = (0..3)
        .map(|_| {
            let src = Arc::clone(&src_path);
            let cp = Arc::clone(&cache_path);
            std::thread::spawn(move || {
                let cache = HashCache::new((*cp).clone());
                let files = walk_files(&src, &[], &[]).unwrap();
                let d = cache.check(&files).unwrap();
                assert_eq!(d, CacheDecision::Skip);
            })
        })
        .collect();

    for h in handles {
        h.join().unwrap();
    }
}

#[test]
#[ignore]
fn parallel_check_mark_interleave() {
    let src = TempDir::new().unwrap();
    common::create_file(src.path(), "a.rs", "a");

    let cache_dir = TempDir::new().unwrap();
    let cache_path = cache_dir.path().join("shared.json");

    let src_path = Arc::new(src.path().to_path_buf());
    let cp = Arc::new(cache_path);

    let handles: Vec<_> = (0..3)
        .map(|_| {
            let src = Arc::clone(&src_path);
            let cp = Arc::clone(&cp);
            std::thread::spawn(move || {
                let cache = HashCache::new((*cp).clone());
                let files = walk_files(&src, &[], &[]).unwrap();
                cache.check(&files).unwrap();
                // Small jitter.
                std::thread::sleep(std::time::Duration::from_millis(5));
                cache.mark_success().unwrap();
            })
        })
        .collect();

    for h in handles {
        h.join().unwrap();
    }

    // After all threads: cache should be valid.
    let cache = HashCache::new((*cp).clone());
    let d = cache
        .check(&walk_files(&src_path, &[], &[]).unwrap())
        .unwrap();
    assert_eq!(d, CacheDecision::Skip);
}

#[test]
#[ignore]
fn parallel_invalidate_while_checking() {
    let src = TempDir::new().unwrap();
    common::create_file(src.path(), "a.rs", "a");

    let cache_dir = TempDir::new().unwrap();
    let cache_path = cache_dir.path().join("shared.json");

    // Initialize.
    let cache = HashCache::new(cache_path.clone());
    cache
        .check(&walk_files(src.path(), &[], &[]).unwrap())
        .unwrap();
    cache.mark_success().unwrap();

    let src_path = Arc::new(src.path().to_path_buf());
    let cp = Arc::new(cache_path);

    // One thread invalidates, others check.
    let cp2 = Arc::clone(&cp);
    let invalidator = std::thread::spawn(move || {
        let cache = HashCache::new((*cp2).clone());
        cache.invalidate().unwrap();
    });

    let cp3 = Arc::clone(&cp);
    let src2 = Arc::clone(&src_path);
    let checker = std::thread::spawn(move || {
        let cache = HashCache::new((*cp3).clone());
        let files = walk_files(&src2, &[], &[]).unwrap();
        // May be Skip or Run depending on timing — just shouldn't crash.
        let _d = cache.check(&files).unwrap();
    });

    invalidator.join().unwrap();
    checker.join().unwrap();
}

// ── Parallel TwoLayerCache ──────────────────────────────────────

#[test]
#[ignore]
fn parallel_two_layer_check() {
    let src = TempDir::new().unwrap();
    for i in 0..5 {
        common::create_file(src.path(), &format!("f{i}.rs"), &format!("{i}"));
    }

    let cache_dir = TempDir::new().unwrap();
    let cache_path = cache_dir.path().join("shared.json");

    let cache = TwoLayerCache::new(cache_path.clone());
    cache
        .check(&walk_files(src.path(), &[], &[]).unwrap())
        .unwrap();
    cache.mark_success().unwrap();

    let src_path = Arc::new(src.path().to_path_buf());
    let cp = Arc::new(cache_path);

    let handles: Vec<_> = (0..3)
        .map(|_| {
            let src = Arc::clone(&src_path);
            let cp = Arc::clone(&cp);
            std::thread::spawn(move || {
                let cache = TwoLayerCache::new((*cp).clone());
                let files = walk_files(&src, &[], &[]).unwrap();
                let d = cache.check(&files).unwrap();
                assert_eq!(d, CacheDecision::Skip);
            })
        })
        .collect();

    for h in handles {
        h.join().unwrap();
    }
}
