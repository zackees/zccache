//! Micro-benchmark for the watcher cold-scan metadata-fetch step.
//!
//! `scan_snapshot` in `polling_watcher.rs` walks the tracked tree and
//! calls `path.metadata()` per file. On Windows the per-stat cost is
//! dominated by Defender / antivirus interception (5–20 µs each), so a
//! 10k-file repo can spend 50–200 ms in this step alone on cold start.
//!
//! This bench measures the syscall pattern only (not the jwalk
//! traversal): given N pre-discovered file paths, time
//! `paths.iter().map(metadata)` versus `paths.par_iter().map(metadata)`.
//!
//! Counts: 100, 1000, 5000 — covers tiny / typical / large repos.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::panic_in_result_fn, clippy::unwrap_in_result)]

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion};
use rayon::prelude::*;

const COUNTS: &[usize] = &[100, 1000, 5000];

struct Fixture {
    _tmp: tempfile::TempDir,
    paths: Vec<PathBuf>,
}

impl Fixture {
    fn new(n: usize) -> Self {
        let tmp = tempfile::tempdir().expect("tempdir");
        let mut paths = Vec::with_capacity(n);
        // Spread the files across nested subdirectories so the bench
        // exercises a realistic directory layout (cold-cache stat behavior
        // can differ on the same vs. different directories on NTFS).
        for i in 0..n {
            let sub = tmp.path().join(format!("d{:02}", i % 32));
            std::fs::create_dir_all(&sub).unwrap();
            let p = sub.join(format!("f_{i:05}.rs"));
            std::fs::write(&p, b"// content\n").unwrap();
            paths.push(p);
        }
        Self { _tmp: tmp, paths }
    }
}

#[derive(Debug)]
#[allow(dead_code)]
struct FileState {
    mtime_ns: u128,
    size: u64,
}

fn stat_one(path: &std::path::Path) -> Option<FileState> {
    let m = path.metadata().ok()?;
    Some(FileState {
        mtime_ns: m
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map_or(0, |d| d.as_nanos()),
        size: m.len(),
    })
}

fn stat_serial(fx: &Fixture) -> Vec<FileState> {
    fx.paths.iter().filter_map(|p| stat_one(p)).collect()
}

fn stat_parallel(fx: &Fixture) -> Vec<FileState> {
    fx.paths.par_iter().filter_map(|p| stat_one(p)).collect()
}

fn bench_scan_metadata(c: &mut Criterion) {
    let mut group = c.benchmark_group("scan_metadata");
    // 5000-file warm-up is slow; tighten sample budget so the bench
    // completes in reasonable time on CI hosts.
    group.sample_size(20);

    for &n in COUNTS {
        let fx = Fixture::new(n);
        // Warm the OS cache once so the comparison isn't dominated by
        // cold-cache noise — that's a separate experiment that doesn't
        // discriminate the parallel speedup at all.
        let _warm: Vec<_> = fx.paths.iter().filter_map(|p| p.metadata().ok()).collect();
        let _ = SystemTime::now(); // suppress unused import on some hosts

        group.bench_with_input(BenchmarkId::new("serial", n), &n, |b, _| {
            b.iter(|| {
                let v = stat_serial(black_box(&fx));
                black_box(v);
            });
        });
        group.bench_with_input(BenchmarkId::new("parallel", n), &n, |b, _| {
            b.iter(|| {
                let v = stat_parallel(black_box(&fx));
                black_box(v);
            });
        });
    }
    group.finish();
}

criterion_group!(benches, bench_scan_metadata);
criterion_main!(benches);
