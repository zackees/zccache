//! Regression test for issue #323: multiple `zccache-daemon` processes
//! must NOT spawn within a single build session.
//!
//! The bug surfaced in a `setup-soldr` benchmark run that produced 5
//! distinct `daemon-spawn-<pid>-<nanos>.log` files for one
//! `cargo build` invocation. Because each daemon's depgraph lives in
//! its own address space, mid-session respawns fragment the
//! in-memory state that #262 (depgraph persistence) was supposed to
//! warm — dropping the cache hit rate near 0%.
//!
//! This test simulates the same pattern (N serial wrapper invocations,
//! one shared `ZCCACHE_CACHE_DIR`) and asserts the post-conditions
//! that would have caught the bug:
//!
//!   1. `<cache_dir>/logs/` contains exactly **one** file matching
//!      `daemon-spawn-*.log`.
//!   2. `<cache_dir>/logs/daemon-lifecycle.log` contains exactly **one**
//!      `event:"spawn"` line.
//!   3. The `spawn-attempt` line emitted by the *first* wrapper call
//!      carries `reason:"initial-start"` — every subsequent wrapper
//!      call must reuse the live daemon (NO further spawn-attempts).
//!
//! Marked `#[ignore]` so the unit-test pass stays sub-second; runs
//! under `./test --integration` and `./test --full`.

use std::io::Write;
use std::process::{Command, Stdio};

fn target_bin_dir() -> std::path::PathBuf {
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

fn stop_daemon(zccache: &std::path::Path, cache_dir: &std::path::Path) {
    let _ = Command::new(zccache)
        .arg("stop")
        .env("ZCCACHE_CACHE_DIR", cache_dir)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// One wrapper invocation. Mirrors what `RUSTC_WRAPPER=zccache` does
/// per crate during `cargo build` — it talks to the daemon (lazily
/// starting one if needed) then exits.
fn run_one_wrapper(
    zccache: &std::path::Path,
    echo_shim: &std::path::Path,
    cache_dir: &std::path::Path,
    payload: &[u8],
) -> std::process::Output {
    let mut cmd = Command::new(zccache);
    cmd.arg(echo_shim)
        .arg("0")
        .env("ZCCACHE_CACHE_DIR", cache_dir)
        .env_remove("ZCCACHE_SESSION_ID")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = cmd.spawn().expect("spawn zccache wrapper");
    {
        let mut stdin = child.stdin.take().expect("piped stdin");
        stdin.write_all(payload).expect("write stdin payload");
    }
    child.wait_with_output().expect("wait_with_output")
}

fn list_spawn_logs(logs_dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(logs_dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with("daemon-spawn-") && name.ends_with(".log") {
            out.push(entry.path());
        }
    }
    out.sort();
    out
}

fn parse_lifecycle_events(logs_dir: &std::path::Path) -> Vec<serde_json::Value> {
    let path = logs_dir.join("daemon-lifecycle.log");
    let Ok(contents) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    contents
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).expect("daemon-lifecycle.log line must be valid JSON"))
        .collect()
}

/// N serial wrapper invocations against a fresh cache dir must
/// produce exactly one daemon spawn.
#[test]
#[ignore] // Integration: spawns the real daemon. Run via `./test --integration`.
fn n_serial_wrappers_share_one_daemon() {
    let zccache = binary_path("zccache");
    let echo_shim = binary_path("echo_shim");
    if !zccache.exists() || !echo_shim.exists() {
        eprintln!(
            "skipping: required binaries not built ({} or {})",
            zccache.display(),
            echo_shim.display()
        );
        return;
    }

    let cache_dir = tempfile::Builder::new()
        .prefix("zccache-single-daemon-")
        .tempdir()
        .expect("tempdir");

    // Belt-and-braces — kill anything pointing at this cache dir from
    // a prior test run. (TempDir is unique per test, but Windows
    // sometimes recycles working dirs between runs of `cargo test`.)
    stop_daemon(&zccache, cache_dir.path());

    let n = 10;
    let payload = b"single-daemon-per-session\n";
    for i in 0..n {
        let output = run_one_wrapper(&zccache, &echo_shim, cache_dir.path(), payload);
        assert!(
            output.status.success(),
            "wrapper invocation {i} failed: stderr={}",
            String::from_utf8_lossy(&output.stderr),
        );
    }

    // Stop the daemon BEFORE inspecting logs so the lifecycle log
    // gets its `died-shutdown` line and we know the file is quiesced.
    stop_daemon(&zccache, cache_dir.path());

    let effective_cache_dir =
        zccache::core::config::effective_cache_root_from_top_level(&cache_dir.path().into());
    let logs_dir = zccache::core::config::log_dir_from_cache_dir(&effective_cache_dir);
    assert!(
        logs_dir.exists(),
        "logs/ directory must exist after running the wrapper"
    );

    let spawn_logs = list_spawn_logs(&logs_dir);
    assert_eq!(
        spawn_logs.len(),
        1,
        "expected exactly one daemon-spawn-*.log, got {}: {:?}",
        spawn_logs.len(),
        spawn_logs
            .iter()
            .map(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
            .collect::<Vec<_>>()
    );

    let events = parse_lifecycle_events(&logs_dir);
    let spawn_events: Vec<&serde_json::Value> =
        events.iter().filter(|e| e["event"] == "spawn").collect();
    assert_eq!(
        spawn_events.len(),
        1,
        "expected exactly one \"event\":\"spawn\" line in daemon-lifecycle.log, got {} (events: {:#?})",
        spawn_events.len(),
        events
    );

    let attempt_events: Vec<&serde_json::Value> = events
        .iter()
        .filter(|e| e["event"] == "spawn-attempt")
        .collect();
    assert_eq!(
        attempt_events.len(),
        1,
        "expected exactly one \"event\":\"spawn-attempt\" line — every wrapper after the first must reuse the live daemon. Got {} (events: {:#?})",
        attempt_events.len(),
        events
    );
    assert_eq!(
        attempt_events[0]["reason"], "initial-start",
        "first (and only) spawn must be reason=initial-start, got {}",
        attempt_events[0]
    );

    // Sanity: the `died-shutdown` line should also be present after
    // our explicit `zccache stop`, confirming the lifecycle log
    // captures the full lifetime end-to-end.
    let died_shutdown = events.iter().any(|e| e["event"] == "died-shutdown");
    assert!(
        died_shutdown,
        "expected a \"died-shutdown\" event after `zccache stop`, got: {events:#?}"
    );
}
