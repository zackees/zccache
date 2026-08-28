//! Performance and profiling integration tests (#1526).
//!
//! Covers artifact fallback, persist-pool and heap benchmarks, the
//! profile matrices, and the inner trace file. Each former top-level
//! `tests/*.rs` file is a module here, so the category links one test
//! executable instead of 10. Test IDs are `perf::<module>::<test_name>`.
//!
//! Per-module lint allows and `#![cfg(...)]` gates stay as the inner
//! attributes at the top of each file, so nothing is widened to a
//! common denominator.

mod artifact_perf_index_durability_test;
mod daemon_cold_path_profile_test;
mod daemon_perf_artifact_fallback_test;
mod daemon_perf_test;
mod daemon_persist_pool_bench;
mod daemon_profile_multi_test;
mod daemon_profile_test;
mod fscache_persistence_perf_test;
mod heap_profile_test;
mod inner_trace_file_test;
