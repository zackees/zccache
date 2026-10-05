//! Daemon lifecycle integration tests (#1526).
//!
//! Covers start/stop, cwd release, exe overwrite, stdio detach, spawn
//! budgets and storms, crash minidumps, endpoint-scoped ownership records,
//! and the embedded service. Each former top-level `tests/*.rs` file is a
//! module here, so the category links one test executable. Test IDs are
//! `daemon_lifecycle::<module>::<test_name>`.
//!
//! Per-module lint allows and `#![cfg(...)]` gates stay as the inner
//! attributes at the top of each file, so nothing is widened to a
//! common denominator.

mod daemon_cli_flow_test;
mod daemon_crash_minidump_test;
mod daemon_cwd_release;
mod daemon_endpoint_scoped_lock_test;
mod daemon_exe_overwrite;
mod daemon_idle_cpu_budget_test;
mod daemon_integration_test;
mod daemon_lockfile_ownership_test;
mod daemon_session_stats_test;
mod daemon_spawn_lockfile_budget_test;
mod daemon_spawn_storm_test;
mod daemon_stdio_detach;
mod daemon_tokio_console_test;
mod embedded_service_test;

/// Serializes the process-global `ZCCACHE_CACHE_DIR` / `ZCCACHE_DAEMON_NAMESPACE`
/// swaps the lockfile fixtures perform to resolve the daemon's lock path.
///
/// `zccache::ipc::lock_file_path()` derives its path from the environment
/// rather than taking arguments, so a fixture must mutate env to compute the
/// path it is going to assert on. Two such fixtures running concurrently in
/// this one binary would read each other's cache root, and the second one's
/// "the daemon wrote / did not write this lock file" assertion would be made
/// against the wrong path. One lock for every fixture in this binary.
pub(crate) static LOCKFILE_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
