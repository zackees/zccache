//! Micro-benchmark for link cache-populate read fan-out.
//!
//! On a successful link, `handle_link_ephemeral` (crates/zccache-daemon/src/server.rs)
//! reads the primary output, every secondary output (e.g., MSVC `.lib`/`.exp`),
//! and every detected side-effect file. Those reads currently run serially
//! before the daemon returns `LinkResult` to the client. This bench measures
//! the speedup of doing them in parallel via `rayon::par_iter`.
//!
//! Sizing follows the same matrix as `write_payloads.rs`:
//! - N = 1: regression guard for single-output paths
//! - N = 3: MSVC link with `.lib` + `.exp`
//! - N = 5: link with a couple of side-effect DLLs
//! - N = 10: link saturated with side-effects

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::panic_in_result_fn,
    clippy::unwrap_in_result
)]

use std::path::PathBuf;

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion};
use rayon::prelude::*;

const PAYLOAD_SIZE: usize = 1024 * 1024; // 1 MiB
const COUNTS: &[usize] = &[1, 3, 5, 10];

struct Fixture {
    _tmp: tempfile::TempDir,
    paths: Vec<PathBuf>,
}

impl Fixture {
    fn new(n: usize) -> Self {
        let tmp = tempfile::tempdir().expect("tempdir");
        let payload = vec![0x42u8; PAYLOAD_SIZE];
        let paths: Vec<PathBuf> = (0..n)
            .map(|i| {
                let p = tmp.path().join(format!("out_{i}"));
                std::fs::write(&p, &payload).unwrap();
                p
            })
            .collect();
        Self { _tmp: tmp, paths }
    }
}

fn read_serial(fx: &Fixture) -> Vec<Vec<u8>> {
    fx.paths.iter().map(|p| std::fs::read(p).unwrap()).collect()
}

fn read_parallel(fx: &Fixture) -> Vec<Vec<u8>> {
    fx.paths
        .par_iter()
        .map(|p| std::fs::read(p).unwrap())
        .collect()
}

fn bench_read_outputs(c: &mut Criterion) {
    let mut group = c.benchmark_group("read_outputs");
    for &n in COUNTS {
        let fx = Fixture::new(n);
        group.bench_with_input(BenchmarkId::new("serial", n), &n, |b, _| {
            b.iter(|| {
                let v = read_serial(black_box(&fx));
                black_box(v);
            });
        });
        group.bench_with_input(BenchmarkId::new("parallel", n), &n, |b, _| {
            b.iter(|| {
                let v = read_parallel(black_box(&fx));
                black_box(v);
            });
        });
    }
    group.finish();
}

criterion_group!(benches, bench_read_outputs);
criterion_main!(benches);
