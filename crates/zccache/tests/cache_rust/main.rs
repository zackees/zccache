//! Rust-toolchain cache integration tests (#1526).
//!
//! Covers the rustc cache (basic, restore, worktree, path remap, async
//! populate), its adversarial matrices, the dylint cache, and the
//! copy-on-write cargo contracts. Each former top-level `tests/*.rs` file
//! is a module here, so the category links one test executable instead of
//! 12. Test IDs are `cache_rust::<module>::<test_name>`.
//!
//! Per-module lint allows and `#![cfg(...)]` gates stay as the inner
//! attributes at the top of each file, so nothing is widened to a
//! common denominator.

mod cow_mediated_contracts;
mod cow_readonly_cargo;
mod daemon_dylint_cache_test;
mod daemon_rustc_adversarial_concurrency_test;
mod daemon_rustc_adversarial_corner_cases_test;
mod daemon_rustc_adversarial_mutations_test;
mod daemon_rustc_cache_basic_test;
mod daemon_rustc_cache_path_remap_test;
mod daemon_rustc_cache_worktree_test;
mod daemon_rustc_issue_210_async_populate_test;
mod daemon_rustc_restore_test;
mod daemon_workspace_pin_747;
