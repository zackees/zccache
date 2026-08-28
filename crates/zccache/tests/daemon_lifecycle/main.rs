//! Daemon lifecycle integration tests (#1526).
//!
//! Covers start/stop, cwd release, exe overwrite, stdio detach, spawn
//! budgets and storms, crash minidumps, and the embedded service. Each
//! former top-level `tests/*.rs` file is a module here, so the category
//! links one test executable instead of 11. Test IDs are
//! `daemon_lifecycle::<module>::<test_name>`.
//!
//! Per-module lint allows and `#![cfg(...)]` gates stay as the inner
//! attributes at the top of each file, so nothing is widened to a
//! common denominator.

mod daemon_cli_flow_test;
mod daemon_crash_minidump_test;
mod daemon_cwd_release;
mod daemon_exe_overwrite;
mod daemon_integration_test;
mod daemon_session_stats_test;
mod daemon_spawn_lockfile_budget_test;
mod daemon_spawn_storm_test;
mod daemon_stdio_detach;
mod daemon_tokio_console_test;
mod embedded_service_test;
