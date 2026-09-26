//! Perf unit test (#1649): a compiler-wrapper process carries no steady CPU
//! cost of its own.
//!
//! kernal-api's native crash capture runs an all-thread pre-crash sampler that
//! takes a full resolved snapshot every 50 ms (about 300 ms of CPU each) and
//! joins an in-flight capture on exit. With it armed in the wrapper, every
//! concurrent compile carried a sampler burning most of a core: a passthrough
//! `zccache <tool>` cost 358 ms instead of 41 ms (release build), cargo's
//! `rustc -vV` probes made the benchmark's warm-target build 240 ms against a
//! 173 ms bare baseline, and a cold `cargo build -p zccache -j4` took
//! 131-146 s instead of 41 s.
//!
//! The wrapped tool is the `zccache` binary's own `--version`, so the test
//! needs no compiler, and `ZCCACHE_DISABLE=1` keeps the daemon out of it: the
//! measurement is the wrapper process alone.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::panic_in_result_fn,
    clippy::unwrap_in_result
)]

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// About 3x the unoptimized post-fix wrapper cost on a quiet host, and well
/// under the ~350 ms a single joined sampler capture adds.
const WRAPPER_PASSTHROUGH_BUDGET: Duration = Duration::from_millis(250);
const RUNS: usize = 7;

fn run_passthrough(zccache: &str, cache_dir: &std::path::Path) -> Duration {
    let started = Instant::now();
    let status = Command::new(zccache)
        .arg(zccache)
        .arg("--version")
        .env("ZCCACHE_DISABLE", "1")
        .env("ZCCACHE_CACHE_DIR", cache_dir)
        .env_remove("ZCCACHE_SESSION_ID")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("spawn zccache wrapper");
    let elapsed = started.elapsed();
    assert!(status.success(), "wrapper passthrough failed: {status:?}");
    elapsed
}

#[test]
fn compiler_wrapper_passthrough_stays_within_budget() {
    let zccache = env!("CARGO_BIN_EXE_zccache");
    let cache_dir = tempfile::tempdir().unwrap();
    // Warm the page cache and the binary's own startup state once.
    run_passthrough(zccache, cache_dir.path());
    let mut samples: Vec<Duration> = (0..RUNS)
        .map(|_| run_passthrough(zccache, cache_dir.path()))
        .collect();
    samples.sort();
    let median = samples[RUNS / 2];
    eprintln!("wrapper passthrough samples: {samples:?}");
    assert!(
        median <= WRAPPER_PASSTHROUGH_BUDGET,
        "median wrapper passthrough {median:?} exceeds {WRAPPER_PASSTHROUGH_BUDGET:?}; \
         a steady per-process cost is back on the compile hot path (#1649): {samples:?}"
    );
}
