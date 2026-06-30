//! Regression test for `trampoline::release_cwd()`.
//!
//! Verifies that after the daemon's launch path calls `release_cwd()`,
//! the process is no longer holding an implicit handle on its launch
//! cwd: the cwd has moved off the temporary directory and dropping the
//! `TempDir` cleans up successfully (on Windows this would have failed
//! before the fix).
//!
//! Threads within a test binary share process-wide cwd, so even though
//! cargo runs test binaries in parallel, multiple tests inside *this*
//! binary must serialize. Guarded by a local static `Mutex` to avoid
//! pulling in `serial_test` as a dependency.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::panic_in_result_fn,
    clippy::unwrap_in_result
)]

use std::sync::Mutex;

static CWD_LOCK: Mutex<()> = Mutex::new(());

#[test]
fn release_cwd_moves_off_tempdir() {
    let _guard = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());

    let dir = tempfile::tempdir().expect("create tempdir");
    let dir_canon = dir
        .path()
        .canonicalize()
        .expect("canonicalize tempdir path");

    std::env::set_current_dir(&dir_canon).expect("chdir into tempdir");

    zccache::daemon::trampoline::release_cwd();

    let now = std::env::current_dir().expect("read current_dir after release");
    let now_canon = now.canonicalize().expect("canonicalize current_dir");
    assert_ne!(
        now_canon, dir_canon,
        "release_cwd() should have moved cwd off the launch tempdir"
    );

    // Drop the TempDir last; on Windows this would have failed before
    // the fix because the process held an implicit handle on `dir`.
    drop(dir);
}
