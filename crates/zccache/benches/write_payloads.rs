//! Micro-benchmark for cache-hit payload write fan-out.
//!
//! Measures the speedup of writing N cached output files in parallel
//! (`rayon::par_iter`) versus serially (a plain `for` loop). Both
//! implementations call `std::fs::hard_link` with `std::fs::write` as the
//! fallback — the same syscall sequence the daemon uses in
//! `write_cached_output` (see crates/zccache-daemon/src/server.rs).
//!
//! - N = 1: single-output `.o` compile (the common case, regression guard)
//! - N = 3: MSVC link with `.lib` + `.exp` secondary outputs
//! - N = 5: link with a couple of side-effect DLLs
//! - N = 10: link saturated with side-effects (MAX_SIDE_EFFECT_COUNT)
//!
//! Each payload is a 1 MiB blob — large enough to stress the kernel's
//! hardlink/copy fast path without dominating measurement noise from
//! filesystem flushing.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::panic_in_result_fn, clippy::unwrap_in_result)]

use std::path::PathBuf;

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion};
use rayon::prelude::*;

const PAYLOAD_SIZE: usize = 1024 * 1024; // 1 MiB
const COUNTS: &[usize] = &[1, 3, 5, 10];

struct Fixture {
    _tmp: tempfile::TempDir,
    cache_files: Vec<PathBuf>,
    out_paths: Vec<PathBuf>,
}

impl Fixture {
    fn new(n: usize) -> Self {
        let tmp = tempfile::tempdir().expect("tempdir");
        let cache_dir = tmp.path().join("cache");
        let out_dir = tmp.path().join("out");
        std::fs::create_dir_all(&cache_dir).unwrap();
        std::fs::create_dir_all(&out_dir).unwrap();
        let payload = vec![0x42u8; PAYLOAD_SIZE];
        let cache_files: Vec<PathBuf> = (0..n)
            .map(|i| {
                let p = cache_dir.join(format!("payload_{i}"));
                std::fs::write(&p, &payload).unwrap();
                p
            })
            .collect();
        let out_paths: Vec<PathBuf> = (0..n).map(|i| out_dir.join(format!("out_{i}"))).collect();
        Self {
            _tmp: tmp,
            cache_files,
            out_paths,
        }
    }

    fn reset_outputs(&self) {
        for p in &self.out_paths {
            let _ = std::fs::remove_file(p);
        }
    }
}

/// Mirrors the production `write_cached_output` syscall sequence:
/// 1. hardlink, 2. remove + retry hardlink, 3. fallback to copy.
fn write_one(cache: &std::path::Path, out: &std::path::Path) -> std::io::Result<()> {
    if std::fs::hard_link(cache, out).is_ok() {
        return Ok(());
    }
    let _ = std::fs::remove_file(out);
    if std::fs::hard_link(cache, out).is_ok() {
        return Ok(());
    }
    std::fs::copy(cache, out).map(|_| ())
}

fn write_serial(fx: &Fixture) {
    for (cache, out) in fx.cache_files.iter().zip(fx.out_paths.iter()) {
        write_one(cache, out).unwrap();
    }
}

fn write_parallel(fx: &Fixture) {
    fx.cache_files
        .par_iter()
        .zip(fx.out_paths.par_iter())
        .for_each(|(cache, out)| {
            write_one(cache, out).unwrap();
        });
}

fn bench_write_payloads(c: &mut Criterion) {
    let mut group = c.benchmark_group("write_payloads");
    for &n in COUNTS {
        let fx = Fixture::new(n);
        group.bench_with_input(BenchmarkId::new("serial", n), &n, |b, _| {
            b.iter(|| {
                fx.reset_outputs();
                write_serial(black_box(&fx));
            });
        });
        group.bench_with_input(BenchmarkId::new("parallel", n), &n, |b, _| {
            b.iter(|| {
                fx.reset_outputs();
                write_parallel(black_box(&fx));
            });
        });
    }
    group.finish();
}

criterion_group!(benches, bench_write_payloads);
criterion_main!(benches);
