//! CLI surface and wrapper integration tests (#1526).
//!
//! Covers the `zccache` CLI verbs, the compiler wrappers,
//! installer/distribution shape, and the formatter API. Each former
//! top-level `tests/*.rs` file is a module here, so the category links
//! one test executable instead of 17. Test IDs are
//! `cli::<module>::<test_name>`.
//!
//! Per-module lint allows and `#![cfg(...)]` gates stay as the inner
//! attributes at the top of each file, so nothing is widened to a
//! common denominator.

mod ci_timeout_and_progress;
mod cli_cache_root;
mod cli_cli_crash_test;
mod cli_daemon_start;
mod cli_defender_exclusions;
mod cli_ino_convert;
mod cli_installers;
mod cli_kv;
mod cli_meson_configure_cache;
mod cli_no_spawn_guard;
mod cli_rust_plan_lifecycle;
mod cli_session_end;
mod cli_single_daemon_per_session;
mod cli_wrapper_failure_boundaries;
mod cli_wrapper_passthrough;
mod formatter_api;
mod single_binary_distribution;
