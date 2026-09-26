//! CI budget for per-TU cache-miss overhead (zccache#1670).
//!
//! Uses `test_support::miss_overhead`, the same fixture as the
//! `miss_overhead` criterion bench. Run by the `miss-overhead` job in
//! `.github/workflows/ci.yml`, which has `clang++`:
//! `soldr cargo test -p zccache-daemon-core --features test-support --lib
//! miss_overhead_budget -- --ignored --nocapture`.

use std::time::Duration;

use crate::test_support::miss_overhead::{DependencySource, MissOverheadFixture};

/// Warm-up compiles (daemon caches, system-include probe) before measuring.
const WARMUP: u64 = 3;
/// Compiles per measured batch; the fastest of `BATCHES` is checked.
const PER_BATCH: u64 = 12;
const BATCHES: usize = 3;
/// Per-TU overhead ceiling for this unoptimized test build on a hosted
/// runner. Measured ~5 ms/TU locally (clang++ itself ~23 ms/TU); the
/// headroom absorbs slower CI machines while still catching a miss-path
/// regression like FastLED/fbuild#1464, which cost tens of ms per TU.
const MISS_OVERHEAD_BUDGET: Duration = Duration::from_millis(20);

async fn best_overhead(source: DependencySource) -> Option<Duration> {
    let mut fixture = MissOverheadFixture::start(source).await?;
    fixture.measure(WARMUP).await;
    let mut best = Duration::MAX;
    for _ in 0..BATCHES {
        let sample = fixture.measure(PER_BATCH).await;
        eprintln!(
            "{source:?}: wall {:?} compiler {:?} overhead/TU {:?}",
            sample.wall / PER_BATCH as u32,
            sample.compiler_process / PER_BATCH as u32,
            sample.overhead_per_compile()
        );
        best = best.min(sample.overhead_per_compile());
    }
    fixture.shutdown().await;
    Some(best)
}

async fn assert_budget(source: DependencySource, budget: Duration) {
    let Some(overhead) = best_overhead(source).await else {
        eprintln!("clang++ not found; skipping {source:?} miss-overhead budget");
        return;
    };
    assert!(
        overhead <= budget,
        "{source:?} miss overhead {overhead:?}/TU exceeds the {budget:?} budget"
    );
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs clang++; run by the ci.yml miss-overhead job (zccache#1670)"]
async fn miss_overhead_budget_include_scan() {
    assert_budget(DependencySource::IncludeScan, MISS_OVERHEAD_BUDGET).await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs clang++; run by the ci.yml miss-overhead job (zccache#1670)"]
async fn miss_overhead_budget_depfile() {
    assert_budget(DependencySource::Depfile, MISS_OVERHEAD_BUDGET).await;
}
