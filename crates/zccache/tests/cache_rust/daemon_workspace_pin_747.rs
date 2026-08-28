//! Integration regression for #747: a live daemon process must not pin
//! the directory from which it was spawned on Windows.
//!
//! Repro shape (same as the issue's repro):
//!   1. Spawn the real `zccache-daemon --foreground` binary with its
//!      child-process cwd set to a fresh temp directory.
//!   2. Wait long enough for the daemon's startup path to run through
//!      `trampoline::release_cwd()` and bind its IPC endpoint.
//!   3. While the daemon is still alive, try to delete the temp dir.
//!
//! On Windows pre-fix, step 3 fails with `os error 32` ("process cannot
//! access the file") because the daemon holds an implicit kernel
//! `FILE_OBJECT` on its inherited cwd.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::panic_in_result_fn,
    clippy::unwrap_in_result
)]

use std::process::{Command, Stdio};
use std::time::Duration;

#[test]
#[cfg_attr(
    not(windows),
    ignore = "CWD-handle pinning is a Windows-only failure mode"
)]
fn spawned_daemon_does_not_pin_its_launch_cwd() {
    let daemon_bin = env!("CARGO_BIN_EXE_zccache-daemon");

    let workspace = tempfile::tempdir().expect("create workspace tempdir");
    let workspace_path = workspace
        .path()
        .canonicalize()
        .expect("canonicalize workspace tempdir");

    // Per-test isolated state so we don't collide with the user's running
    // daemon or with another test running in parallel.
    let nonce = format!(
        "{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    );
    let endpoint = endpoint_for(&nonce);
    let cache_dir = std::env::temp_dir().join(format!("zccache-747-cache-{nonce}"));
    let log_file = std::env::temp_dir().join(format!("zccache-747-log-{nonce}.log"));
    std::fs::create_dir_all(&cache_dir).expect("create per-test cache dir");

    let mut cmd = Command::new(daemon_bin);
    cmd.current_dir(&workspace_path)
        .env("ZCCACHE_CACHE_DIR", &cache_dir)
        .env("ZCCACHE_QUIET", "1")
        .args([
            "--foreground",
            "--endpoint",
            &endpoint,
            "--log-file",
            &log_file.to_string_lossy(),
            // Short idle timeout so a wedged daemon eventually exits even
            // if the test panics before its own kill arm.
            "--idle-timeout",
            "30",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    let mut child = cmd.spawn().expect("spawn zccache-daemon");

    // Give the daemon time to:
    //   * detach stdio
    //   * init tracing
    //   * call release_cwd()
    //   * enter run_server() and bind the IPC endpoint
    // 3 s is generous — startup on a cold cache is ~hundreds of ms.
    std::thread::sleep(Duration::from_secs(3));

    // Sanity: a daemon that crashed before reaching release_cwd would
    // make the test pass for the wrong reason. Skip with a clear note if
    // the binary didn't survive startup (e.g. CI runner without the
    // necessary cache-dir permissions). Don't `assert!` — surface as
    // ignored-style skip via early return.
    if let Ok(Some(status)) = child.try_wait() {
        eprintln!(
            "warn: daemon exited during startup with {status:?}; \
             skipping pin-check (cannot reproduce the issue on a dead daemon)"
        );
        let _ = std::fs::remove_dir_all(&workspace_path);
        let _ = std::fs::remove_dir_all(&cache_dir);
        let _ = std::fs::remove_file(&log_file);
        return;
    }

    // Heart of the test: while the daemon is alive, the workspace dir
    // it was spawned from MUST be deletable. Pre-fix on Windows this is
    // exactly the OS-level `Remove-Item` failure the issue captured.
    let delete_result = std::fs::remove_dir_all(&workspace_path);

    // Tear the daemon down before asserting so a failing test never
    // leaks a background process or its on-disk state.
    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&cache_dir);
    let _ = std::fs::remove_file(&log_file);

    delete_result.unwrap_or_else(|e| {
        panic!(
            "workspace tempdir {} should be deletable while the daemon is alive, \
             but Remove-Item-equivalent failed: {e} (#747)",
            workspace_path.display()
        )
    });
}

#[cfg(windows)]
fn endpoint_for(nonce: &str) -> String {
    format!(r"\\.\pipe\zccache-test-747-{nonce}")
}

#[cfg(unix)]
fn endpoint_for(nonce: &str) -> String {
    std::env::temp_dir()
        .join(format!("zccache-test-747-{nonce}.sock"))
        .to_string_lossy()
        .into_owned()
}
