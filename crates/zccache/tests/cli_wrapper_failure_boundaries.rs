//! Executable-level tests for the wrapper/daemon failure safety contract.
//!
//! These tests intentionally exercise the published wrapper binary. Since
//! #1170 the contract they prove is the opposite of the original one: a
//! pre-dispatch daemon failure must NOT run the compiler at all. It fails
//! with the wrapper's own infrastructure exit code and a durable event, so a
//! daemon outage can never present as a green build.

#![allow(
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_in_result,
    clippy::unwrap_used
)]

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

const SHUTDOWN_POLL_INTERVAL: Duration = Duration::from_millis(25);

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

fn stop_daemon(zccache: &Path, cache_dir: &Path) -> Output {
    Command::new(zccache)
        .arg("stop")
        .env("ZCCACHE_CACHE_DIR", cache_dir)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .expect("run zccache stop")
}

fn run_wrapper(
    zccache: &Path,
    echo_shim: &Path,
    cache_dir: &Path,
    payload: &[u8],
    session_id: Option<&str>,
) -> Output {
    let mut command = Command::new(zccache);
    command
        .arg(echo_shim)
        .arg("7")
        .env("ZCCACHE_CACHE_DIR", cache_dir)
        .env("ZCCACHE_NO_SPAWN", "1")
        .env("ZCCACHE_DAEMON_WIRE", "bincode")
        // These tests assert that the refusal contract holds. They must not
        // inherit the sanctioned bypasses they exist to contrast against:
        // `ZCCACHE_DISABLE` and `ZCCACHE_PROBE_BYPASS` both passthrough-exec
        // the tool *before* any endpoint resolution, so an inherited one turns
        // "refused with 125" into "ran the tool and mirrored its exit code"
        // with no assertion able to tell the difference.
        //
        // This is not hypothetical: the CI step added for #1317 set
        // `ZCCACHE_DISABLE=1` for journal hygiene, and both tests failed with
        // `left: Some(7)` — the shim's own code. Reproduced on Windows by
        // exporting the same variable, so it was never platform-specific.
        //
        // `ZCCACHE_ENDPOINT` is removed too: `resolve_endpoint` honours it
        // *ahead of* the cache dir, so an inherited value would silently point
        // these tests at a real daemon and defeat the tempdir isolation.
        .env_remove("ZCCACHE_DISABLE")
        .env_remove("ZCCACHE_PROBE_BYPASS")
        .env_remove("ZCCACHE_ENDPOINT")
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

fn daemon_pid(cache_dir: &Path) -> u32 {
    lifecycle_events(cache_dir)
        .into_iter()
        .rev()
        .find(|event| event["event"] == "spawn")
        .and_then(|event| event["pid"].as_u64())
        .and_then(|pid| u32::try_from(pid).ok())
        .expect("current daemon spawn event with a valid PID")
}

/// Wait until the specific daemon is genuinely unreachable, not merely until
/// it said it was going away.
///
/// The `died-shutdown` lifecycle event is published *before* the listener
/// stops accepting, so a wrapper run started on that signal alone can still
/// connect, dispatch, and have the daemon run the tool — returning the tool's
/// exit code (7) where the test demands the wrapper's refusal (125). Windows
/// timing hid this; Linux CI failed on it intermittently.
///
/// So the event is the *first* gate and reachability is the second: `zccache
/// status` exits non-zero once nothing answers the endpoint, which is the
/// property the refusal contract actually depends on.
///
/// Graceful shutdown is setup for this test, not the contract under test. If
/// the matching daemon has published its terminal event but still answers,
/// kill that exact PID rather than waiting indefinitely for unrelated cleanup.
fn wait_for_daemon_shutdown(zccache: &Path, cache_dir: &Path, daemon_pid: u32) {
    let deadline = Instant::now() + zccache::test_support::INTEGRATION_TEST_TIMEOUT;
    let mut forced_kill = false;
    loop {
        let saw_matching_event = lifecycle_events(cache_dir)
            .iter()
            .any(|event| event["event"] == "died-shutdown" && event["pid"] == daemon_pid);
        let endpoint_reachable = daemon_answers(zccache, cache_dir);
        if saw_matching_event && !endpoint_reachable {
            return;
        }
        if saw_matching_event && endpoint_reachable && !forced_kill {
            zccache::ipc::force_kill_process(daemon_pid)
                .unwrap_or_else(|error| panic!("kill test daemon {daemon_pid}: {error}"));
            forced_kill = true;
        }
        if Instant::now() >= deadline {
            panic!(
                "daemon {daemon_pid} shutdown incomplete after {:?}: \
                 matching died-shutdown event={saw_matching_event}, \
                 endpoint_reachable={endpoint_reachable}, forced_kill={forced_kill}",
                zccache::test_support::INTEGRATION_TEST_TIMEOUT
            );
        }
        std::thread::sleep(SHUTDOWN_POLL_INTERVAL);
    }
}

/// Does anything still answer on this cache dir's endpoint?
fn daemon_answers(zccache: &Path, cache_dir: &Path) -> bool {
    Command::new(zccache)
        .arg("status")
        .env("ZCCACHE_CACHE_DIR", cache_dir)
        .env("ZCCACHE_NO_SPAWN", "1")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// The wrapper-infrastructure exit code from #1170. Kept as a literal here
/// rather than imported: these tests stand in for the external consumers
/// (soldr, fbuild, CI classifiers) that see only the process exit status, so
/// they should break if the number changes, not follow it.
const DAEMON_UNAVAILABLE_EXIT_CODE: i32 = 125;

/// Assert the run recorded exactly one daemon-unavailable refusal, and that
/// nothing recorded the retired `wrapper-local-fallback`.
fn assert_one_refusal(cache_dir: &Path, test_name: &'static str) {
    let events = lifecycle_events(cache_dir);
    let refusals: Vec<_> = events
        .iter()
        .filter(|event| event["event"] == "wrapper-daemon-unavailable")
        .collect();
    assert_eq!(
        refusals.len(),
        1,
        "expected exactly one daemon-unavailable event: {events:#?}"
    );
    assert_eq!(refusals[0]["phase"], "pre-dispatch");
    assert_eq!(refusals[0]["exit_code"], DAEMON_UNAVAILABLE_EXIT_CODE);
    assert!(
        !events
            .iter()
            .any(|event| event["event"] == "wrapper-local-fallback"),
        "the silent local fallback is retired and must not reappear: {events:#?}"
    );

    let effective =
        zccache::core::config::effective_cache_root_from_top_level(&cache_dir.to_path_buf().into());
    let report = zccache::audit::audit_cache_root(
        &effective,
        zccache::audit::LogAuditContext::Integration,
        &zccache::audit::AuditOptions::default()
            .allow_for_test(test_name, [zccache::audit::RuleId("no-daemon-unavailable")]),
    )
    .expect("audit intentional refusal fixture");
    assert!(report.passed(), "{}", report.format_human());
    assert_eq!(report.test_allow_name.as_deref(), Some(test_name));
}

fn assert_tool_never_ran(output: &Output) {
    assert!(
        !output
            .stdout
            .windows(b"ZCCACHE_PASSTHROUGH_STDOUT_MARKER".len())
            .any(|window| window == b"ZCCACHE_PASSTHROUGH_STDOUT_MARKER"),
        "the wrapper must not run the tool uncached"
    );
}

/// #1170: a pre-dispatch daemon failure is a hard error with a distinct
/// infrastructure exit code.
///
/// The behaviour this replaces is the reason the issue exists: the old path
/// ran the compiler directly and *mirrored its exit code*, so an outage that
/// happened to compile fine exited 0 and left a green build hiding it. The
/// load-bearing assertions are therefore "not 0" and "the tool never ran" —
/// the shim would exit 7 if it were reached, so 125 also proves the code is
/// the wrapper's own and not the tool's.
#[test]
#[ignore = "integration test: launches the wrapper binary"]
fn ephemeral_pre_dispatch_failure_is_a_hard_error_with_the_infra_exit_code() {
    let zccache = binary_path("zccache");
    let echo_shim = binary_path("echo_shim");
    if !zccache.exists() || !echo_shim.exists() {
        eprintln!("skipping: required binaries are not built");
        return;
    }

    let cache_dir = tempfile::tempdir().expect("cache tempdir");
    let payload = b"pre-dispatch-stdin\0with-a-nul\n";
    let output = run_wrapper(&zccache, &echo_shim, cache_dir.path(), payload, None);
    let _ = stop_daemon(&zccache, cache_dir.path());

    assert_eq!(
        output.status.code(),
        Some(DAEMON_UNAVAILABLE_EXIT_CODE),
        "a daemon outage must fail with the wrapper's infra code, not the tool's 7"
    );
    assert_tool_never_ran(&output);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("zccache[err][D]:"),
        "stderr must use the daemon-unavailable severity prefix: {stderr}"
    );
    assert!(
        stderr.contains("cannot start daemon") || stderr.contains("cannot connect to daemon"),
        "stderr must carry the concrete daemon-failure reason: {stderr}"
    );
    assert!(
        stderr.contains("ZCCACHE_DISABLE=1"),
        "the refusal must name the sanctioned bypass: {stderr}"
    );
    assert_one_refusal(
        cache_dir.path(),
        "ephemeral_pre_dispatch_failure_is_a_hard_error_with_the_infra_exit_code",
    );
}

/// The session route degrades into the ephemeral route on connect failure, so
/// it reaches the same refusal — and must not slurp stdin a second time on the
/// way there. Stdin handling is the part of the old contract worth keeping:
/// the wrapper still reads it exactly once even though nothing replays it now.
#[test]
#[ignore = "integration test: launches the wrapper binary"]
fn session_pre_dispatch_failure_refuses_without_double_reading_stdin() {
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
    let daemon_pid = daemon_pid(cache_dir.path());
    let stop = stop_daemon(&zccache, cache_dir.path());
    assert!(
        stop.status.success(),
        "zccache stop failed for daemon {daemon_pid}: {}",
        String::from_utf8_lossy(&stop.stderr)
    );
    wait_for_daemon_shutdown(&zccache, cache_dir.path(), daemon_pid);

    let payload = b"session-refusal-stdin\0must-not-double-read\n";
    let output = run_wrapper(
        &zccache,
        &echo_shim,
        cache_dir.path(),
        payload,
        Some(&session_id),
    );
    let _ = stop_daemon(&zccache, cache_dir.path());

    assert_eq!(
        output.status.code(),
        Some(DAEMON_UNAVAILABLE_EXIT_CODE),
        "the session route must reach the same hard error as the ephemeral one"
    );
    assert_tool_never_ran(&output);
    // #1317: the name promised a stdin property nothing checked — the
    // distinctive payload was passed in and then ignored. The refusal path
    // never replays stdin, so the marker must not surface on either stream;
    // a wrapper that echoed or re-emitted what it slurped would show it here.
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    for (stream, text) in [("stdout", &stdout), ("stderr", &stderr)] {
        assert!(
            !text.contains("must-not-double-read"),
            "refusal must not replay the stdin payload; {stream} carried it: {text}"
        );
    }
    assert_one_refusal(
        cache_dir.path(),
        "session_pre_dispatch_failure_refuses_without_double_reading_stdin",
    );
}

/// #1325: the deploy directory must be *born* private, never tightened.
///
/// #1314 made `create_dir_all_private` apply the owner-only DACL at creation,
/// but nothing asserted the end-to-end result — and it was silently bypassed
/// for months of commits because `crash::install` (the first statement of the
/// CLI's `run_main`) created `<cache>/v<VERSION>` with a plain
/// `create_dir_all` before any private creator ran. The deploy's own
/// `create_dir_all_private` then early-returned on the existing directory and
/// `ensure_dir_private` tightened after the fact, leaving #1172's window open.
///
/// `insecure_deploy_dir` is the observable that distinguishes the two: it is
/// emitted only when a tighten was necessary. Asserting its absence on a fresh
/// cache root is what makes "born private" testable — a unit test on the
/// creation primitive cannot catch an upstream caller creating the directory
/// first, which is exactly how this regressed.
#[test]
#[ignore = "integration test: launches the wrapper binary"]
fn a_fresh_cache_root_never_needs_its_deploy_dir_tightened() {
    let zccache = binary_path("zccache");
    if !zccache.exists() {
        eprintln!("skipping: zccache binary is not built");
        return;
    }

    let cache_dir = tempfile::tempdir().expect("cache tempdir");
    // Any command that installs the crash guard and touches the daemon state
    // dir will do; `status` is the cheapest and starts no daemon.
    let output = Command::new(&zccache)
        .arg("status")
        .env("ZCCACHE_CACHE_DIR", cache_dir.path())
        .env("ZCCACHE_NO_SPAWN", "1")
        .env_remove("ZCCACHE_DISABLE")
        .output()
        .expect("run zccache status");

    let tightened: Vec<_> = lifecycle_events(cache_dir.path())
        .into_iter()
        .filter(|event| event["event"] == "insecure_deploy_dir")
        .collect();
    assert!(
        tightened.is_empty(),
        "the deploy directory must be created private, not tightened after the \
         fact — some caller is creating it with a plain create_dir_all before \
         the private creator runs (#1325): {tightened:#?}\nstderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}
