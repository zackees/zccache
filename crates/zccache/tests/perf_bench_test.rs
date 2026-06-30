//! Performance benchmark: warm-cache compilation latency.
//!
//! This is a thin shim. The implementation has been split across the
//! `perf_bench/` directory so each file stays under the 1000-LOC limit
//! while still being discoverable as a single `--test perf_bench_test`
//! test binary by cargo.
//!
//! See [`perf_bench/README.md`](perf_bench/README.md) for the module map.
//!
//! Run with: soldr cargo test -p zccache-daemon --test perf_bench_test -- --nocapture --ignored

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::panic_in_result_fn, clippy::unwrap_in_result)]

#[path = "perf_bench/mod.rs"]
mod perf_bench;
