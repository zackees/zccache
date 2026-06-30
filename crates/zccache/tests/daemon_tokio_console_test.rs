//! Integration proof for Tokio Console's default-dormant behavior.
//!
//! The daemon always links the console subscriber, but it must only start the
//! console server when explicitly launched with the tokio-console daemon
//! profile. This test snapshots that process state with a real daemon process:
//! no TCP listener in normal mode, then either a TCP listener plus profile log
//! in console mode or a clean unavailable warning when the binary was not built
//! with Tokio's `tokio_unstable` cfg.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::panic_in_result_fn,
    clippy::unwrap_in_result
)]

use serde_json::json;
use std::io;
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

#[test]
fn tokio_console_is_compiled_in_but_dormant_until_profile_start() {
    let daemon_bin = env!("CARGO_BIN_EXE_zccache-daemon");
    let tmp = tempfile::tempdir().expect("create tempdir");

    let dormant_bind = free_console_bind();
    let mut dormant = spawn_daemon(daemon_bin, tmp.path(), "dormant", None);
    wait_for_log(
        &dormant.log_file,
        "listening for connections",
        Duration::from_secs(10),
    )
    .expect("dormant daemon should reach listening state");

    let dormant_tcp_listening = wait_for_tcp(&dormant_bind, Duration::from_millis(750));
    let dormant_log = read_log(&dormant.log_file);
    let dormant_snapshot = json!({
        "mode": "dormant",
        "daemon_pid": dormant.child.id(),
        "tokio_console_bind": dormant_bind.to_string(),
        "tokio_console_tcp_listening": dormant_tcp_listening,
        "profile_log_present": dormant_log.contains("tokio-console daemon profile enabled"),
    });
    println!("tokio-console dormant snapshot: {dormant_snapshot}");

    assert!(
        !dormant_tcp_listening,
        "normal daemon unexpectedly opened Tokio Console listener at {dormant_bind}; \
         snapshot={dormant_snapshot}"
    );
    assert!(
        !dormant_log.contains("tokio-console daemon profile enabled"),
        "normal daemon unexpectedly logged tokio-console profile activation; \
         snapshot={dormant_snapshot}"
    );
    dormant.stop();

    let active_bind = free_console_bind();
    let mut active = spawn_daemon(
        daemon_bin,
        tmp.path(),
        "active",
        Some(active_bind.to_string()),
    );
    wait_for_any_log(
        &active.log_file,
        &[
            "tokio-console daemon profile enabled",
            "tokio-console daemon profile requested but unavailable",
        ],
        Duration::from_secs(10),
    )
    .expect("profile daemon should log tokio-console activation or unavailable warning");

    let active_tcp_listening = wait_for_tcp(&active_bind, Duration::from_secs(10));
    let active_log = read_log(&active.log_file);
    let active_enabled = active_log.contains("tokio-console daemon profile enabled");
    let active_unavailable =
        active_log.contains("tokio-console daemon profile requested but unavailable");
    let active_snapshot = json!({
        "mode": "active",
        "daemon_pid": active.child.id(),
        "tokio_console_bind": active_bind.to_string(),
        "tokio_console_tcp_listening": active_tcp_listening,
        "profile_log_present": active_enabled,
        "profile_unavailable_log_present": active_unavailable,
    });
    println!("tokio-console active snapshot: {active_snapshot}");

    active.stop();

    assert!(
        active_enabled || active_unavailable,
        "tokio-console profile logged neither activation nor unavailable warning; \
         snapshot={active_snapshot}"
    );
    if active_enabled {
        assert!(
            active_tcp_listening,
            "tokio-console profile did not open listener at {active_bind}; \
             snapshot={active_snapshot}"
        );
    } else {
        assert!(
            !active_tcp_listening,
            "tokio-console profile reported unavailable but opened listener at {active_bind}; \
             snapshot={active_snapshot}"
        );
    }
}

struct DaemonProcess {
    child: Child,
    log_file: PathBuf,
}

impl DaemonProcess {
    fn stop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for DaemonProcess {
    fn drop(&mut self) {
        self.stop();
    }
}

fn spawn_daemon(
    daemon_bin: &str,
    root: &Path,
    mode: &str,
    tokio_console_bind: Option<String>,
) -> DaemonProcess {
    let nonce = format!(
        "{}-{mode}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    );
    let cache_dir = root.join(format!("cache-{nonce}"));
    let log_file = root.join(format!("daemon-{nonce}.log"));
    std::fs::create_dir_all(&cache_dir).expect("create cache dir");

    let endpoint = endpoint_for(&nonce, root);
    let mut cmd = Command::new(daemon_bin);
    cmd.args([
        "--foreground",
        "--endpoint",
        &endpoint,
        "--log-file",
        &log_file.to_string_lossy(),
        "--idle-timeout",
        "30",
    ])
    .env("ZCCACHE_CACHE_DIR", &cache_dir)
    .env("ZCCACHE_QUIET", "1")
    .env("ZCCACHE_NO_UNLOCK", "1")
    .stdin(Stdio::null())
    .stdout(Stdio::null())
    .stderr(Stdio::null());

    if let Some(bind) = tokio_console_bind {
        cmd.env("ZCCACHE_DAEMON_PROFILE", "tokio-console")
            .env("TOKIO_CONSOLE_BIND", bind);
    }

    let child = cmd.spawn().expect("spawn zccache-daemon");
    DaemonProcess { child, log_file }
}

#[cfg(windows)]
fn endpoint_for(nonce: &str, _root: &Path) -> String {
    format!(r"\\.\pipe\zccache-test-tokio-console-{nonce}")
}

#[cfg(unix)]
fn endpoint_for(nonce: &str, root: &Path) -> String {
    root.join(format!("zccache-test-tokio-console-{nonce}.sock"))
        .to_string_lossy()
        .into_owned()
}

fn free_console_bind() -> SocketAddr {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind free local port");
    listener.local_addr().expect("read local addr")
}

fn wait_for_tcp(addr: &SocketAddr, timeout: Duration) -> bool {
    wait_until(timeout, || {
        TcpStream::connect_timeout(addr, Duration::from_millis(100)).is_ok()
    })
}

fn wait_for_log(log_file: &Path, needle: &str, timeout: Duration) -> io::Result<()> {
    let found = wait_until(timeout, || read_log(log_file).contains(needle));
    if found {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::TimedOut,
            format!(
                "timed out waiting for log line {needle:?}; current log: {}",
                read_log(log_file)
            ),
        ))
    }
}

fn wait_for_any_log(log_file: &Path, needles: &[&str], timeout: Duration) -> io::Result<()> {
    let found = wait_until(timeout, || {
        let log = read_log(log_file);
        needles.iter().any(|needle| log.contains(needle))
    });
    if found {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::TimedOut,
            format!(
                "timed out waiting for any log line in {needles:?}; current log: {}",
                read_log(log_file)
            ),
        ))
    }
}

fn read_log(log_file: &Path) -> String {
    std::fs::read_to_string(log_file).unwrap_or_default()
}

fn wait_until(timeout: Duration, mut predicate: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if predicate() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    predicate()
}
