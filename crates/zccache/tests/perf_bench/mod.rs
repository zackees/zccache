//! Performance benchmark: warm-cache compilation latency.
//!
//! Twelve `#[ignore]` perf benchmarks plus one always-on regression test.
//! The test functions are split across submodules but cargo discovers them all
//! under the same `perf_bench_test` test binary (the thin shim
//! `tests/perf_bench_test.rs` re-exports this module).
//!
//! Run with: soldr cargo test -p zccache-daemon --test perf_bench_test -- --nocapture --ignored

// Shared helpers (constants, timing, daemon boot, tool finders, etc.)
mod common;

// Project-generation + per-language compile helpers
mod c_project;
mod cpp_project;
mod response_file;
mod rust_project;

// Cross-cutting helpers used by multiple test modules
mod link;
mod sibling_remap;

// Test modules — each declares one or more `#[tokio::test] #[ignore]`
// perf benchmarks. Names are preserved across the split so the canonical
// invocation `soldr cargo test ... --test perf_bench_test -- <name>` still
// works.
mod tests_c;
mod tests_cpp;
mod tests_emcc;
mod tests_link;
mod tests_response_file;
mod tests_rust;
mod tests_sibling_remap;
