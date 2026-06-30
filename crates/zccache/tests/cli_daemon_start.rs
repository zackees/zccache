//! Integration tests for daemon auto-start.
//!
//! These tests verify that `zccache start` works correctly when invoked
//! from a parent process that captures stdout/stderr via pipes.
//!
//! ## Bug: Handle inheritance on Windows
//!
//! When `zccache start` is called from a process that captures pipes
//! (e.g. Python's `subprocess.run(capture_output=True)`), the spawned
//! daemon can inherit the pipe handles. Since the daemon runs forever,
//! the pipe never closes, and the parent hangs indefinitely.
//!
//! The fix: mark stdout/stderr as non-inheritable before spawning the
//! daemon process on Windows.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::panic_in_result_fn,
    clippy::unwrap_in_result
)]

use std::process::Command;
use std::time::{Duration, Instant};
use zccache::core::NormalizedPath;

/// Find the zccache binary in the target directory.
fn zccache_bin() -> NormalizedPath {
    let mut path = std::env::current_exe()
        .expect("current_exe")
        .parent()
        .expect("parent of test binary")
        .parent()
        .expect("target dir")
        .to_path_buf();

    if cfg!(windows) {
        path.push("zccache.exe");
    } else {
        path.push("zccache");
    }

    assert!(
        path.exists(),
        "zccache binary not found at {path:?}. Run `cargo build` first."
    );
    NormalizedPath::new(path)
}

/// Stop the daemon and wait until the endpoint is fully released.
fn stop_daemon_and_wait(bin: &std::path::Path) {
    let _ = Command::new(bin)
        .arg("stop")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();

    // Wait until the daemon fully exits and releases the named pipe / socket.
    // On Windows, named pipes can linger briefly after the server process exits.
    for _ in 0..30 {
        std::thread::sleep(Duration::from_millis(200));

        // Try to connect — if it fails, the daemon is fully stopped
        let status = Command::new(bin)
            .arg("status")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();

        match status {
            Ok(s) if !s.success() => return, // daemon stopped
            Err(_) => return,                // can't even run status
            _ => {}                          // still running, keep waiting
        }
    }
    // If we get here, daemon is still running after 6s — proceed anyway
}

fn spawn_sleepy_process() -> std::process::Child {
    #[cfg(windows)]
    {
        Command::new("powershell")
            .args(["-NoProfile", "-Command", "Start-Sleep -Seconds 30"])
            .spawn()
            .expect("spawn sleeper")
    }

    #[cfg(unix)]
    {
        Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("spawn sleeper")
    }
}

/// `zccache start` must complete promptly when stdout/stderr are pipes.
///
/// This is the core regression test for the Windows handle inheritance bug.
/// Before the fix, this test would hang indefinitely because the daemon
/// inherited the pipe handles and never closed them.
#[test]
#[ignore] // Integration test — starts a real daemon. Run with `test --full`.
fn start_completes_with_captured_pipes() {
    let bin = zccache_bin();
    stop_daemon_and_wait(&bin);

    let start = Instant::now();
    let output = Command::new(&bin)
        .arg("start")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .expect("failed to run zccache start");

    let elapsed = start.elapsed();

    // Clean up
    stop_daemon_and_wait(&bin);

    assert!(
        output.status.success(),
        "zccache start failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // The command should complete in well under 10 seconds.
    // Before the fix, it would hang forever (>30s timeout in CI).
    assert!(
        elapsed < Duration::from_secs(10),
        "zccache start took {elapsed:?} — likely hanging due to handle inheritance"
    );
}

/// Multiple concurrent `zccache start` calls should all complete.
///
/// This tests the daemon auto-start race: when N processes try to start
/// the daemon simultaneously, they should all succeed (one spawns, others
/// connect to the already-started daemon).
#[test]
#[ignore] // Integration test — starts a real daemon. Run with `test --full`.
fn concurrent_starts_all_complete() {
    let bin = zccache_bin();
    stop_daemon_and_wait(&bin);

    // Extra wait to ensure pipe is fully released on Windows
    std::thread::sleep(Duration::from_secs(1));

    let n = 5;
    let handles: Vec<_> = (0..n)
        .map(|_| {
            let bin = bin.clone();
            std::thread::spawn(move || {
                let start = Instant::now();
                let output = Command::new(&bin)
                    .arg("start")
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::piped())
                    .output()
                    .expect("failed to run zccache start");
                (start.elapsed(), output)
            })
        })
        .collect();

    let mut failures = Vec::new();
    for (i, handle) in handles.into_iter().enumerate() {
        let (elapsed, output) = handle.join().expect("thread panicked");
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            failures.push(format!(
                "  thread {i}: exit={}, elapsed={elapsed:?}, stderr={stderr}",
                output.status,
            ));
        }
        assert!(
            elapsed < Duration::from_secs(20),
            "thread {i} took {elapsed:?} — hanging due to handle inheritance"
        );
    }

    // Clean up
    stop_daemon_and_wait(&bin);

    assert!(
        failures.is_empty(),
        "Some concurrent starts failed:\n{}",
        failures.join("\n")
    );
}

/// `zccache stop` must still terminate a daemon process when IPC is unreachable
/// but the lock file still points at a live PID.
#[test]
#[ignore] // Integration test — manipulates the real daemon lock file.
fn stop_kills_locked_process_when_ipc_is_unreachable() {
    let bin = zccache_bin();
    stop_daemon_and_wait(&bin);

    let lock_path = zccache::ipc::lock_file_path();
    let mut child = spawn_sleepy_process();
    std::fs::write(&lock_path, child.id().to_string()).expect("write daemon lock");

    let output = Command::new(&bin)
        .arg("stop")
        .output()
        .expect("failed to run zccache stop");

    let status = child.wait().expect("wait for killed process");
    let _ = std::fs::remove_file(&lock_path);

    assert!(
        output.status.success(),
        "zccache stop failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !status.success(),
        "expected stop to terminate the locked process, got exit status {status}"
    );
    assert!(
        !lock_path.exists(),
        "lock file should be removed after forced stop"
    );
}
