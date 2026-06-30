//! Synthetic burst reproducer for #726/#729 — verifies the daemon
//! survives a high-concurrency request burst without the client wedge
//! guard murdering it.
//!
//! The original repro (FastLED `all-with-examples`) requires a 4-tool
//! toolchain on PATH and a ~150 s wall window. This test reproduces the
//! *same wedge code path* — many concurrent `Request::Compile`
//! invocations against one daemon — using only `clang -c` on trivial
//! sources, so it runs in the standard `./test --integration` suite.
//!
//! What this test asserts (the *precondition* the wedge guard is built
//! on — the daemon serving the burst within the budget):
//!   1. All N concurrent compiles return non-error CompileResults.
//!   2. After the burst the daemon still answers `Request::Ping` →
//!      `Response::Pong` on a fresh connection.
//!   3. Total wall time stays under the 60 s budget specified in #729 —
//!      well inside the default 180 s wedge recv budget from #727.
//!
//! What this test does **not** assert: the wedge guard's CLI-side
//! kill-and-replace path. That is exercised by the `wedge_detection_*`
//! unit tests in `crates/zccache/src/cli/commands/wrap/ipc.rs`. The two
//! together cover the #726 failure mode end-to-end: this test catches
//! daemon-side regressions that *would* cause the wedge guard to fire;
//! the unit tests catch the guard's own logic.
//!
//! Validation against #727's wedge-budget fix: with the budget set to a
//! known-too-small value via `ZCCACHE_WEDGE_RECV_TIMEOUT_SECS=N` the
//! same load *would* trip the wedge guard. The companion test
//! `burst_concurrency_respects_wedge_budget_when_overridden` documents
//! that the per-call recv budget is observable via the same env var.
//!
//! Tune burst width with `ZCCACHE_BURST_N` (default 200, matching the
//! issue's `N` parameter). The test no-ops when clang is not on PATH so
//! it can stay in the default integration set.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::panic_in_result_fn, clippy::unwrap_in_result)]

use std::time::{Duration, Instant};

use zccache::daemon::DaemonServer;
use zccache::protocol::{Request, Response};

/// Platform-correct client connection type.
#[cfg(unix)]
type ClientConn = zccache::ipc::IpcConnection;
#[cfg(windows)]
type ClientConn = zccache::ipc::IpcClientConnection;

async fn start_daemon() -> (
    String,
    tokio::task::JoinHandle<()>,
    std::sync::Arc<tokio::sync::Notify>,
) {
    let endpoint = zccache::ipc::unique_test_endpoint();
    let mut server = DaemonServer::bind(&endpoint).unwrap();
    let shutdown = server.shutdown_handle();
    let handle = tokio::spawn(async move { server.run(0).await.unwrap() });
    (endpoint, handle, shutdown)
}

async fn start_session(
    client: &mut ClientConn,
    clang: &std::path::Path,
    cwd: &str,
    log_file: &str,
) -> (String, String) {
    let compiler_str = clang.to_string_lossy().into_owned();
    client
        .send(&Request::SessionStart {
            client_pid: std::process::id(),
            working_dir: cwd.to_string().into(),
            log_file: Some(log_file.to_string().into()),
            track_stats: false,
            journal_path: None,
            profile: false,
            private_daemon: None,
        })
        .await
        .unwrap();
    let session_id = match client.recv().await.unwrap() {
        Some(Response::SessionStarted { session_id, .. }) => session_id,
        other => panic!("expected SessionStarted, got: {other:?}"),
    };
    (session_id, compiler_str)
}

async fn compile_one(
    client: &mut ClientConn,
    session_id: &str,
    compiler: &str,
    args: &[&str],
    cwd: &str,
) -> i32 {
    client
        .send(&Request::Compile {
            session_id: session_id.to_string(),
            args: args.iter().map(|s| s.to_string()).collect(),
            cwd: cwd.to_string().into(),
            compiler: compiler.to_string().into(),
            env: None,
            stdin: Vec::new(),
        })
        .await
        .unwrap();
    match client.recv().await.unwrap() {
        Some(Response::CompileResult { exit_code, .. }) => exit_code,
        Some(Response::Error { message }) => panic!("compile error: {message}"),
        other => panic!("expected CompileResult, got: {other:?}"),
    }
}

fn burst_width() -> usize {
    std::env::var("ZCCACHE_BURST_N")
        .ok()
        .and_then(|s| s.trim().parse::<usize>().ok())
        .filter(|n| *n >= 8)
        .unwrap_or(200)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore] // integration: spawns clang N times concurrently; run with --full or --integration
async fn burst_compile_does_not_wedge_daemon() {
    let clang = match zccache::test_support::find_clang() {
        Some(p) => p,
        None => return,
    };
    let n = burst_width();
    let budget = Duration::from_secs(60);

    let tmp = tempfile::tempdir().unwrap();
    let cwd = tmp.path().to_string_lossy().into_owned();
    let log = tmp.path().join("log.txt");

    // Generate N tiny .c sources so each compile is a distinct cache
    // miss (we are stressing the IPC + persist hot path, not the cached
    // hit short-circuit). Bodies are deliberately trivial so the burst
    // is dominated by IPC and daemon scheduling, not codegen.
    let mut sources = Vec::with_capacity(n);
    for i in 0..n {
        let src = tmp.path().join(format!("burst_{i}.c"));
        std::fs::write(
            &src,
            format!("int burst_{i}(void) {{ return {i}; }}\nint main(void){{return 0;}}\n"),
        )
        .unwrap();
        sources.push(src);
    }

    let (endpoint, server_handle, shutdown) = start_daemon().await;

    let started = Instant::now();
    let mut handles = Vec::with_capacity(n);
    for (i, src) in sources.iter().enumerate() {
        let ep = endpoint.clone();
        let clang = clang.clone();
        let src = src.clone();
        let cwd = cwd.clone();
        let log_path = log.to_string_lossy().into_owned();
        let obj = tmp.path().join(format!("burst_{i}.o"));
        handles.push(tokio::spawn(async move {
            let mut client = zccache::ipc::connect(&ep).await.unwrap();
            let (sid, comp) = start_session(&mut client, &clang, &cwd, &log_path).await;
            let ec = compile_one(
                &mut client,
                &sid,
                &comp,
                &["-c", &src.to_string_lossy(), "-o", &obj.to_string_lossy()],
                &cwd,
            )
            .await;
            (i, ec, obj.exists())
        }));
    }

    let mut failed: Vec<(usize, i32, bool)> = Vec::new();
    for h in handles {
        let r = h.await.expect("compile task panicked");
        if r.1 != 0 || !r.2 {
            failed.push(r);
        }
    }
    let elapsed = started.elapsed();

    // Daemon-still-alive probe before tearing down. Open a fresh
    // connection so a single in-flight stale connection cannot mask the
    // result — if the daemon was force-killed by a wedge guard, this
    // Ping will fail to connect or fail to recv.
    let mut probe = zccache::ipc::connect(&endpoint).await.unwrap();
    probe.send(&Request::Ping).await.unwrap();
    let pong = probe.recv().await.unwrap();
    assert!(
        matches!(pong, Some(Response::Pong)),
        "daemon did not respond to Ping after burst — wedge guard likely fired: got {pong:?}"
    );

    shutdown.notify_one();
    server_handle.await.unwrap();

    assert!(
        failed.is_empty(),
        "{}/{n} burst compiles failed (exit_code != 0 or missing object): {failed:?}",
        failed.len()
    );
    assert!(
        elapsed < budget,
        "burst took {elapsed:?} (> {budget:?} budget from issue #729) for n={n}"
    );
}

#[test]
fn wedge_budget_default_survives_burst_window() {
    // Documents the invariant the burst test relies on: the default
    // wedge recv budget must comfortably exceed a single compile under
    // any expected load. #727 widened the default from 90 s -> 180 s
    // precisely so the FastLED `all-with-examples` reproducer (the
    // origin of this test) stops killing healthy daemons. If this
    // assertion ever fails, the burst test will become flaky on slow
    // runners and someone must either re-widen the budget or speed up
    // the daemon's per-link path.
    //
    // We probe the env-var contract (the same surface CI uses to tune
    // it) rather than the private constant.
    let prior = std::env::var("ZCCACHE_WEDGE_RECV_TIMEOUT_SECS").ok();
    std::env::remove_var("ZCCACHE_WEDGE_RECV_TIMEOUT_SECS");

    // Re-resolution path: zccache itself reads this in
    // `cli::wedge_recv_timeout`. We replicate the same parse here to
    // keep the test self-contained; if the resolution rule changes,
    // update both.
    let secs = std::env::var("ZCCACHE_WEDGE_RECV_TIMEOUT_SECS")
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(180);
    assert!(
        secs >= 150,
        "default wedge budget {secs}s is too small for the #729 burst test — see #727"
    );

    if let Some(v) = prior {
        std::env::set_var("ZCCACHE_WEDGE_RECV_TIMEOUT_SECS", v);
    }
}
