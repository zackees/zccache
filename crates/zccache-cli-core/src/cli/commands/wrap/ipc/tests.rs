//! Focused tests for wrapper IPC delivery, retry, wedge, and relay behavior.

use super::*;
use std::sync::Arc;

fn failed(message: &str) -> CompileRecvOutcome {
    CompileRecvOutcome::Failed(TransportFailure {
        message: message.to_string(),
        phase: FailurePhase::DeliveryUnknown,
        explicit_wire_mismatch: false,
    })
}

fn predispatch_failed(message: &str) -> CompileRecvOutcome {
    CompileRecvOutcome::Failed(TransportFailure {
        message: message.to_string(),
        phase: FailurePhase::PreDispatch,
        explicit_wire_mismatch: false,
    })
}

#[test]
fn bincode_retry_requires_auto_and_an_explicit_wire_rejection() {
    let rejected = CompileRecvOutcome::Failed(TransportFailure {
        message: "protocol version mismatch".to_string(),
        phase: FailurePhase::DeliveryUnknown,
        explicit_wire_mismatch: true,
    });
    assert!(outcome_requires_bincode_retry(
        crate::protocol::wire_prost::ClientWireSelection::Auto,
        &rejected
    ));
    assert!(!outcome_requires_bincode_retry(
        crate::protocol::wire_prost::ClientWireSelection::ProstV16,
        &rejected
    ));

    let application_error = CompileRecvOutcome::Done(Some(crate::protocol::Response::Error {
        message: "nested protocol version mismatch".to_string(),
    }));
    assert!(!outcome_requires_bincode_retry(
        crate::protocol::wire_prost::ClientWireSelection::Auto,
        &application_error
    ));

    let ambiguous_close = failed("broken connection to daemon: connection closed");
    assert!(!outcome_requires_bincode_retry(
        crate::protocol::wire_prost::ClientWireSelection::Auto,
        &ambiguous_close
    ));

    let version_mismatch = CompileRecvOutcome::Failed(TransportFailure {
        message: "protocol version mismatch".to_string(),
        phase: FailurePhase::DeliveryUnknown,
        explicit_wire_mismatch: true,
    });
    assert!(outcome_requires_bincode_retry(
        crate::protocol::wire_prost::ClientWireSelection::Auto,
        &version_mismatch
    ));
}

#[test]
fn compile_response_relay_writes_stdout_stderr_and_exit_code() {
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();

    let exit = relay_compile_response(
        Some(crate::protocol::Response::CompileResult {
            exit_code: 7,
            stdout: Arc::new(b"compiler-out".to_vec()),
            stderr: Arc::new(b"compiler-err".to_vec()),
            cached: false,
        }),
        &mut stdout,
        &mut stderr,
    );

    assert_eq!(exit, RelayOutcome::Verdict(ExitCode::from(7)));
    assert_eq!(stdout, b"compiler-out");
    assert_eq!(stderr, b"compiler-err");
}

#[test]
fn compile_response_relay_colors_only_unknown_warning_on_terminal() {
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let warning = b"compiler-err\nzccache[warn][M]: unknown branch=test\n";

    let outcome = relay_compile_response_with_color(
        Some(crate::protocol::Response::CompileResult {
            exit_code: 0,
            stdout: Arc::new(Vec::new()),
            stderr: Arc::new(warning.to_vec()),
            cached: false,
        }),
        &mut stdout,
        &mut stderr,
        true,
    );

    assert_eq!(outcome, RelayOutcome::Verdict(ExitCode::SUCCESS));
    assert_eq!(
        String::from_utf8(stderr).unwrap(),
        "compiler-err\n\x1b[33mzccache[warn][M]: unknown branch=test\x1b[0m\n"
    );
}

#[test]
fn daemon_error_is_a_no_verdict_failure_not_a_local_fallback_signal() {
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let outcome = relay_compile_response(
        Some(crate::protocol::Response::Error {
            message: "cache staging failed".to_string(),
        }),
        &mut stdout,
        &mut stderr,
    );

    assert!(
        matches!(outcome, RelayOutcome::NoVerdict(message) if message.contains("cache staging failed"))
    );
    assert!(stdout.is_empty());
    assert!(stderr.is_empty());
}

#[test]
fn missing_response_is_a_no_verdict_failure() {
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let outcome = relay_compile_response(None, &mut stdout, &mut stderr);

    assert!(matches!(outcome, RelayOutcome::NoVerdict(_)));
}

// ── Issue #666: wedge-detection helper ──────────────────────────────
//
// Verifies that `compile_recv_with_wedge_detection`:
//   • returns `Done` on a normal response,
//   • returns `Wedged` only when the underlying recv times out,
//   • returns `Failed` (not `Wedged`) on a non-timeout transport error,
//   • respects the disabled (`secs == 0`) configuration.

struct FakeConn {
    behavior: FakeBehavior,
}

#[allow(clippy::large_enum_variant)]
enum FakeBehavior {
    Ok(crate::protocol::Response),
    TimesOut,
    BrokenPipe,
    /// Issue #1216: a timed script of frames. Each step sleeps for its
    /// delay and then yields its frame; once the script is exhausted the
    /// fake behaves like [`FakeBehavior::TimesOut`], so a test only has
    /// to enumerate the frames it cares about.
    Scripted(std::collections::VecDeque<(std::time::Duration, crate::protocol::Response)>),
}

impl ConnRecv for FakeConn {
    async fn recv_with_timeout(
        &mut self,
        timeout: std::time::Duration,
        _wire: crate::protocol::wire_prost::WireFormat,
    ) -> Result<Option<crate::protocol::Response>, crate::ipc::IpcError> {
        match &mut self.behavior {
            FakeBehavior::Ok(r) => Ok(Some(r.clone())),
            FakeBehavior::TimesOut => {
                tokio::time::sleep(timeout).await;
                Err(crate::ipc::IpcError::Timeout(timeout))
            }
            FakeBehavior::BrokenPipe => Err(crate::ipc::IpcError::ConnectionClosed),
            FakeBehavior::Scripted(steps) => match steps.pop_front() {
                Some((delay, response)) if delay < timeout => {
                    tokio::time::sleep(delay).await;
                    Ok(Some(response))
                }
                // The scripted gap exceeds the caller's budget: the recv
                // must trip, exactly as the real transport would.
                Some(_) | None => {
                    tokio::time::sleep(timeout).await;
                    Err(crate::ipc::IpcError::Timeout(timeout))
                }
            },
        }
    }
}

fn progress(queue_position: u32, queue_depth: u32) -> crate::protocol::Response {
    crate::protocol::Response::CompileProgress {
        queue_position,
        queue_depth,
        in_flight: 8,
        phase: "queued".to_string(),
    }
}

fn compile_result(exit_code: i32) -> crate::protocol::Response {
    crate::protocol::Response::CompileResult {
        exit_code,
        stdout: std::sync::Arc::new(Vec::new()),
        stderr: std::sync::Arc::new(Vec::new()),
        cached: true,
    }
}

// Test-only budget: 1 s mirrors the prior env-var convention but is
// injected directly so parallel tests can't race the process-global env
// (#745). The matching test for the env-var parser lives in
// `crate::cli` next to `wedge_recv_timeout`.
const TEST_BUDGET: Option<std::time::Duration> = Some(std::time::Duration::from_secs(1));

#[tokio::test]
async fn wedge_detection_returns_done_on_normal_response() {
    let mut conn = FakeConn {
        behavior: FakeBehavior::Ok(crate::protocol::Response::Pong),
    };
    let outcome = compile_recv_with_wedge_detection(
        &mut conn,
        TEST_BUDGET,
        crate::protocol::wire_prost::WireFormat::BincodeV15,
    )
    .await;
    assert!(matches!(
        outcome,
        CompileRecvOutcome::Done(Some(crate::protocol::Response::Pong))
    ));
}

#[tokio::test(start_paused = true)]
async fn wedge_detection_returns_wedged_on_recv_timeout() {
    // Pre-#666 this path inherited the 300 s global default and the
    // whole build paid that wall × N workers.
    //
    // Issue #717: `start_paused = true` + `tokio::time::Instant` make
    // the elapsed measurement deterministic against the configured
    // budget instead of wall-clock-dependent.
    //
    // Issue #745: the budget is now an explicit parameter, so parallel
    // tests can't race the `ZCCACHE_WEDGE_RECV_TIMEOUT_SECS` env var
    // out from under each other and accidentally surface the 180 s
    // default mid-recv.
    let mut conn = FakeConn {
        behavior: FakeBehavior::TimesOut,
    };
    let started = tokio::time::Instant::now();
    let outcome = compile_recv_with_wedge_detection(
        &mut conn,
        TEST_BUDGET,
        crate::protocol::wire_prost::WireFormat::BincodeV15,
    )
    .await;
    let elapsed = started.elapsed();
    assert!(matches!(outcome, CompileRecvOutcome::Wedged));
    // Lower bound: the wedge budget was actually respected (no early
    // false-positive). Upper bound: fail-fast at the configured budget
    // with a tight margin for the post-timeout return path. Both bounds
    // measure tokio-virtual time, not wall clock.
    assert!(
        elapsed >= std::time::Duration::from_secs(1)
            && elapsed < std::time::Duration::from_millis(1100),
        "wedge detection took {elapsed:?} against a never-responding fake; \
             issue #666 expects fail-fast at the configured budget"
    );
}

#[tokio::test(start_paused = true)]
async fn compile_progress_heartbeats_reset_the_wedge_budget() {
    // Issue #1216: three 900 ms gaps total 2.7 s — nearly 3× the 1 s
    // budget. Pre-#1216 the single blocking recv would have tripped at
    // 1 s and classified a perfectly healthy, queued compile as wedged.
    // Every frame (including non-terminal ones) restarts the budget, so
    // the terminal result is relayed instead.
    let gap = std::time::Duration::from_millis(900);
    let mut conn = FakeConn {
        behavior: FakeBehavior::Scripted(
            [
                (gap, progress(2, 3)),
                (gap, progress(1, 2)),
                (gap, compile_result(0)),
            ]
            .into_iter()
            .collect(),
        ),
    };
    let started = tokio::time::Instant::now();
    let outcome = compile_recv_with_wedge_detection(
        &mut conn,
        TEST_BUDGET,
        crate::protocol::wire_prost::WireFormat::BincodeV15,
    )
    .await;
    let elapsed = started.elapsed();
    assert!(
        matches!(
            outcome,
            CompileRecvOutcome::Done(Some(crate::protocol::Response::CompileResult {
                exit_code: 0,
                cached: true,
                ..
            }))
        ),
        "queued-but-progressing compile must deliver its terminal result on \
             the original connection"
    );
    assert!(
        elapsed >= std::time::Duration::from_millis(2700),
        "the loop must actually have waited out all three gaps, got {elapsed:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn silence_after_heartbeats_still_trips_wedge_detection() {
    // The complement of the test above: a daemon that heartbeats and then
    // goes completely quiet is genuinely wedged and must still get the
    // #753/#955 treatment. Progress-based detection must not become
    // "never detect a wedge".
    let mut conn = FakeConn {
        behavior: FakeBehavior::Scripted(
            [(std::time::Duration::from_millis(900), progress(4, 5))]
                .into_iter()
                .collect(),
        ),
    };
    let outcome = compile_recv_with_wedge_detection(
        &mut conn,
        TEST_BUDGET,
        crate::protocol::wire_prost::WireFormat::BincodeV15,
    )
    .await;
    assert!(
        matches!(outcome, CompileRecvOutcome::Wedged),
        "an exhausted script (no further frames) must trip the budget"
    );
}

#[tokio::test]
async fn compile_progress_is_never_relayed_as_a_terminal_response() {
    // Belt-and-braces: `relay_compile_response` treats any unexpected
    // variant as a hard `[U]` failure, so a heartbeat that leaked past the
    // recv loop would fail the compile. Assert the loop swallows it.
    let mut conn = FakeConn {
        behavior: FakeBehavior::Scripted(
            [
                (std::time::Duration::ZERO, progress(0, 0)),
                (std::time::Duration::ZERO, compile_result(7)),
            ]
            .into_iter()
            .collect(),
        ),
    };
    let outcome = compile_recv_with_wedge_detection(
        &mut conn,
        TEST_BUDGET,
        crate::protocol::wire_prost::WireFormat::BincodeV15,
    )
    .await;
    let CompileRecvOutcome::Done(response) = outcome else {
        panic!("expected a terminal response");
    };
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let relayed = relay_compile_response(response, &mut stdout, &mut stderr);
    assert_eq!(relayed, RelayOutcome::Verdict(exit_code_from_i32(7)));
}

#[test]
fn compile_progress_line_names_position_depth_and_in_flight() {
    assert_eq!(
        compile_progress_line(2, 5, 8, "queued"),
        "zccache[info][Q]: daemon under load: queued, position 2 of 5 queued, 8 in flight"
    );
    // Nothing queued: the position/depth pair carries no information, so
    // the line drops it rather than printing "position 0 of 0".
    assert_eq!(
        compile_progress_line(0, 0, 3, "compiling"),
        "zccache[info][Q]: daemon under load: compiling, 3 compiles in flight"
    );
}

#[tokio::test]
async fn wedge_detection_does_not_misclassify_broken_pipe_as_wedge() {
    // A non-timeout transport error must NOT trigger the recovery path
    // (force-killing the daemon on every protocol mismatch would be a
    // worse cure than the disease).
    let mut conn = FakeConn {
        behavior: FakeBehavior::BrokenPipe,
    };
    let outcome = compile_recv_with_wedge_detection(
        &mut conn,
        TEST_BUDGET,
        crate::protocol::wire_prost::WireFormat::BincodeV15,
    )
    .await;
    assert!(matches!(outcome, CompileRecvOutcome::Failed(_)));
}

// ── Issue #752: link retry on transport failure ────────────────────
//
// `cmd_link_ephemeral` / `cmd_compile_ephemeral` used to bail with
// `ExitCode::FAILURE` on any `CompileRecvOutcome::Failed` — including
// "daemon went away mid-recv" under FastLED's parallel-link storm
// (`lost connection to daemon`; FastLED/FastLED#3011). The recovery
// the error message itself recommends (`zccache stop` + retry) is
// now applied automatically: on a transport-level Failed, kill the
// stale daemon, spawn a fresh one (via the caller's recover hook),
// and re-run the attempt. Bounded retry — at most `max_recoveries`
// recoveries — so a real bug still surfaces.

#[tokio::test]
async fn link_retry_returns_done_when_first_attempt_succeeds() {
    let attempts = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
    let recoveries = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
    let outcome = link_with_retry(
        || {
            attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            async { CompileRecvOutcome::Done(Some(crate::protocol::Response::Pong)) }
        },
        || {
            recoveries.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            async {}
        },
        1,
    )
    .await;
    assert!(matches!(
        outcome,
        CompileRecvOutcome::Done(Some(crate::protocol::Response::Pong))
    ));
    assert_eq!(attempts.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(recoveries.load(std::sync::atomic::Ordering::SeqCst), 0);
}

#[tokio::test]
async fn link_retry_recovers_after_one_predispatch_failure() {
    let attempts = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
    let recoveries = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
    let outcome = link_with_retry(
        || {
            let n = attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
            async move {
                if n == 1 {
                    predispatch_failed("cannot connect to daemon")
                } else {
                    CompileRecvOutcome::Done(Some(crate::protocol::Response::Pong))
                }
            }
        },
        || {
            recoveries.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            async {}
        },
        1,
    )
    .await;
    assert!(
        matches!(
            outcome,
            CompileRecvOutcome::Done(Some(crate::protocol::Response::Pong))
        ),
        "retry should recover a request that never reached the daemon (#752/#1417)"
    );
    assert_eq!(attempts.load(std::sync::atomic::Ordering::SeqCst), 2);
    assert_eq!(recoveries.load(std::sync::atomic::Ordering::SeqCst), 1);
}

#[tokio::test]
async fn link_retry_surfaces_failure_after_exhausting_budget() {
    let attempts = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
    let recoveries = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
    let outcome = link_with_retry(
        || {
            attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            async { predispatch_failed("daemon really gone") }
        },
        || {
            recoveries.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            async {}
        },
        1,
    )
    .await;
    assert!(matches!(outcome, CompileRecvOutcome::Failed(_)));
    assert_eq!(
        attempts.load(std::sync::atomic::Ordering::SeqCst),
        2,
        "exactly the initial attempt plus one retry — no infinite loop"
    );
    assert_eq!(recoveries.load(std::sync::atomic::Ordering::SeqCst), 1);
}

#[tokio::test]
async fn link_retry_never_replays_ambiguous_delivery() {
    let attempts = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
    let recoveries = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
    let outcome = link_with_retry(
        || {
            attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            async { failed("connection closed after request send") }
        },
        || {
            recoveries.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            async {}
        },
        5,
    )
    .await;
    assert!(matches!(
        outcome,
        CompileRecvOutcome::Failed(TransportFailure {
            phase: FailurePhase::DeliveryUnknown,
            ..
        })
    ));
    assert_eq!(attempts.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(recoveries.load(std::sync::atomic::Ordering::SeqCst), 0);
}

#[tokio::test]
async fn link_retry_does_not_retry_on_wedge() {
    // Wedge has its own kill-daemon path on the compile arm and is
    // intentionally fail-fast on the ephemeral arms (per #666).
    // The retry helper must not turn Wedged into a recovery loop.
    let attempts = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
    let recoveries = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
    let outcome = link_with_retry(
        || {
            attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            async { CompileRecvOutcome::Wedged }
        },
        || {
            recoveries.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            async {}
        },
        5,
    )
    .await;
    assert!(matches!(outcome, CompileRecvOutcome::Wedged));
    assert_eq!(attempts.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(recoveries.load(std::sync::atomic::Ordering::SeqCst), 0);
}

#[tokio::test]
async fn link_retry_disabled_when_budget_is_zero() {
    // `link_retry_budget() == 0` (e.g. `ZCCACHE_DISABLE_LINK_RETRY=1`)
    // opts back into pre-#752 fail-fast behavior.
    let attempts = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
    let recoveries = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
    let outcome = link_with_retry(
        || {
            attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            async { failed("once") }
        },
        || {
            recoveries.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            async {}
        },
        0,
    )
    .await;
    assert!(matches!(outcome, CompileRecvOutcome::Failed(_)));
    assert_eq!(attempts.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(recoveries.load(std::sync::atomic::Ordering::SeqCst), 0);
}

// ── Issue #753: probe-before-kill classifier ──────────────────────
//
// The wedge guard in `cmd_compile`'s `Wedged` arm used to send
// `Shutdown` unconditionally — which #726 / FastLED/#3011 showed
// collapses legitimate in-flight work under burst-link load.
// `classify_probe_outcome` is the pure-function decision matrix
// the new probe-before-kill path consults; tests pass the three
// possible probe results directly so the matrix is pinned
// without standing up an IPC connection.

#[test]
fn classify_probe_outcome_pong_within_budget_means_no_kill() {
    // The probe came back inside its budget: daemon is alive and
    // answering. Don't kill — the original wedge was burst-load
    // backpressure, not a hung daemon.
    let probe: Result<Result<(), crate::ipc::IpcError>, tokio::time::error::Elapsed> = Ok(Ok(()));
    assert_eq!(classify_probe_outcome(probe), WedgeAction::DowngradeNoKill);
}

#[test]
fn classify_probe_outcome_probe_error_escalates_to_kill() {
    // Transport-level error before the budget expired (broken
    // pipe, version mismatch, connect refused). A daemon that
    // can't even accept a fresh connection is in worse shape than
    // a wedged one — escalate to kill.
    let probe: Result<Result<(), crate::ipc::IpcError>, tokio::time::error::Elapsed> =
        Ok(Err(crate::ipc::IpcError::ConnectionClosed));
    assert_eq!(
        classify_probe_outcome(probe),
        WedgeAction::EscalateKillProbeError
    );
}

#[test]
fn classify_probe_outcome_probe_timeout_escalates_to_kill() {
    // Probe itself timed out: daemon isn't even answering Pings,
    // run the existing kill+respawn recovery.
    //
    // Construct an `Elapsed` via a 0-ms timeout that fires
    // immediately so the test stays deterministic without
    // depending on tokio runtime timing.
    let elapsed = {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        rt.block_on(async {
            tokio::time::timeout(
                std::time::Duration::from_nanos(1),
                std::future::pending::<()>(),
            )
            .await
            .unwrap_err()
        })
    };
    let probe: Result<Result<(), crate::ipc::IpcError>, tokio::time::error::Elapsed> = Err(elapsed);
    assert_eq!(classify_probe_outcome(probe), WedgeAction::EscalateKill);
}

#[test]
fn wedge_probe_budget_default_is_three_seconds() {
    // When `ZCCACHE_WEDGE_PROBE_BUDGET_MS` is unset, the budget
    // falls to the documented default. Read directly via
    // `WEDGE_PROBE_DEFAULT_MS` so the constant remains the single
    // source of truth — no env mutation in the test (#745).
    assert_eq!(
        WEDGE_PROBE_DEFAULT_MS, 3_000,
        "schema commits to 3s default — tooling docs reference this number"
    );
}

#[tokio::test(start_paused = true)]
async fn wedge_detection_disabled_when_budget_is_none() {
    // `budget = None` opts out of wedge classification/respawn while
    // keeping the IPC layer's 300 s default recv timeout (used in
    // production when `ZCCACHE_WEDGE_RECV_TIMEOUT_SECS=0`).
    let mut conn = FakeConn {
        behavior: FakeBehavior::TimesOut,
    };
    let outcome = compile_recv_with_wedge_detection(
        &mut conn,
        None,
        crate::protocol::wire_prost::WireFormat::BincodeV15,
    )
    .await;
    // Disabled means a timeout is surfaced as a normal failure, not a
    // wedge-triggering respawn.
    assert!(matches!(outcome, CompileRecvOutcome::Failed(_)));
}

#[test]
fn link_response_relay_preserves_warning_after_tool_stderr() {
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();

    let exit = relay_link_response(
        Some(crate::protocol::Response::LinkResult {
            exit_code: 0,
            stdout: Arc::new(b"link-out".to_vec()),
            stderr: Arc::new(b"link-err\n".to_vec()),
            cached: true,
            warning: Some("non-deterministic archive flags".to_string()),
        }),
        &mut stdout,
        &mut stderr,
    );

    assert_eq!(exit, RelayOutcome::Verdict(ExitCode::SUCCESS));
    assert_eq!(stdout, b"link-out");
    assert_eq!(
        String::from_utf8(stderr).unwrap(),
        "link-err\nzccache warning: non-deterministic archive flags\n"
    );
}
