//! Adversarial and stress integration tests (#1526).
//!
//! Covers compiler, correctness, edge and watcher stress, the adversarial
//! corner-case and mutation matrices, KV stress, and compile-concurrency
//! gating. Each former top-level `tests/*.rs` file is a module here, so
//! the category links one test executable instead of 9. Test IDs are
//! `stress::<module>::<test_name>`.
//!
//! Per-module lint allows and `#![cfg(...)]` gates stay as the inner
//! attributes at the top of each file, so nothing is widened to a
//! common denominator.

mod artifact_kv_stress;
mod compile_concurrency_gating;
mod daemon_adversarial_corner_cases;
mod daemon_adversarial_mutations;
mod daemon_stress_compiler_test;
mod daemon_stress_correctness_test;
mod daemon_stress_edges_test;
mod daemon_watcher_adversarial;
mod watcher_stress_test;
