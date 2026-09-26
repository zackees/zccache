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

/// Daemon namespace pinned by the forced-stop tests. Development builds
/// otherwise derive a `<version>-<exe hash>` namespace (#1362 / #1394) that
/// this test process does not share, so a lock path computed here would not be
/// the one the CLI reads.
const FORCED_STOP_NAMESPACE: &str = "forced-stop";

/// One isolated cache root plus pinned daemon namespace. Dropping it stops
/// any daemon the scenario left behind.
struct IsolatedDaemon {
    bin: NormalizedPath,
    cache_dir: tempfile::TempDir,
}

impl IsolatedDaemon {
    fn new() -> Self {
        Self {
            bin: zccache_bin(),
            cache_dir: tempfile::Builder::new()
                .prefix("zccache-forced-stop-")
                .tempdir()
                .expect("tempdir"),
        }
    }

    fn command(&self, subcommand: &str) -> Command {
        let mut cmd = Command::new(&self.bin);
        cmd.arg(subcommand)
            .env("ZCCACHE_CACHE_DIR", self.cache_dir.path())
            .env("ZCCACHE_DAEMON_NAMESPACE", FORCED_STOP_NAMESPACE)
            .env_remove("ZCCACHE_ENDPOINT");
        cmd
    }

    /// With `ZCCACHE_CACHE_DIR` set, the daemon lock lives at
    /// `<root>/v<VERSION>/daemon-<namespace>-v<VERSION>.lock` on every
    /// platform.
    fn lock_path(&self) -> std::path::PathBuf {
        let version = zccache::core::config::versioned_subdir();
        self.cache_dir
            .path()
            .join(&version)
            .join(format!("daemon-{FORCED_STOP_NAMESPACE}-{version}.lock"))
    }

    /// Run `zccache stop` against an endpoint nothing serves, so the IPC
    /// shutdown request is unreachable while the lock still names a PID.
    fn stop_with_unreachable_ipc(&self) -> std::process::Output {
        self.command("stop")
            .env("ZCCACHE_ENDPOINT", unserved_endpoint())
            .output()
            .expect("failed to run zccache stop")
    }
}

impl Drop for IsolatedDaemon {
    fn drop(&mut self) {
        let _ = self
            .command("stop")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
}

fn unserved_endpoint() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let unique = format!("zccache-unserved-{}-{nanos}", std::process::id());
    #[cfg(windows)]
    {
        format!(r"\\.\pipe\{unique}")
    }
    #[cfg(unix)]
    {
        std::env::temp_dir()
            .join(format!("{unique}.sock"))
            .to_string_lossy()
            .into_owned()
    }
}

/// `zccache stop` must still terminate the daemon when IPC is unreachable but
/// the lock file points at the live, identity-verified daemon process.
#[test]
#[ignore] // Integration test — starts a real daemon. Run with `test --full`.
fn stop_kills_locked_process_when_ipc_is_unreachable() {
    let daemon = IsolatedDaemon::new();
    let start = daemon.command("start").output().expect("run zccache start");
    assert!(
        start.status.success(),
        "zccache start failed: {}",
        String::from_utf8_lossy(&start.stderr)
    );

    let lock_path = daemon.lock_path();
    let pid: u32 = std::fs::read_to_string(&lock_path)
        .unwrap_or_else(|e| panic!("read daemon lock {}: {e}", lock_path.display()))
        .trim()
        .parse()
        .expect("daemon lock holds a PID");
    assert!(
        zccache::ipc::is_process_alive(pid),
        "daemon {pid} must be alive before the forced stop"
    );

    let output = daemon.stop_with_unreachable_ipc();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "zccache stop failed: {stderr}");
    assert!(
        stderr.contains("terminated after IPC connection failed"),
        "stop must take the forced-kill path, not an IPC shutdown: {stderr}"
    );

    let deadline = Instant::now() + Duration::from_secs(10);
    while zccache::ipc::is_process_alive(pid) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        !zccache::ipc::is_process_alive(pid),
        "expected stop to terminate the locked daemon process {pid}"
    );
    assert!(
        !lock_path.exists(),
        "lock file should be removed after forced stop"
    );
}

/// The other half of the forced-stop contract (#1161 fixed in #1286,
/// tightened in #1600): a lock naming a live process that has no persisted
/// daemon identity is uncertain, not stale. `zccache stop` must refuse to kill
/// it, report failure, and keep the lock instead of reopening a possibly
/// recycled PID.
#[test]
#[ignore] // Integration test — manipulates a daemon lock file.
fn stop_refuses_to_kill_unverified_locked_process_when_ipc_is_unreachable() {
    let daemon = IsolatedDaemon::new();
    let lock_path = daemon.lock_path();
    std::fs::create_dir_all(lock_path.parent().expect("lock parent")).expect("create lock dir");

    let mut child = spawn_sleepy_process();
    std::fs::write(&lock_path, child.id().to_string()).expect("write daemon lock");

    let output = daemon.stop_with_unreachable_ipc();
    let survived = child.try_wait().expect("poll sleeper").is_none();
    let _ = child.kill();
    let _ = child.wait();

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success(),
        "stop must report failure when it cannot verify the locked process: {stderr}"
    );
    assert!(
        stderr.contains("refusing to kill"),
        "stop must explain why it left the locked process alone: {stderr}"
    );
    assert!(
        survived,
        "stop must not kill a process it cannot verify as the zccache daemon"
    );
    assert!(
        lock_path.exists(),
        "an uncertain live lock must be preserved, not retired"
    );
}
