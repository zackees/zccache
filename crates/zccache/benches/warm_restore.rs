//! Micro-benchmark for `zccache warm` restore fan-out.
//!
//! `restore_artifacts` in `crates/zccache-cli/src/main.rs` iterates over
//! cached artifacts and, for each output, does
//! `remove_file (if exists) + hard_link (fallback copy) + open + set_times`.
//! Each entry is independent. CI cache restores can be 1k–5k entries; the
//! per-file syscalls dominate.
//!
//! This bench mirrors that syscall sequence and compares a serial loop
//! against `rayon::par_iter`. Counts: 100 / 1000 / 5000.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::panic_in_result_fn, clippy::unwrap_in_result)]

use std::path::PathBuf;
use std::time::SystemTime;

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion};
use rayon::prelude::*;

const COUNTS: &[usize] = &[100, 1000, 5000];

struct Fixture {
    _tmp: tempfile::TempDir,
    work: Vec<(PathBuf, PathBuf)>,
    file_times: std::fs::FileTimes,
}

impl Fixture {
    fn new(n: usize) -> Self {
        let tmp = tempfile::tempdir().expect("tempdir");
        let cache_dir = tmp.path().join("cache");
        let dst_dir = tmp.path().join("dst");
        std::fs::create_dir_all(&cache_dir).unwrap();
        std::fs::create_dir_all(&dst_dir).unwrap();

        let mut work = Vec::with_capacity(n);
        for i in 0..n {
            let src = cache_dir.join(format!("a_{i:05}"));
            // Small payloads — warm-restore artifacts are typically
            // rlibs/rmetas; the per-file cost is dominated by syscalls
            // (hard_link + open + set_times), not bytes.
            std::fs::write(&src, b"// fake rlib bytes\n").unwrap();
            let dst = dst_dir.join(format!("a_{i:05}.rlib"));
            work.push((src, dst));
        }

        let now = SystemTime::now();
        let file_times = std::fs::FileTimes::new()
            .set_accessed(now)
            .set_modified(now);
        Self {
            _tmp: tmp,
            work,
            file_times,
        }
    }

    fn reset(&self) {
        for (_, dst) in &self.work {
            let _ = std::fs::remove_file(dst);
        }
    }
}

fn restore_one(src: &std::path::Path, dst: &std::path::Path, file_times: std::fs::FileTimes) {
    if dst.exists() {
        let _ = std::fs::remove_file(dst);
    }
    if std::fs::hard_link(src, dst).is_err() {
        let _ = std::fs::copy(src, dst);
    }
    if let Ok(f) = std::fs::File::open(dst) {
        let _ = f.set_times(file_times);
    }
}

fn restore_serial(fx: &Fixture) {
    for (src, dst) in &fx.work {
        restore_one(src, dst, fx.file_times);
    }
}

fn restore_parallel(fx: &Fixture) {
    fx.work.par_iter().for_each(|(src, dst)| {
        restore_one(src, dst, fx.file_times);
    });
}

fn bench_warm_restore(c: &mut Criterion) {
    let mut group = c.benchmark_group("warm_restore");
    group.sample_size(20);

    for &n in COUNTS {
        let fx = Fixture::new(n);
        group.bench_with_input(BenchmarkId::new("serial", n), &n, |b, _| {
            b.iter(|| {
                fx.reset();
                restore_serial(black_box(&fx));
            });
        });
        group.bench_with_input(BenchmarkId::new("parallel", n), &n, |b, _| {
            b.iter(|| {
                fx.reset();
                restore_parallel(black_box(&fx));
            });
        });
    }
    group.finish();
}

criterion_group!(benches, bench_warm_restore);
criterion_main!(benches);
