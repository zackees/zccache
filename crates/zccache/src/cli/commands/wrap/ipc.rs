//! Wrapper IPC request construction and response relay.

use crate::core::NormalizedPath;
use std::io::Write;
use std::path::Path;
use std::process::ExitCode;

use super::super::super::{link_retry_budget, wedge_recv_timeout};
use super::super::daemon::{ensure_daemon, stop_stale_daemon};
use super::super::util::{connect, exit_code_from_i32, slurp_stdin_if_piped, LOST_CONNECTION_MSG};

pub(super) async fn cmd_compile(
    endpoint: &str,
    session_id: &str,
    args: Vec<String>,
    cwd: NormalizedPath,
    compiler: NormalizedPath,
    client_env: Vec<(String, String)>,
) -> ExitCode {
    let stdin_bytes = slurp_stdin_if_piped();
    let mut conn = match connect(endpoint).await {
        Ok(c) => c,
        Err(e) => {
            eprintln!("zccache[err][C]: cannot connect to daemon at {endpoint}: {e}");
            return ExitCode::FAILURE;
        }
    };

    let wire = crate::protocol::wire_prost::full_family_wire_format_from_env();
    let request = crate::protocol::Request::Compile {
        session_id: session_id.to_string(),
        args: args.clone(),
        cwd: cwd.clone(),
        compiler: compiler.clone(),
        env: Some(client_env.clone()),
        stdin: stdin_bytes.clone(),
    };
    if let Err(e) = conn.send_request(&request, wire).await {
        eprintln!("zccache[err][S]: failed to send to daemon: {e}");
        return ExitCode::FAILURE;
    }

    match compile_recv_with_wedge_detection(&mut conn, wedge_recv_timeout()).await {
        CompileRecvOutcome::Done(recv_result) => {
            relay_compile_response(recv_result, &mut std::io::stdout(), &mut std::io::stderr())
        }
        CompileRecvOutcome::Wedged => {
            // Daemon wedged mid-compile (issue #666). Recovery: force-kill
            // the wedged daemon (releases the IPC endpoint and frees other
            // workers waiting on `pipe.connect()`), spawn a fresh one, and
            // retry the compile in ephemeral mode (the new daemon doesn't
            // have the old session). The fall-through to ephemeral loses
            // session-stats accounting for this single compile but keeps
            // the build alive — a much better trade than the pre-#666
            // 300 s wall × N workers behaviour.
            eprintln!(
                "zccache[warn][W]: daemon at {endpoint} appears wedged \
                 (no response within wedge budget); recovering — issue #666"
            );
            drop(conn);
            stop_stale_daemon(endpoint).await;
            cmd_compile_ephemeral(endpoint, compiler.as_path(), args, cwd, client_env).await
        }
        CompileRecvOutcome::Failed(msg) => {
            // #755 acceptance #3: log the dropout at the point of
            // failure so dashboards correlate against the spawn-attempt
            // that follows.
            emit_client_disconnected_event(
                endpoint,
                crate::core::lifecycle::CAUSE_COMM_ERROR,
                &msg,
            );
            eprintln!("zccache[err][R]: {msg}");
            ExitCode::FAILURE
        }
    }
}

#[allow(clippy::large_enum_variant)]
enum CompileRecvOutcome {
    // `Response` is large (cached compile result holds 2× Arc<Vec<u8>>),
    // but `CompileRecvOutcome` is only ever stack-local for one match arm
    // before being dropped — the extra indirection of Box would add an
    // allocation per request on the hot wrapper path for no real gain.
    Done(Option<crate::protocol::Response>),
    /// Daemon stopped responding within the configured wedge budget.
    Wedged,
    /// Non-timeout recv failure (broken pipe, deserialization error, etc.).
    Failed(String),
}

/// Wrap a compile-response recv with an optional wedge budget.
///
/// `budget = Some(d)` enables wedge detection; `budget = None` falls
/// through to an unbounded recv. Production callers pass
/// [`wedge_recv_timeout`] so the env knob still works; tests pass an
/// explicit value so they don't race the process-global env var (#745).
///
/// Returns [`CompileRecvOutcome::Wedged`] only for the specific
/// `IpcError::Timeout` signal — everything else (graceful close, broken
/// pipe, protocol error) maps to [`CompileRecvOutcome::Failed`] so the
/// caller does not respawn the daemon on errors that have nothing to do
/// with a wedge.
async fn compile_recv_with_wedge_detection<C: ConnRecv>(
    conn: &mut C,
    budget: Option<std::time::Duration>,
) -> CompileRecvOutcome {
    match budget {
        Some(budget) => match conn.recv_with_timeout(budget).await {
            Ok(opt) => CompileRecvOutcome::Done(opt),
            Err(crate::ipc::IpcError::Timeout(_)) => CompileRecvOutcome::Wedged,
            Err(e) => CompileRecvOutcome::Failed(format!("broken connection to daemon: {e}")),
        },
        None => match conn.recv().await {
            Ok(opt) => CompileRecvOutcome::Done(opt),
            Err(e) => CompileRecvOutcome::Failed(format!("broken connection to daemon: {e}")),
        },
    }
}

/// Tiny seam over the platform-specific IPC connection types so the
/// wedge-detection helper can be unit-tested without spinning up a real
/// pipe/socket. Two impls live below — one for Unix `IpcConnection`, one
/// for the Windows client-side `IpcClientConnection`.
trait ConnRecv {
    async fn recv(&mut self) -> Result<Option<crate::protocol::Response>, crate::ipc::IpcError>;
    async fn recv_with_timeout(
        &mut self,
        timeout: std::time::Duration,
    ) -> Result<Option<crate::protocol::Response>, crate::ipc::IpcError>;
}

/// Drive a link/compile request through bounded retry on transport
/// failure. The closures are called in sequence:
///
///   * `attempt()` performs one full ensure-daemon → connect →
///     send-request → recv cycle and returns the resulting
///     [`CompileRecvOutcome`].
///   * `recover()` is called between attempts on a
///     [`CompileRecvOutcome::Failed`] outcome. In production this is a
///     jittered backoff (`retry_backoff_with_jitter`) — NOT a daemon
///     kill: `ensure_daemon`'s next call already detects a dead
///     daemon (probe → CommError → stop + respawn) and a parallel
///     worker may have just spawned a healthy daemon we must not
///     racingly tear down.
///
/// Only [`CompileRecvOutcome::Failed`] triggers retry — wedge has its
/// own kill-daemon path on the compile arm and is intentionally
/// fail-fast on the ephemeral arms per #666. Issue #752 (FastLED
/// `lost connection to daemon` under parallel-link storm).
async fn link_with_retry<A, AF, R, RF>(
    mut attempt: A,
    mut recover: R,
    max_recoveries: u32,
) -> CompileRecvOutcome
where
    A: FnMut() -> AF,
    AF: std::future::Future<Output = CompileRecvOutcome>,
    R: FnMut() -> RF,
    RF: std::future::Future<Output = ()>,
{
    let mut outcome = attempt().await;
    let mut recoveries_used = 0;
    while matches!(outcome, CompileRecvOutcome::Failed(_)) && recoveries_used < max_recoveries {
        recover().await;
        recoveries_used += 1;
        outcome = attempt().await;
    }
    outcome
}

#[cfg(unix)]
impl ConnRecv for crate::ipc::IpcConnection {
    async fn recv(&mut self) -> Result<Option<crate::protocol::Response>, crate::ipc::IpcError> {
        crate::ipc::IpcConnection::recv_response(self).await
    }
    async fn recv_with_timeout(
        &mut self,
        timeout: std::time::Duration,
    ) -> Result<Option<crate::protocol::Response>, crate::ipc::IpcError> {
        crate::ipc::IpcConnection::recv_response_with_timeout(self, timeout).await
    }
}

#[cfg(windows)]
impl ConnRecv for crate::ipc::IpcClientConnection {
    async fn recv(&mut self) -> Result<Option<crate::protocol::Response>, crate::ipc::IpcError> {
        crate::ipc::IpcClientConnection::recv_response(self).await
    }
    async fn recv_with_timeout(
        &mut self,
        timeout: std::time::Duration,
    ) -> Result<Option<crate::protocol::Response>, crate::ipc::IpcError> {
        crate::ipc::IpcClientConnection::recv_response_with_timeout(self, timeout).await
    }
}

/// Ephemeral session: single-roundtrip compile (session start + compile +
/// session end in one IPC message). Used when `ZCCACHE_SESSION_ID` is not set.
pub(super) async fn cmd_compile_ephemeral(
    endpoint: &str,
    compiler: &Path,
    args: Vec<String>,
    cwd: NormalizedPath,
    client_env: Vec<(String, String)>,
) -> ExitCode {
    let stdin_bytes = slurp_stdin_if_piped();
    let request = crate::protocol::Request::CompileEphemeral {
        client_pid: std::process::id(),
        working_dir: cwd.clone(),
        compiler: compiler.into(),
        args,
        cwd,
        env: Some(client_env),
        stdin: stdin_bytes,
    };

    // Issue #752: retry once on transport failure
    // (`lost connection to daemon`). Wedge has its own handling.
    // Recovery is a small jittered sleep — ensure_daemon's next call
    // detects + handles a dead daemon (probe -> CommError -> stop +
    // respawn), so we deliberately do NOT pre-emptively kill here:
    // a healthy daemon another worker just spawned must survive.
    let outcome = link_with_retry(
        || run_ephemeral_attempt(endpoint, &request),
        retry_backoff_with_jitter,
        link_retry_budget(),
    )
    .await;

    match outcome {
        CompileRecvOutcome::Done(recv_result) => {
            relay_compile_response(recv_result, &mut std::io::stdout(), &mut std::io::stderr())
        }
        CompileRecvOutcome::Wedged => {
            eprintln!(
                "zccache[err][W]: daemon at {endpoint} stopped responding within \
                 the wedge budget; killing it so the next compile starts fresh — issue #666"
            );
            stop_stale_daemon(endpoint).await;
            ExitCode::FAILURE
        }
        CompileRecvOutcome::Failed(msg) => {
            eprintln!("zccache[err][R]: {msg}");
            ExitCode::FAILURE
        }
    }
}

/// Ephemeral link/archive: single-roundtrip for `zccache ar ...` etc.
pub(super) async fn cmd_link_ephemeral(
    endpoint: &str,
    tool: &Path,
    args: Vec<String>,
    cwd: NormalizedPath,
    client_env: Vec<(String, String)>,
) -> ExitCode {
    let request = crate::protocol::Request::LinkEphemeral {
        client_pid: std::process::id(),
        tool: tool.into(),
        args,
        cwd,
        env: Some(client_env),
    };

    // Issue #752: retry once on transport failure
    // (`lost connection to daemon`). Wedge has its own handling.
    // See `cmd_compile_ephemeral` for the recovery-closure rationale.
    let outcome = link_with_retry(
        || run_ephemeral_attempt(endpoint, &request),
        retry_backoff_with_jitter,
        link_retry_budget(),
    )
    .await;

    match outcome {
        CompileRecvOutcome::Done(recv_result) => {
            relay_link_response(recv_result, &mut std::io::stdout(), &mut std::io::stderr())
        }
        CompileRecvOutcome::Wedged => {
            eprintln!(
                "zccache[err][W]: daemon at {endpoint} stopped responding within \
                 the wedge budget on a Link; killing it so the next request starts \
                 fresh — issue #666"
            );
            stop_stale_daemon(endpoint).await;
            ExitCode::FAILURE
        }
        CompileRecvOutcome::Failed(msg) => {
            eprintln!("zccache[err][R]: {msg}");
            ExitCode::FAILURE
        }
    }
}

/// Jittered backoff fired between retries on transport failure. 50 –
/// 250 ms (random sub-window per call) so N parallel workers that all
/// lost their connection to the same daemon don't fan back in at the
/// exact same moment and pile a fresh spawn-storm on top of the
/// failure that started the retry. Caveat noted on #752.
///
/// Uses `SystemTime::subsec_nanos()` as the jitter source — fine here
/// because we only need decorrelation across same-host concurrent
/// workers, not cryptographic randomness.
async fn retry_backoff_with_jitter() {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let jitter_ms = 50 + u64::from(nanos % 201); // [50, 250]
    tokio::time::sleep(std::time::Duration::from_millis(jitter_ms)).await;
}

/// One full ensure-daemon → connect → send → recv cycle. Any pre-recv
/// failure (daemon spawn error, connect error, send error) is folded
/// into `Failed` so the retry orchestrator can decide whether to
/// recover. The recv outcome (`Done`/`Wedged`/`Failed`) is returned
/// verbatim so the caller can distinguish wedge from transport
/// failure.
async fn run_ephemeral_attempt(
    endpoint: &str,
    request: &crate::protocol::Request,
) -> CompileRecvOutcome {
    if let Err(e) = ensure_daemon(endpoint).await {
        return failed_with_disconnect_event(
            endpoint,
            crate::core::lifecycle::CAUSE_COMM_ERROR,
            format!("cannot start daemon at {endpoint}: {e}"),
        );
    }
    let mut conn = match connect(endpoint).await {
        Ok(c) => c,
        Err(e) => {
            return failed_with_disconnect_event(
                endpoint,
                crate::core::lifecycle::CAUSE_COMM_ERROR,
                format!("cannot connect to daemon at {endpoint}: {e}"),
            );
        }
    };
    let wire = crate::protocol::wire_prost::full_family_wire_format_from_env();
    if let Err(e) = conn.send_request(request, wire).await {
        return failed_with_disconnect_event(
            endpoint,
            crate::core::lifecycle::CAUSE_PIPE_CLOSED_MID_WRITE,
            format!("failed to send to daemon: {e}"),
        );
    }
    let outcome = compile_recv_with_wedge_detection(&mut conn, wedge_recv_timeout()).await;
    if let CompileRecvOutcome::Failed(msg) = &outcome {
        emit_client_disconnected_event(endpoint, crate::core::lifecycle::CAUSE_COMM_ERROR, msg);
    }
    outcome
}

/// Build a `Failed` outcome and emit the matching `client-disconnected`
/// event in one call so the JSONL row is written at the exact moment
/// the dropout was observed. #755 acceptance #3.
fn failed_with_disconnect_event(endpoint: &str, cause: &str, msg: String) -> CompileRecvOutcome {
    emit_client_disconnected_event(endpoint, cause, &msg);
    CompileRecvOutcome::Failed(msg)
}

/// Write a `client-disconnected` JSONL row carrying the client's
/// version, binary path, the endpoint, the cause classification, and
/// the underlying transport message. Pre-#755 these dropouts were
/// only visible one round-trip later as the next
/// `spawn-attempt`'s `reason: replaced-comm-error` — surfacing them
/// at the point of failure lets dashboards correlate against the
/// downstream `daemon-died` / `pipe-handover` events without
/// inferring causality from timestamps.
fn emit_client_disconnected_event(endpoint: &str, cause: &str, detail: &str) {
    let meta = crate::core::lifecycle::client_meta(crate::core::VERSION);
    crate::core::lifecycle::write_event(
        crate::core::lifecycle::EVENT_CLIENT_DISCONNECTED,
        serde_json::json!({
            "endpoint": endpoint,
            "client_pid": std::process::id(),
            "client_version": meta["client_version"],
            "client_binary_path": meta["client_binary_path"],
            "cause": cause,
            "detail": detail,
        }),
    );
}

fn relay_compile_response<W: Write, E: Write>(
    recv_result: Option<crate::protocol::Response>,
    stdout: &mut W,
    stderr: &mut E,
) -> ExitCode {
    match recv_result {
        Some(crate::protocol::Response::CompileResult {
            exit_code,
            stdout: out,
            stderr: err,
            ..
        }) => {
            let _ = stdout.write_all(&out);
            let _ = stderr.write_all(&err);
            exit_code_from_i32(exit_code)
        }
        Some(crate::protocol::Response::Error { message }) => {
            let _ = writeln!(stderr, "zccache[err][E]: daemon error: {message}");
            ExitCode::FAILURE
        }
        None => {
            let _ = writeln!(stderr, "{LOST_CONNECTION_MSG}");
            ExitCode::FAILURE
        }
        Some(other) => {
            let _ = writeln!(
                stderr,
                "zccache[err][U]: unexpected response from daemon: {other:?}"
            );
            ExitCode::FAILURE
        }
    }
}

fn relay_link_response<W: Write, E: Write>(
    recv_result: Option<crate::protocol::Response>,
    stdout: &mut W,
    stderr: &mut E,
) -> ExitCode {
    match recv_result {
        Some(crate::protocol::Response::LinkResult {
            exit_code,
            stdout: out,
            stderr: err,
            warning,
            ..
        }) => {
            let _ = stdout.write_all(&out);
            let _ = stderr.write_all(&err);
            if let Some(w) = warning {
                let _ = writeln!(stderr, "zccache warning: {w}");
            }
            exit_code_from_i32(exit_code)
        }
        Some(crate::protocol::Response::Error { message }) => {
            let _ = writeln!(stderr, "zccache[err][E]: daemon error: {message}");
            ExitCode::FAILURE
        }
        None => {
            let _ = writeln!(stderr, "{LOST_CONNECTION_MSG}");
            ExitCode::FAILURE
        }
        Some(other) => {
            let _ = writeln!(
                stderr,
                "zccache[err][U]: unexpected response from daemon: {other:?}"
            );
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

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

        assert_eq!(exit, ExitCode::from(7));
        assert_eq!(stdout, b"compiler-out");
        assert_eq!(stderr, b"compiler-err");
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
    }

    impl ConnRecv for FakeConn {
        async fn recv(
            &mut self,
        ) -> Result<Option<crate::protocol::Response>, crate::ipc::IpcError> {
            match &self.behavior {
                FakeBehavior::Ok(r) => Ok(Some(r.clone())),
                FakeBehavior::TimesOut => {
                    // Sleep forever; the outer timeout wrapper handles it.
                    futures::future::pending::<()>().await;
                    unreachable!()
                }
                FakeBehavior::BrokenPipe => Err(crate::ipc::IpcError::ConnectionClosed),
            }
        }
        async fn recv_with_timeout(
            &mut self,
            timeout: std::time::Duration,
        ) -> Result<Option<crate::protocol::Response>, crate::ipc::IpcError> {
            match &self.behavior {
                FakeBehavior::Ok(r) => Ok(Some(r.clone())),
                FakeBehavior::TimesOut => {
                    tokio::time::sleep(timeout).await;
                    Err(crate::ipc::IpcError::Timeout(timeout))
                }
                FakeBehavior::BrokenPipe => Err(crate::ipc::IpcError::ConnectionClosed),
            }
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
        let outcome = compile_recv_with_wedge_detection(&mut conn, TEST_BUDGET).await;
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
        let outcome = compile_recv_with_wedge_detection(&mut conn, TEST_BUDGET).await;
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

    #[tokio::test]
    async fn wedge_detection_does_not_misclassify_broken_pipe_as_wedge() {
        // A non-timeout transport error must NOT trigger the recovery path
        // (force-killing the daemon on every protocol mismatch would be a
        // worse cure than the disease).
        let mut conn = FakeConn {
            behavior: FakeBehavior::BrokenPipe,
        };
        let outcome = compile_recv_with_wedge_detection(&mut conn, TEST_BUDGET).await;
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
    async fn link_retry_recovers_after_one_transport_failure() {
        let attempts = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
        let recoveries = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
        let outcome = link_with_retry(
            || {
                let n = attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
                async move {
                    if n == 1 {
                        CompileRecvOutcome::Failed("lost connection to daemon".to_string())
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
            "retry should drive a transport-flaky link to a Done outcome (#752)"
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
                async { CompileRecvOutcome::Failed("daemon really gone".to_string()) }
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
                async { CompileRecvOutcome::Failed("once".to_string()) }
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

    #[tokio::test]
    async fn wedge_detection_disabled_when_budget_is_none() {
        // `budget = None` opts back into the pre-#666 unbounded recv
        // (used in production when `ZCCACHE_WEDGE_RECV_TIMEOUT_SECS=0`).
        // The fake's BrokenPipe path returns immediately so the test
        // doesn't hang.
        let mut conn = FakeConn {
            behavior: FakeBehavior::BrokenPipe,
        };
        let outcome = compile_recv_with_wedge_detection(&mut conn, None).await;
        // Disabled → falls through to `conn.recv()` unbounded, which the
        // fake reports as a broken pipe. Crucially: not classified as Wedged.
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

        assert_eq!(exit, ExitCode::SUCCESS);
        assert_eq!(stdout, b"link-out");
        assert_eq!(
            String::from_utf8(stderr).unwrap(),
            "link-err\nzccache warning: non-deterministic archive flags\n"
        );
    }
}
