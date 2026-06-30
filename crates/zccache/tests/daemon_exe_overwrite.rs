//! Regression test for `trampoline::unlock_exe()`.
//!
//! On Windows the OS file-locks a running executable, so without
//! `unlock_exe()` `pip install --upgrade zccache` would fail to overwrite
//! `Scripts/zccache-daemon.exe` while the daemon is alive. `unlock_exe()`
//! sidesteps the lock by renaming the canonical path to
//! `zccache-daemon.exe.old.<rand>` and copying back; the running process
//! keeps executing from the renamed file and the canonical path is now an
//! unlocked copy.
//!
//! This test proves the lock was actually lifted by trying to overwrite
//! the canonical path while the daemon is running.
//!
//! Windows-only: on Unix, running binaries can be replaced freely so the
//! test would pass without proving anything.
//!
//! See issue #134 / PR #135.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::panic_in_result_fn, clippy::unwrap_in_result)]

#![cfg(windows)]

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[test]
fn daemon_exe_path_is_overwritable_while_running() {
    let daemon_src = env!("CARGO_BIN_EXE_zccache-daemon");
    let tmp = tempfile::tempdir().expect("create tempdir");
    let dest = tmp.path().join("zccache-daemon.exe");
    std::fs::copy(daemon_src, &dest).expect("copy daemon binary into tempdir");

    let endpoint = zccache::ipc::unique_test_endpoint();
    let cache_dir = tmp.path().join("cache");
    std::fs::create_dir_all(&cache_dir).expect("create per-test cache dir");

    let mut child = Command::new(&dest)
        .args(["--foreground", "--endpoint", &endpoint])
        .env("ZCCACHE_CACHE_DIR", &cache_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn daemon");

    // Poll until the canonical path is overwritable. unlock_exe() runs
    // synchronously at the top of main(), but the daemon hasn't necessarily
    // reached it yet at the instant spawn() returns. If the daemon ever
    // exits before the overwrite succeeds, fail loudly — that would mean
    // the file is overwritable for the wrong reason.
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut last_err: Option<std::io::Error> = None;
    let overwrote = loop {
        if let Some(status) = child.try_wait().expect("query daemon child") {
            let _ = child.wait();
            panic!("daemon exited prematurely (status {status:?}); last write error: {last_err:?}");
        }
        match std::fs::write(&dest, b"replaced") {
            Ok(()) => break true,
            Err(e) => last_err = Some(e),
        }
        if Instant::now() >= deadline {
            break false;
        }
        std::thread::sleep(Duration::from_millis(50));
    };

    // The daemon is running from `<tmp>/zccache-daemon.exe.old.<rand>`,
    // so killing by PID still works even though the canonical path now
    // contains "replaced".
    let _ = child.kill();
    let _ = child.wait();

    assert!(
        overwrote,
        "daemon exe path remained locked after spawn — last error: {last_err:?}"
    );
}
