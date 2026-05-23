use std::path::Path;

use criterion::{criterion_group, criterion_main, Criterion};
use tempfile::TempDir;
use zccache_fingerprint::{walk_files, walk_files_glob, HashCache, TwoLayerCache};

/// Create a synthetic directory tree with `n` files.
fn create_synthetic_tree(dir: &Path, n: usize) {
    // 10 subdirectories
    let dirs = 10;
    for d in 0..dirs {
        let sub = dir.join(format!("dir_{d:02}"));
        std::fs::create_dir_all(&sub).unwrap();
        let files_per_dir = n / dirs;
        for f in 0..files_per_dir {
            let idx = d * files_per_dir + f;
            let path = sub.join(format!("file_{idx:04}.cpp"));
            std::fs::write(
                &path,
                format!("// content {idx}\nint f{idx}() {{ return {idx}; }}\n"),
            )
            .unwrap();
        }
    }
    // Handle remainder
    let remainder = n - (n / dirs) * dirs;
    if remainder > 0 {
        let sub = dir.join("dir_extra");
        std::fs::create_dir_all(&sub).unwrap();
        for f in 0..remainder {
            let idx = n - remainder + f;
            let path = sub.join(format!("file_{idx:04}.cpp"));
            std::fs::write(
                &path,
                format!("// content {idx}\nint f{idx}() {{ return {idx}; }}\n"),
            )
            .unwrap();
        }
    }
}

fn bench_walk_files(c: &mut Criterion) {
    let src = TempDir::new().unwrap();
    create_synthetic_tree(src.path(), 1000);

    c.bench_function("walk_files_1000", |b| {
        b.iter(|| {
            let files = walk_files(src.path(), &["cpp"], &[]).unwrap();
            assert_eq!(files.len(), 1000);
        });
    });
}

fn bench_walk_files_glob(c: &mut Criterion) {
    let src = TempDir::new().unwrap();
    create_synthetic_tree(src.path(), 1000);

    c.bench_function("walk_files_glob_1000", |b| {
        b.iter(|| {
            let files = walk_files_glob(src.path(), &["**/*.cpp"], &[]).unwrap();
            assert_eq!(files.len(), 1000);
        });
    });
}

fn bench_two_layer_miss(c: &mut Criterion) {
    let src = TempDir::new().unwrap();
    let cache_dir = TempDir::new().unwrap();
    create_synthetic_tree(src.path(), 1000);
    let files = walk_files(src.path(), &["cpp"], &[]).unwrap();

    c.bench_function("two_layer_miss_1000", |b| {
        b.iter(|| {
            let cache_file = cache_dir.path().join("two_layer.json");
            let _ = std::fs::remove_file(&cache_file);
            let _ = std::fs::remove_file(cache_file.with_extension("pending"));
            let cache = TwoLayerCache::new(cache_file);
            let decision = cache.check(&files).unwrap();
            assert!(decision.should_run());
        });
    });
}

fn bench_two_layer_hit(c: &mut Criterion) {
    let src = TempDir::new().unwrap();
    let cache_dir = TempDir::new().unwrap();
    create_synthetic_tree(src.path(), 1000);
    let files = walk_files(src.path(), &["cpp"], &[]).unwrap();

    // Warm up: first check + mark success
    let cache_file = cache_dir.path().join("two_layer.json");
    let cache = TwoLayerCache::new(cache_file.clone());
    cache.check(&files).unwrap();
    cache.mark_success().unwrap();

    c.bench_function("two_layer_hit_1000", |b| {
        b.iter(|| {
            let cache = TwoLayerCache::new(cache_file.clone());
            let decision = cache.check(&files).unwrap();
            assert!(decision.should_skip());
        });
    });
}

fn bench_hash_cache_miss(c: &mut Criterion) {
    let src = TempDir::new().unwrap();
    let cache_dir = TempDir::new().unwrap();
    create_synthetic_tree(src.path(), 1000);
    let files = walk_files(src.path(), &["cpp"], &[]).unwrap();

    c.bench_function("hash_cache_miss_1000", |b| {
        b.iter(|| {
            let cache_file = cache_dir.path().join("hash.json");
            let _ = std::fs::remove_file(&cache_file);
            let _ = std::fs::remove_file(cache_file.with_extension("pending"));
            let cache = HashCache::new(cache_file);
            let decision = cache.check(&files).unwrap();
            assert!(decision.should_run());
        });
    });
}

fn bench_hash_cache_hit(c: &mut Criterion) {
    let src = TempDir::new().unwrap();
    let cache_dir = TempDir::new().unwrap();
    create_synthetic_tree(src.path(), 1000);
    let files = walk_files(src.path(), &["cpp"], &[]).unwrap();

    // Warm up
    let cache_file = cache_dir.path().join("hash.json");
    let cache = HashCache::new(cache_file.clone());
    cache.check(&files).unwrap();
    cache.mark_success().unwrap();

    c.bench_function("hash_cache_hit_1000", |b| {
        b.iter(|| {
            let cache = HashCache::new(cache_file.clone());
            let decision = cache.check(&files).unwrap();
            assert!(decision.should_skip());
        });
    });
}

fn bench_two_layer_fast_path(c: &mut Criterion) {
    let src = TempDir::new().unwrap();
    let cache_dir = TempDir::new().unwrap();
    create_synthetic_tree(src.path(), 1000);
    let files = walk_files(src.path(), &["cpp"], &[]).unwrap();

    // Warm up: first check + mark success
    let cache_file = cache_dir.path().join("two_layer.json");
    let cache = TwoLayerCache::new(cache_file.clone());
    cache.check(&files).unwrap();
    cache.mark_success().unwrap();

    c.bench_function("two_layer_fast_path_1000", |b| {
        b.iter(|| {
            let cache = TwoLayerCache::new(cache_file.clone());
            let decision = cache.try_skip_fast(src.path()).unwrap();
            assert_eq!(decision, Some(zccache_fingerprint::CacheDecision::Skip));
        });
    });
}

criterion_group!(
    benches,
    bench_walk_files,
    bench_walk_files_glob,
    bench_two_layer_miss,
    bench_two_layer_hit,
    bench_hash_cache_miss,
    bench_hash_cache_hit,
    bench_two_layer_fast_path,
);
criterion_main!(benches);
