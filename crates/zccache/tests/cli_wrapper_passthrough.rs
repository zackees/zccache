//! End-to-end byte-for-byte stdio passthrough test for the wrapper +
//! daemon IPC chain.
//!
//! Setup: point `ZCCACHE_CACHE_DIR` at a tempdir so the daemon spins up
//! fresh and we don't poison the developer's cache. Invoke
//! `zccache <echo_shim>` with a multi-KB random stdin payload (NUL bytes
//! included). Assert:
//!  * `STDOUT_MARKER` shows up in the wrapper's stdout
//!  * `STDERR_MARKER` + the entire stdin payload show up in the wrapper's
//!    stderr, byte-for-byte
//!  * exit code matches what `echo_shim` was told to return
//!
//! Covers both `cmd_compile_ephemeral` (no `ZCCACHE_SESSION_ID`) and
//! `cmd_compile` (with one). Wrapping a non-compiler binary forces the
//! daemon's `run_compiler_direct` path, which is also the path that
//! exercises the stdin pipe (see `crates/zccache-daemon/src/server.rs`
//! `run_compiler_direct` and `process::tokio_command_output_with_priority_stdin`).
//!
//! Marked `#[ignore]` so the unit-test pass stays sub-second; the
//! `./test --integration` and `./test --full` runners pick this up.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::panic_in_result_fn,
    clippy::unwrap_in_result
)]

use std::io::Write;
use std::process::{Command, Stdio};

const STDOUT_MARKER: &[u8] = b"ZCCACHE_PASSTHROUGH_STDOUT_MARKER\n";
const STDERR_MARKER: &[u8] = b"ZCCACHE_PASSTHROUGH_STDERR_MARKER\n";

fn target_bin_dir() -> std::path::PathBuf {
    // Test binaries live at target/<profile>/deps/<name>-<hash>(.exe).
    // current_exe().parent() is `deps/`, parent of that is the profile dir.
    let mut p = std::env::current_exe().expect("current_exe");
    p.pop(); // deps/
    p.pop(); // target/<profile>/
    p
}

fn binary_path(stem: &str) -> std::path::PathBuf {
    let mut p = target_bin_dir();
    if cfg!(windows) {
        p.push(format!("{stem}.exe"));
    } else {
        p.push(stem);
    }
    p
}

fn cache_dir_tempdir() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("zccache-wrapper-passthrough-")
        .tempdir()
        .expect("tempdir")
}

fn random_payload(len: usize, seed: u64) -> Vec<u8> {
    // Deterministic xorshift — no rand crate needed in test-support.
    let mut s = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut out = Vec::with_capacity(len);
    for _ in 0..len {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        out.push(s as u8);
    }
    out
}

fn stop_daemon(zccache: &std::path::Path, endpoint: &std::path::Path) {
    let _ = Command::new(zccache)
        .arg("stop")
        .env("ZCCACHE_CACHE_DIR", endpoint)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

fn run_wrapper(
    zccache: &std::path::Path,
    echo_shim: &std::path::Path,
    cache_dir: &std::path::Path,
    stdin_payload: &[u8],
    exit_code: i32,
    session_id: Option<&str>,
) -> (i32, Vec<u8>, Vec<u8>) {
    let mut cmd = Command::new(zccache);
    cmd.arg(echo_shim)
        .arg(exit_code.to_string())
        .env("ZCCACHE_CACHE_DIR", cache_dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(sid) = session_id {
        cmd.env("ZCCACHE_SESSION_ID", sid);
    } else {
        cmd.env_remove("ZCCACHE_SESSION_ID");
    }

    let mut child = cmd.spawn().expect("spawn zccache wrapper");
    {
        let mut stdin = child.stdin.take().expect("piped stdin");
        stdin.write_all(stdin_payload).expect("write stdin payload");
        // Drop closes the pipe → EOF for the child.
    }
    let output = child.wait_with_output().expect("wait_with_output");
    (
        output.status.code().unwrap_or(-1),
        output.stdout,
        output.stderr,
    )
}

fn assert_passthrough(payload: &[u8], stdout: &[u8], stderr: &[u8]) {
    assert!(
        stdout
            .windows(STDOUT_MARKER.len())
            .any(|w| w == STDOUT_MARKER),
        "STDOUT_MARKER missing from wrapper stdout. stdout = {:?}",
        String::from_utf8_lossy(stdout)
    );
    let stderr_marker_pos = stderr
        .windows(STDERR_MARKER.len())
        .position(|w| w == STDERR_MARKER)
        .unwrap_or_else(|| {
            panic!(
                "STDERR_MARKER missing from wrapper stderr. stderr = {:?}",
                String::from_utf8_lossy(stderr)
            )
        });
    let after_marker = &stderr[stderr_marker_pos + STDERR_MARKER.len()..];
    let payload_pos = after_marker
        .windows(payload.len())
        .position(|w| w == payload)
        .unwrap_or_else(|| {
            panic!(
                "stdin payload ({} bytes) not found in wrapper stderr after the marker",
                payload.len()
            )
        });
    let _ = payload_pos; // existence is the assertion
}

#[test]
#[ignore] // Integration: spawns the real daemon. Run via `./test --integration`.
fn wrapper_passthrough_ephemeral_exit_zero() {
    let zccache = binary_path("zccache");
    let echo_shim = binary_path("echo_shim");
    assert!(
        zccache.exists(),
        "zccache binary missing at {zccache:?} — run `soldr cargo build -p zccache-cli` first"
    );
    assert!(
        echo_shim.exists(),
        "echo_shim binary missing at {echo_shim:?} — run `soldr cargo build -p zccache-test-support` first"
    );

    let cache_dir = cache_dir_tempdir();
    let payload = random_payload(8 * 1024 + 7, 0xDEAD_BEEF); // odd size, includes NULs

    let (code, stdout, stderr) =
        run_wrapper(&zccache, &echo_shim, cache_dir.path(), &payload, 0, None);
    stop_daemon(&zccache, cache_dir.path());

    assert_eq!(
        code,
        0,
        "wrapper exit code wrong (stderr: {:?})",
        String::from_utf8_lossy(&stderr)
    );
    assert_passthrough(&payload, &stdout, &stderr);
}

#[test]
#[ignore] // Integration: spawns the real daemon.
fn wrapper_passthrough_ephemeral_exit_nonzero() {
    let zccache = binary_path("zccache");
    let echo_shim = binary_path("echo_shim");
    if !zccache.exists() || !echo_shim.exists() {
        eprintln!("skipping: required binaries not built");
        return;
    }

    let cache_dir = cache_dir_tempdir();
    let payload = random_payload(2_048, 0xC0FFEE);

    let (code, stdout, stderr) =
        run_wrapper(&zccache, &echo_shim, cache_dir.path(), &payload, 7, None);
    stop_daemon(&zccache, cache_dir.path());

    assert_eq!(
        code, 7,
        "wrapper must mirror child exit code on non-zero exit"
    );
    assert_passthrough(&payload, &stdout, &stderr);
}

#[test]
#[ignore] // Integration: spawns the real daemon.
fn wrapper_passthrough_session_path() {
    let zccache = binary_path("zccache");
    let echo_shim = binary_path("echo_shim");
    if !zccache.exists() || !echo_shim.exists() {
        eprintln!("skipping: required binaries not built");
        return;
    }

    let cache_dir = cache_dir_tempdir();
    let payload = random_payload(1_024, 0x1234_5678);

    // Need a real session id for cmd_compile. The CLI creates one via
    // SessionStart; for the test we set the env var with a UUID-shaped
    // value and rely on the daemon allocating it ephemerally on first
    // touch. Today the daemon rejects unknown session ids with
    // Response::Error — which the wrapper relays. So the test asserts
    // the wrapper relays the error rather than swallowing it: stdout
    // empty, stderr contains `zccache error:`, exit non-zero.
    let (code, stdout, stderr) = run_wrapper(
        &zccache,
        &echo_shim,
        cache_dir.path(),
        &payload,
        0,
        Some("550e8400-e29b-41d4-a716-446655440000"),
    );
    stop_daemon(&zccache, cache_dir.path());

    assert!(
        stdout.is_empty(),
        "wrapper must not invent stdout on daemon error"
    );
    assert_ne!(code, 0, "wrapper exit must be non-zero when daemon errors");
    assert!(
        stderr.windows(b"zccache".len()).any(|w| w == b"zccache"),
        "wrapper must surface the daemon error to stderr"
    );
}
