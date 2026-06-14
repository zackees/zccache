//! Synthetic reproducer for issue #691: parallel `zccache` wrapper
//! invocations against a fresh cache dir must converge on a **single**
//! daemon, not race into a spawn storm.
//!
//! ## What #691 reports
//!
//! A single timed-out parent under heavy parallel rustc load was
//! observed to leave 22+ zombie `zccache` client processes and 21+
//! zombie `zccache-daemon` processes in a single namespace, with an
//! aggregate spawn rate of ~15–18 daemons/second. The post-spawn
//! daemon-side dedup (#640) keeps most racers from binding the IPC
//! endpoint, but the storm itself burns kernel objects and process
//! slots until the orchestrator dies.
//!
//! ## What this test asserts
//!
//! Modeled on the serial `cli_single_daemon_per_session` test (#323
//! regression) and on `daemon_burst_link_test` (#729 / #733 precedent
//! for a synthetic reproducer of a daemon-side concurrency bug). Fires
//! N parallel `zccache <echo_shim>` invocations against a single fresh
//! `ZCCACHE_CACHE_DIR`, then asserts:
//!
//!   1. Exactly **one** `daemon-spawn-*.log` lands in `logs/`.
//!   2. The `daemon-lifecycle.log` contains exactly **one**
//!      `event:"spawn"` line.
//!
//! On main today this test is expected to FAIL — multiple `spawn-attempt`
//! events fire in the racing window before the post-spawn dedup catches
//! them, and on heavily loaded runners multiple `spawn` events land
//! before sibling daemons probe and exit.
//!
//! ## How to run
//!
//! Marked `#[ignore]` and additionally gated on
//! `ZCCACHE_RUN_REPRO_691=1` so it does not surprise either the unit
//! suite (`cargo test`) or the standard integration suite
//! (`cargo test -- --ignored` / `./test --integration`). Opt in with:
//!
//! ```text
//! ZCCACHE_RUN_REPRO_691=1 \
//!   ./test --integration -- daemon_spawn_storm
//! ```
//!
//! Tunables:
//!   - `ZCCACHE_SPAWN_STORM_N` — number of parallel wrapper invocations
//!     (default 16). The storm in the issue used a much larger N under
//!     real workload pressure; 16 is enough to demonstrate the race
//!     on a typical multi-core dev box.
//!   - `ZCCACHE_SPAWN_STORM_BUDGET_SECS` — overall wall-time budget
//!     (default 90 s). Mirrors the budget the burst-link reproducer
//!     uses for the same class of test.
//!
//! Once the spawn-coordination fix from #691 lands (likely a named
//! mutex on Windows + `flock` on Unix gating `ensure_daemon`), this
//! test should pass and the env-var gate can be dropped so it joins
//! the standard integration suite as a permanent regression test.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::{Duration, Instant};

const DEFAULT_N: usize = 16;
const DEFAULT_BUDGET_SECS: u64 = 90;
const GATE_ENV: &str = "ZCCACHE_RUN_REPRO_691";
const N_ENV: &str = "ZCCACHE_SPAWN_STORM_N";
const BUDGET_ENV: &str = "ZCCACHE_SPAWN_STORM_BUDGET_SECS";

fn target_bin_dir() -> PathBuf {
    let mut p = std::env::current_exe().expect("current_exe");
    p.pop(); // deps/
    p.pop(); // target/<profile>/
    p
}

fn binary_path(stem: &str) -> PathBuf {
    let mut p = target_bin_dir();
    if cfg!(windows) {
        p.push(format!("{stem}.exe"));
    } else {
        p.push(stem);
    }
    p
}

fn stop_daemon(zccache: &Path, cache_dir: &Path) {
    let _ = Command::new(zccache)
        .arg("stop")
        .env("ZCCACHE_CACHE_DIR", cache_dir)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// One wrapper invocation, mirroring `RUSTC_WRAPPER=zccache` plus a
/// trivial compiler shim. Blocks on a shared `Barrier` so all N
/// invocations leave the gate together — without the barrier the OS
/// scheduler tends to serialize fork/exec and we lose the race window
/// that #691 actually trips over.
fn run_one_wrapper(
    zccache: &Path,
    echo_shim: &Path,
    cache_dir: &Path,
    payload: &[u8],
    gate: &Barrier,
) -> std::io::Result<std::process::Output> {
    let mut cmd = Command::new(zccache);
    cmd.arg(echo_shim)
        .arg("0")
        .env("ZCCACHE_CACHE_DIR", cache_dir)
        .env_remove("ZCCACHE_SESSION_ID")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    // All threads block here, then race the process spawn together.
    gate.wait();
    let mut child = cmd.spawn()?;
    {
        let mut stdin = child.stdin.take().expect("piped stdin");
        stdin.write_all(payload)?;
    }
    child.wait_with_output()
}

fn list_spawn_logs(logs_dir: &Path) -> Vec<PathBuf> {
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

fn parse_lifecycle_events(logs_dir: &Path) -> Vec<serde_json::Value> {
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

fn env_usize(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn env_secs(name: &str, default: u64) -> Duration {
    let secs = std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default);
    Duration::from_secs(secs)
}

/// N parallel wrapper invocations against one fresh cache dir must
/// converge on a single daemon.
#[test]
#[ignore = "reproducer for #691 — gated on ZCCACHE_RUN_REPRO_691"]
fn parallel_wrappers_must_share_one_daemon() {
    if std::env::var(GATE_ENV).is_err() {
        eprintln!(
            "skipping: set {GATE_ENV}=1 to run this reproducer (it is expected to FAIL on \
             main until the #691 spawn-coordination fix lands)"
        );
        return;
    }

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

    let n = env_usize(N_ENV, DEFAULT_N);
    assert!(n >= 2, "{N_ENV}={n} — need at least 2 to race");
    let budget = env_secs(BUDGET_ENV, DEFAULT_BUDGET_SECS);

    let cache_dir = tempfile::Builder::new()
        .prefix("zccache-spawn-storm-")
        .tempdir()
        .expect("tempdir");

    // Belt-and-braces: kill any prior daemon pointing at this dir, in
    // case a previous run was Ctrl-C'd before its TempDir drop ran.
    stop_daemon(&zccache, cache_dir.path());

    let gate = Arc::new(Barrier::new(n));
    let payload: &[u8] = b"spawn-storm-repro-691\n";

    let started = Instant::now();
    let handles: Vec<thread::JoinHandle<std::process::Output>> = (0..n)
        .map(|i| {
            let zccache = zccache.clone();
            let echo_shim = echo_shim.clone();
            let cache_path = cache_dir.path().to_path_buf();
            let gate = Arc::clone(&gate);
            thread::Builder::new()
                .name(format!("spawn-storm-{i}"))
                .spawn(move || {
                    run_one_wrapper(&zccache, &echo_shim, &cache_path, payload, &gate)
                        .unwrap_or_else(|e| panic!("wrapper {i} spawn failed: {e}"))
                })
                .expect("thread spawn")
        })
        .collect();

    let mut failures: Vec<String> = Vec::new();
    for (i, h) in handles.into_iter().enumerate() {
        let out = h.join().unwrap_or_else(|_| panic!("thread {i} panicked"));
        if !out.status.success() {
            failures.push(format!(
                "wrapper {i} exit={:?} stderr={}",
                out.status.code(),
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
    }

    let elapsed = started.elapsed();
    assert!(
        elapsed < budget,
        "spawn-storm reproducer exceeded {budget:?} (took {elapsed:?}); raise \
         {BUDGET_ENV} or lower {N_ENV}={n} if this is expected on the runner"
    );
    assert!(
        failures.is_empty(),
        "{} of {} wrapper invocations failed:\n{}",
        failures.len(),
        n,
        failures.join("\n")
    );

    stop_daemon(&zccache, cache_dir.path());

    let logs_dir = cache_dir.path().join("logs");
    assert!(
        logs_dir.exists(),
        "logs/ directory must exist after running the wrapper"
    );

    let spawn_logs = list_spawn_logs(&logs_dir);
    let events = parse_lifecycle_events(&logs_dir);
    let spawn_events: Vec<&serde_json::Value> =
        events.iter().filter(|e| e["event"] == "spawn").collect();
    let attempt_events: Vec<&serde_json::Value> = events
        .iter()
        .filter(|e| e["event"] == "spawn-attempt")
        .collect();

    assert_eq!(
        spawn_logs.len(),
        1,
        "expected exactly one daemon-spawn-*.log under #691 fix, got {} logs and {} \
         lifecycle spawn events ({} attempts). Files: {:?}",
        spawn_logs.len(),
        spawn_events.len(),
        attempt_events.len(),
        spawn_logs
            .iter()
            .map(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
            .collect::<Vec<_>>()
    );
    assert_eq!(
        spawn_events.len(),
        1,
        "expected exactly one event:\"spawn\" under #691 fix, got {} (attempts: {}) — \
         full lifecycle: {:#?}",
        spawn_events.len(),
        attempt_events.len(),
        events
    );
}
