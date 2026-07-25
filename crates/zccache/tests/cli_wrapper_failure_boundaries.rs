//! Executable-level tests for the wrapper/daemon failure safety contract.
//!
//! These tests intentionally exercise the published wrapper binary. They
//! prove that a pre-dispatch failure replays the compiler exactly once,
//! including stdin, and that a real session does not consume stdin twice
//! while moving from the session path to the ephemeral fallback path.

#![allow(
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_in_result,
    clippy::unwrap_used
)]

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::Duration;

fn target_bin_dir() -> PathBuf {
    let mut path = std::env::current_exe().expect("current executable");
    path.pop();
    path.pop();
    path
}

fn binary_path(stem: &str) -> PathBuf {
    let mut path = target_bin_dir();
    if cfg!(windows) {
        path.push(format!("{stem}.exe"));
    } else {
        path.push(stem);
    }
    path
}

fn stop_daemon(zccache: &Path, cache_dir: &Path) {
    let _ = Command::new(zccache)
        .arg("stop")
        .env("ZCCACHE_CACHE_DIR", cache_dir)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

fn run_wrapper(
    zccache: &Path,
    echo_shim: &Path,
    cache_dir: &Path,
    payload: &[u8],
    session_id: Option<&str>,
    fallback_policy: &str,
) -> Output {
    let mut command = Command::new(zccache);
    command
        .arg(echo_shim)
        .arg("7")
        .env("ZCCACHE_CACHE_DIR", cache_dir)
        .env("ZCCACHE_NO_SPAWN", "1")
        .env("ZCCACHE_DAEMON_WIRE", "bincode")
        // Issue #1211: the default policy is Error (uncached fallback is
        // unsafe with read-only hardlinked artifacts), so the warn-path
        // tests must opt in explicitly to keep exercising the fallback.
        .env("ZCCACHE_FALLBACK", fallback_policy)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(session_id) = session_id {
        command.env("ZCCACHE_SESSION_ID", session_id);
    } else {
        command.env_remove("ZCCACHE_SESSION_ID");
    }

    let mut child = command.spawn().expect("spawn wrapper");
    child
        .stdin
        .take()
        .expect("piped wrapper stdin")
        .write_all(payload)
        .expect("write wrapper stdin");
    child.wait_with_output().expect("wait for wrapper")
}

fn lifecycle_events(cache_dir: &Path) -> Vec<serde_json::Value> {
    let effective =
        zccache::core::config::effective_cache_root_from_top_level(&cache_dir.to_path_buf().into());
    let path =
        zccache::core::config::log_dir_from_cache_dir(&effective).join("daemon-lifecycle.log");
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).expect("valid lifecycle JSONL"))
        .collect()
}

fn assert_one_fallback(cache_dir: &Path, test_name: &'static str) {
    let events = lifecycle_events(cache_dir);
    let fallbacks: Vec<_> = events
        .iter()
        .filter(|event| event["event"] == "wrapper-local-fallback")
        .collect();
    assert_eq!(
        fallbacks.len(),
        1,
        "expected one fallback event: {events:#?}"
    );
    assert_eq!(fallbacks[0]["phase"], "pre-dispatch");
    assert_eq!(
        fallbacks[0]["outcome"], "ran",
        "warn-policy fallback must record that the tool actually ran"
    );

    let effective =
        zccache::core::config::effective_cache_root_from_top_level(&cache_dir.to_path_buf().into());
    let report = zccache::audit::audit_cache_root(
        &effective,
        zccache::audit::LogAuditContext::Integration,
        &zccache::audit::AuditOptions::default().allow_for_test(
            test_name,
            [zccache::audit::RuleId("no-wrapper-local-fallback")],
        ),
    )
    .expect("audit intentional fallback fixture");
    assert!(report.passed(), "{}", report.format_human());
    assert_eq!(report.test_allow_name.as_deref(), Some(test_name));
}

fn wait_for_daemon_shutdown(cache_dir: &Path) {
    for _ in 0..80 {
        if lifecycle_events(cache_dir)
            .iter()
            .any(|event| event["event"] == "died-shutdown")
        {
            return;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    panic!("daemon did not publish died-shutdown within the test deadline");
}

#[test]
#[ignore = "integration test: launches the wrapper binary"]
fn ephemeral_pre_dispatch_fallback_preserves_process_contract() {
    let zccache = binary_path("zccache");
    let echo_shim = binary_path("echo_shim");
    if !zccache.exists() || !echo_shim.exists() {
        eprintln!("skipping: required binaries are not built");
        return;
    }

    let cache_dir = tempfile::tempdir().expect("cache tempdir");
    let payload = b"pre-dispatch-stdin\0with-a-nul\n";
    let output = run_wrapper(
        &zccache,
        &echo_shim,
        cache_dir.path(),
        payload,
        None,
        "warn",
    );
    stop_daemon(&zccache, cache_dir.path());

    assert_eq!(output.status.code(), Some(7));
    assert!(output
        .stdout
        .windows(b"ZCCACHE_PASSTHROUGH_STDOUT_MARKER\n".len())
        .any(|window| window == b"ZCCACHE_PASSTHROUGH_STDOUT_MARKER\n"));
    assert_eq!(
        output
            .stderr
            .windows(payload.len())
            .filter(|window| *window == payload)
            .count(),
        1,
        "stdin must reach the local compiler exactly once"
    );
    assert_one_fallback(
        cache_dir.path(),
        "ephemeral_pre_dispatch_fallback_preserves_process_contract",
    );
}

#[test]
#[ignore = "integration test: launches the wrapper binary"]
fn session_pre_dispatch_fallback_does_not_consume_stdin_twice() {
    let zccache = binary_path("zccache");
    let echo_shim = binary_path("echo_shim");
    if !zccache.exists() || !echo_shim.exists() {
        eprintln!("skipping: required binaries are not built");
        return;
    }

    let cache_dir = tempfile::tempdir().expect("cache tempdir");
    let session = Command::new(&zccache)
        .arg("session-start")
        .env("ZCCACHE_CACHE_DIR", cache_dir.path())
        .env("ZCCACHE_DAEMON_WIRE", "bincode")
        .output()
        .expect("start session");
    assert!(
        session.status.success(),
        "session-start failed: {}",
        String::from_utf8_lossy(&session.stderr)
    );
    let session_json: serde_json::Value =
        serde_json::from_slice(&session.stdout).expect("session-start JSON");
    let session_id = session_json["session_id"]
        .as_str()
        .expect("session id")
        .to_string();
    stop_daemon(&zccache, cache_dir.path());
    wait_for_daemon_shutdown(cache_dir.path());

    let payload = b"session-fallback-stdin\0must-appear-once\n";
    let output = run_wrapper(
        &zccache,
        &echo_shim,
        cache_dir.path(),
        payload,
        Some(&session_id),
        "warn",
    );
    stop_daemon(&zccache, cache_dir.path());

    assert_eq!(output.status.code(), Some(7));
    assert_eq!(
        output
            .stderr
            .windows(payload.len())
            .filter(|window| *window == payload)
            .count(),
        1,
        "session fallback must not slurp stdin a second time"
    );
    assert_one_fallback(
        cache_dir.path(),
        "session_pre_dispatch_fallback_does_not_consume_stdin_twice",
    );
}

/// Issue #1211: under the strict policy (`ZCCACHE_FALLBACK=error`, also the
/// default on every host) a pre-dispatch daemon failure must fail the
/// compile with the failure reason on stderr — the tool is never run
/// uncached (it could not overwrite read-only hardlinked artifacts anyway).
#[test]
#[ignore = "integration test: launches the wrapper binary"]
fn strict_policy_refuses_pre_dispatch_fallback() {
    let zccache = binary_path("zccache");
    let echo_shim = binary_path("echo_shim");
    if !zccache.exists() || !echo_shim.exists() {
        eprintln!("skipping: required binaries are not built");
        return;
    }

    let cache_dir = tempfile::tempdir().expect("cache tempdir");
    let payload = b"strict-mode-stdin\n";
    let output = run_wrapper(
        &zccache,
        &echo_shim,
        cache_dir.path(),
        payload,
        None,
        "error",
    );
    stop_daemon(&zccache, cache_dir.path());

    assert_eq!(
        output.status.code(),
        Some(1),
        "strict mode must fail the compile, not mirror the tool's exit code"
    );
    assert!(
        !output
            .stdout
            .windows(b"ZCCACHE_PASSTHROUGH_STDOUT_MARKER".len())
            .any(|window| window == b"ZCCACHE_PASSTHROUGH_STDOUT_MARKER"),
        "strict mode must not run the tool uncached"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("refusing uncached fallback"),
        "stderr must announce the refusal: {stderr}"
    );
    assert!(
        stderr.contains("cannot start daemon") || stderr.contains("cannot connect to daemon"),
        "stderr must carry the daemon-failure reason: {stderr}"
    );

    let events = lifecycle_events(cache_dir.path());
    let fallbacks: Vec<_> = events
        .iter()
        .filter(|event| event["event"] == "wrapper-local-fallback")
        .collect();
    assert_eq!(
        fallbacks.len(),
        1,
        "expected one blocked event: {events:#?}"
    );
    assert_eq!(fallbacks[0]["outcome"], "blocked");
}
