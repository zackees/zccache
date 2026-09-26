//! Criterion bench for per-TU cache-miss overhead (zccache#1670).
//!
//! Each iteration compiles a never-seen C++ translation unit through an
//! in-process daemon; the reported time is the request's wall time minus
//! the compiler child's own run time, i.e. what zccache adds to a cold
//! compile (include scan or depfile parse, blake3 hashing, staged store,
//! index enqueue). The fixture lives in
//! `zccache_daemon_core::test_support::miss_overhead` and is shared with
//! the CI budget test `miss_overhead_budget`.
//!
//! Run with: `soldr cargo bench -p zccache --bench miss_overhead`.
//! Needs `clang++` on PATH (or clang-tool-chain); skips otherwise.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::panic_in_result_fn,
    clippy::unwrap_in_result
)]

use std::time::Duration;

use criterion::{criterion_group, criterion_main, Criterion};
use zccache::test_support::miss_overhead::{DependencySource, MissOverheadFixture};

fn bench_miss_overhead(c: &mut Criterion) {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    let mut group = c.benchmark_group("miss_overhead");
    group
        .sample_size(20)
        .measurement_time(Duration::from_secs(10));
    for (name, source) in [
        ("include_scan", DependencySource::IncludeScan),
        ("depfile", DependencySource::Depfile),
    ] {
        let Some(mut fixture) = rt.block_on(MissOverheadFixture::start(source)) else {
            eprintln!("clang++ not found; skipping miss_overhead benches");
            return;
        };
        rt.block_on(fixture.measure(3));
        group.bench_function(name, |b| {
            b.iter_custom(|iters| rt.block_on(fixture.measure(iters)).overhead());
        });
        rt.block_on(fixture.shutdown());
    }
    group.finish();
}

criterion_group!(benches, bench_miss_overhead);
criterion_main!(benches);
