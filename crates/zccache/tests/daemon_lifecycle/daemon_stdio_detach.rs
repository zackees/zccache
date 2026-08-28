//! Regression test for `trampoline::detach_stdio()`.
//!
//! The daemon must release any stdio handles it inherited from its parent
//! at startup. Otherwise a grandparent process that reads those pipes
//! (e.g. Python's `subprocess.Popen(..., stdout=PIPE)` wrapping
//! `soldr cargo build`) never observes EOF on its read end after the
//! parent exits — the orphaned daemon keeps the pipe's write end open
//! indefinitely.
//!
//! Test strategy: spawn the daemon binary with `Stdio::piped()` for
//! stdin/stdout/stderr. We hold the read ends; the daemon inherits the
//! write ends. After `detach_stdio()` runs in the daemon, our reader
//! threads see EOF and unblock. Without the fix this test hangs until
//! the cleanup `kill` (which we count as a failure).
//!
//! See issue #276.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::panic_in_result_fn,
    clippy::unwrap_in_result
)]

use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

#[test]
fn daemon_releases_inherited_stdout_and_stderr_pipes() {
    let daemon_bin = env!("CARGO_BIN_EXE_zccache-daemon");
    let tmp = tempfile::tempdir().expect("create tempdir");
    let cache_dir = tmp.path().join("cache");
    std::fs::create_dir_all(&cache_dir).expect("create cache dir");

    let endpoint = zccache::ipc::unique_test_endpoint();

    let mut child = Command::new(daemon_bin)
        .args(["--foreground", "--endpoint", &endpoint])
        .env("ZCCACHE_CACHE_DIR", &cache_dir)
        // Skip the Windows exe rename dance; irrelevant to this regression
        // and avoids touching the cargo target dir from inside the test.
        .env("ZCCACHE_NO_UNLOCK", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn daemon");

    let stdout = child.stdout.take().expect("take child stdout");
    let stderr = child.stderr.take().expect("take child stderr");

    let (tx_out, rx_out) = mpsc::channel::<()>();
    thread::spawn(move || {
        let mut sink = stdout;
        let mut buf = Vec::new();
        let _ = sink.read_to_end(&mut buf);
        let _ = tx_out.send(());
    });

    let (tx_err, rx_err) = mpsc::channel::<()>();
    thread::spawn(move || {
        let mut sink = stderr;
        let mut buf = Vec::new();
        let _ = sink.read_to_end(&mut buf);
        let _ = tx_err.send(());
    });

    // The daemon's detach_stdio() runs at the top of main(); reaching it is
    // a few hundred microseconds. 10 s is generous enough for slow CI yet
    // short enough that a hang is unambiguous.
    let timeout = Duration::from_secs(10);
    let stdout_eof = rx_out.recv_timeout(timeout).is_ok();
    let stderr_eof = rx_err.recv_timeout(timeout).is_ok();

    // Tear down before asserting so we never leave a daemon behind.
    let _ = child.kill();
    let _ = child.wait();

    assert!(
        stdout_eof,
        "daemon did not close its inherited stdout within {timeout:?}; \
         a grandparent process reading this pipe would hang indefinitely. \
         See issue #276."
    );
    assert!(
        stderr_eof,
        "daemon did not close its inherited stderr within {timeout:?}; \
         a grandparent process reading this pipe would hang indefinitely. \
         See issue #276."
    );
}
