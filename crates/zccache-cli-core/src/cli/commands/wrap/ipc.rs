//! Wrapper IPC request construction and response relay.

use crate::core::NormalizedPath;
use std::io::{IsTerminal, Write};
use std::path::Path;
use std::process::ExitCode;

use super::super::super::{link_retry_budget, wedge_recv_timeout};
use super::super::util::{connect, exit_code_from_i32, slurp_stdin_if_piped, LOST_CONNECTION_MSG};
use crate::cli::runtime::{current_daemon_instance, ensure_daemon, stop_wedged_daemon};

pub(super) async fn cmd_compile(
    endpoint: &str,
    session_id: &str,
    args: Vec<String>,
    cwd: NormalizedPath,
    compiler: NormalizedPath,
    client_env: Vec<(String, String)>,
) -> ExitCode {
    let stdin_bytes = slurp_stdin_if_piped();
    // #1161: name the instance we are about to talk to BEFORE the exchange.
    // If this request wedges, the lock file may already name a replacement
    // some other client spawned, and killing "whoever is current" is how one
    // client's timeout becomes a kill chain through a `-j16` herd.
    let served_by = current_daemon_instance();
    let mut conn = match connect(endpoint).await {
        Ok(c) => c,
        Err(e) => {
            let reason = format!("cannot connect to daemon at {endpoint}: {e}");
            emit_client_disconnected_event(
                endpoint,
                crate::core::lifecycle::CAUSE_COMM_ERROR,
                &reason,
            );
            super::emit_wrapper_warning(&format!(
                "zccache[warn][S]: {reason}; retrying via ephemeral session"
            ));
            return cmd_compile_ephemeral_with_stdin(
                endpoint,
                compiler.as_path(),
                args,
                cwd,
                client_env,
                stdin_bytes,
            )
            .await;
        }
    };

    let selection = crate::protocol::wire_prost::full_family_wire_selection_from_env();
    let wire = selection.preferred_format();
    let request = crate::protocol::Request::Compile {
        session_id: session_id.to_string(),
        args: args.clone(),
        cwd: cwd.clone(),
        compiler: compiler.clone(),
        env: Some(client_env.clone()),
        stdin: stdin_bytes.clone(),
    };
    if let Err(e) = conn.send_request(&request, wire).await {
        let failure = TransportFailure {
            message: format!("failed to send to daemon: {e}"),
            phase: FailurePhase::DeliveryUnknown,
            explicit_wire_mismatch: false,
        };
        emit_client_disconnected_event(
            endpoint,
            crate::core::lifecycle::CAUSE_PIPE_CLOSED_MID_WRITE,
            &failure.message,
        );
        eprintln!("zccache[err][S]: {}", failure.message);
        return ExitCode::FAILURE;
    }

    let outcome = compile_recv_with_wedge_detection(&mut conn, wedge_recv_timeout(), wire).await;
    let outcome =
        retry_bincode_on_explicit_wire_mismatch(endpoint, &request, selection, outcome).await;
    match outcome {
        CompileRecvOutcome::Done(recv_result) => {
            report_relay_outcome(relay_compile_response_to_stdio(recv_result))
        }
        CompileRecvOutcome::Wedged => {
            // Daemon went past the wedge budget for *this* request. Pre-#753
            // we always killed it; #726 / FastLED/#3011 showed that under
            // burst-link load the "wedge" is almost always the daemon
            // being too busy with other workers' legitimate requests to
            // service ours in time, and unconditional kill collapses the
            // whole shared cohort.
            //
            // New behaviour (#753): probe the daemon with `Ping` on a
            // fresh connection within `wedge_probe_budget()`. If it
            // answers, preserve it but fail this invocation: the original
            // request may still be queued or executing, so replaying it would
            // duplicate work (#1417). If the probe itself fails or times out,
            // run the pre-#753 kill+respawn recovery.
            drop(conn);
            match wedge_action(endpoint).await {
                WedgeAction::DowngradeNoKill => {
                    eprintln!(
                        "zccache[err][W]: daemon at {endpoint} answered a probe but the \
                         original compile has ambiguous delivery; failing without replaying \
                         or killing the responsive daemon — issues #753/#1417"
                    );
                    ExitCode::FAILURE
                }
                WedgeAction::EscalateKill | WedgeAction::EscalateKillProbeError => {
                    // The daemon is genuinely wedged: it missed the
                    // per-request wedge budget AND failed the follow-up
                    // responsiveness probe. Per the fail-fast policy (#955)
                    // a *detected* wedge fails IMMEDIATELY — we do not mask
                    // it with a slow uncached retry/fallback. Kill the
                    // wedged daemon so the next invocation starts fresh,
                    // then surface the failure now. (The root-cause daemon
                    // fix keeps this path from triggering in normal builds.)
                    eprintln!(
                        "zccache[err][W]: daemon at {endpoint} is wedged \
                         (missed wedge budget + failed probe); killing it and \
                         failing immediately — issue #955"
                    );
                    stop_wedged_daemon(endpoint, served_by.as_ref()).await;
                    ExitCode::FAILURE
                }
            }
        }
        CompileRecvOutcome::Failed(msg) => {
            // #755 acceptance #3: log the dropout at the point of
            // failure so dashboards correlate against the spawn-attempt
            // that follows.
            emit_client_disconnected_event(
                endpoint,
                crate::core::lifecycle::CAUSE_COMM_ERROR,
                &msg.message,
            );
            eprintln!("zccache[err][R]: {}", msg.message);
            ExitCode::FAILURE
        }
    }
}

/// Decide what a wedge means before acting on it.
///
/// #753 established that under burst-link load a "wedge" is usually a healthy
/// daemon too busy to answer in time, and that killing it collapses the whole
/// shared cohort. #1170 change 2 makes that classification apply to **every**
/// wedge arm — the session arm had it, the two ephemeral arms killed
/// unconditionally.
///
/// `ZCCACHE_WEDGE_PROBE_BUDGET_MS=0` disables the probe and preserves the
/// pre-#753 unconditional-replace behaviour for anyone A/B-testing it.
async fn wedge_action(endpoint: &str) -> WedgeAction {
    match wedge_probe_budget() {
        Some(budget) => classify_probe_outcome(probe_daemon_responsive(endpoint, budget).await),
        None => WedgeAction::EscalateKill,
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
    Failed(TransportFailure),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FailurePhase {
    /// No request bytes were handed to a daemon.
    PreDispatch,
    /// The request may have reached the daemon; direct execution is unsafe.
    DeliveryUnknown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TransportFailure {
    message: String,
    phase: FailurePhase,
    /// Framing was rejected before daemon dispatch, so an auto-selected prost
    /// request may be replayed once over bincode without duplicating work.
    explicit_wire_mismatch: bool,
}

#[derive(Debug, PartialEq)]
enum RelayOutcome {
    Verdict(ExitCode),
    /// The daemon completed the request without returning a compiler/tool
    /// verdict. This is intentionally not eligible for local fallback: the
    /// daemon may have already executed the tool or changed output state.
    NoVerdict(String),
}

fn report_relay_outcome(outcome: RelayOutcome) -> ExitCode {
    match outcome {
        RelayOutcome::Verdict(code) => code,
        RelayOutcome::NoVerdict(message) => {
            eprintln!("{message}");
            ExitCode::FAILURE
        }
    }
}

/// Wrap a compile-response recv with an optional wedge budget.
///
/// `budget = Some(d)` enables wedge detection; `budget = None` falls
/// back to the IPC layer's 300 s default recv timeout but disables wedge
/// classification/daemon respawn. Production callers pass [`wedge_recv_timeout`]
/// so the env knob still works; tests pass an explicit value so they don't race
/// the process-global env var (#745).
///
/// Returns [`CompileRecvOutcome::Wedged`] only for the specific
/// `IpcError::Timeout` signal — everything else (graceful close, broken
/// pipe, protocol error) maps to [`CompileRecvOutcome::Failed`] so the
/// caller does not respawn the daemon on errors that have nothing to do
/// with a wedge.
///
/// # Progress-based wedge detection (issue #1216)
///
/// This is a **loop**, not a single recv: the daemon pushes non-terminal
/// [`crate::protocol::Response::CompileProgress`] heartbeats on this same
/// connection while the compile waits for a compile-concurrency permit. Each
/// heartbeat is reported to the user and restarts the budget, so the budget
/// means "the daemon has gone quiet for this long" rather than "this compile
/// took this long". A compile legitimately queued for longer than the budget
/// therefore completes on its original connection with its cached-path
/// result — no probe, no ephemeral re-run, no kill — while a daemon that
/// emits *nothing* for a full budget still trips the #753/#955 handling.
async fn compile_recv_with_wedge_detection<C: ConnRecv>(
    conn: &mut C,
    budget: Option<std::time::Duration>,
    wire: crate::protocol::wire_prost::WireFormat,
) -> CompileRecvOutcome {
    let recv_timeout = budget.unwrap_or(crate::ipc::DEFAULT_CLIENT_RECV_TIMEOUT);
    loop {
        match conn.recv_with_timeout(recv_timeout, wire).await {
            Ok(Some(crate::protocol::Response::CompileProgress {
                queue_position,
                queue_depth,
                in_flight,
                phase,
            })) => {
                report_compile_progress(queue_position, queue_depth, in_flight, &phase);
                // Budget restarts on the next loop iteration — this is the
                // progress reset that makes wedge detection progress-based.
            }
            Ok(opt) => return CompileRecvOutcome::Done(opt),
            // Only a configured budget turns a quiet daemon into a wedge; with
            // `budget == None` wedge classification is disabled, so the IPC
            // default timeout is reported as a plain transport failure.
            Err(crate::ipc::IpcError::Timeout(_)) if budget.is_some() => {
                return CompileRecvOutcome::Wedged
            }
            Err(e) => {
                let explicit_wire_mismatch = crate::ipc::full_family_wire_mismatch_error(&e);
                return CompileRecvOutcome::Failed(TransportFailure {
                    message: format!("broken connection to daemon: {e}"),
                    phase: FailurePhase::DeliveryUnknown,
                    explicit_wire_mismatch,
                });
            }
        }
    }
}

fn outcome_requires_bincode_retry(
    selection: crate::protocol::wire_prost::ClientWireSelection,
    outcome: &CompileRecvOutcome,
) -> bool {
    if !selection.allows_bincode_fallback() {
        return false;
    }
    matches!(
        outcome,
        CompileRecvOutcome::Failed(TransportFailure {
            explicit_wire_mismatch: true,
            ..
        })
    )
}

/// Retry an auto-selected prost streaming request exactly once over bincode
/// only when the first peer explicitly rejected framing before dispatch.
/// Ambiguous close/write failures never enter this path because replaying a
/// compile or link could execute the tool twice.
async fn retry_bincode_on_explicit_wire_mismatch(
    endpoint: &str,
    request: &crate::protocol::Request,
    selection: crate::protocol::wire_prost::ClientWireSelection,
    outcome: CompileRecvOutcome,
) -> CompileRecvOutcome {
    if !outcome_requires_bincode_retry(selection, &outcome) {
        return outcome;
    }

    let mut conn = match connect(endpoint).await {
        Ok(conn) => conn,
        Err(err) => {
            return failed_with_disconnect_event(
                endpoint,
                crate::core::lifecycle::CAUSE_COMM_ERROR,
                FailurePhase::PreDispatch,
                format!("cannot reconnect for bincode compatibility retry: {err}"),
            );
        }
    };
    if let Err(err) = conn
        .send_request(request, crate::protocol::wire_prost::WireFormat::BincodeV15)
        .await
    {
        return failed_with_disconnect_event(
            endpoint,
            crate::core::lifecycle::CAUSE_PIPE_CLOSED_MID_WRITE,
            FailurePhase::DeliveryUnknown,
            format!("failed to send bincode compatibility retry: {err}"),
        );
    }
    compile_recv_with_wedge_detection(
        &mut conn,
        wedge_recv_timeout(),
        crate::protocol::wire_prost::WireFormat::BincodeV15,
    )
    .await
}

/// Human-readable one-liner for a `CompileProgress` heartbeat.
///
/// Kept separate from the printing so it can be asserted in unit tests
/// without capturing stderr.
fn compile_progress_line(
    queue_position: u32,
    queue_depth: u32,
    in_flight: u32,
    phase: &str,
) -> String {
    if queue_depth == 0 && queue_position == 0 {
        format!("zccache[info][Q]: daemon under load: {phase}, {in_flight} compiles in flight")
    } else {
        format!(
            "zccache[info][Q]: daemon under load: {phase}, position {queue_position} of \
             {queue_depth} queued, {in_flight} in flight"
        )
    }
}

fn report_compile_progress(queue_position: u32, queue_depth: u32, in_flight: u32, phase: &str) {
    let line = compile_progress_line(queue_position, queue_depth, in_flight, phase);
    // Diagnostic only: never let a failed status write affect the compile.
    let _ = writeln!(std::io::stderr(), "{line}");
}

/// Tiny seam over the platform-specific IPC connection types so the
/// wedge-detection helper can be unit-tested without spinning up a real
/// pipe/socket. Two impls live below — one for Unix `IpcConnection`, one
/// for the Windows client-side `IpcClientConnection`.
trait ConnRecv {
    async fn recv_with_timeout(
        &mut self,
        timeout: std::time::Duration,
        wire: crate::protocol::wire_prost::WireFormat,
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
/// Only a [`FailurePhase::PreDispatch`] failure triggers retry. Once request
/// bytes may have reached the daemon, EOF/I/O is ambiguous and replaying a
/// compile or link can execute it twice (#1417). Wedge has its own no-replay
/// classification path.
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
    while matches!(
        outcome,
        CompileRecvOutcome::Failed(TransportFailure {
            phase: FailurePhase::PreDispatch,
            ..
        })
    ) && recoveries_used < max_recoveries
    {
        recover().await;
        recoveries_used += 1;
        outcome = attempt().await;
    }
    outcome
}

/// Issue #753: outcome of a "is the daemon responsive?" probe sent
/// just before the wedge guard would `Shutdown` it. The point of the
/// probe is to distinguish a daemon that is *genuinely wedged* (no
/// progress, kill it) from one that is *busy processing legitimate
/// in-flight work* under burst-link load (don't kill it — recover via
/// the existing ephemeral fall-through instead).
///
/// Returned by [`classify_probe_outcome`] from a pure-function input
/// so the decision matrix is unit-testable without a real daemon.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WedgeAction {
    /// Probe came back inside its budget — the daemon is alive and
    /// answering on its IPC endpoint. The wedge on the original
    /// request must have been triggered by the daemon being too busy
    /// to respond within the wedge budget, not by it being hung. The
    /// caller should NOT send `Shutdown`. The current request still fails
    /// without replay because it may remain queued or executing (#1417).
    DowngradeNoKill,
    /// Probe itself timed out inside its (short) budget. The daemon
    /// is genuinely wedged — no accept, no dispatch, no response.
    /// Caller should run the existing kill+respawn recovery.
    EscalateKill,
    /// Probe failed with a transport-level error before the budget
    /// expired (broken pipe, version mismatch, connect refused, …).
    /// Caller should run the existing kill+respawn recovery: a daemon
    /// that can't even accept a fresh connection is in worse shape
    /// than a wedged one.
    EscalateKillProbeError,
}

/// Pure-function classifier: maps the result of a `Ping`-budget probe
/// to a wedge action. Production callers wire `attempt_daemon_ping`
/// (below) as the probe; tests pass stub outcomes directly. Issue
/// [#753].
pub(crate) fn classify_probe_outcome(
    probe: Result<Result<(), crate::ipc::IpcError>, tokio::time::error::Elapsed>,
) -> WedgeAction {
    match probe {
        Ok(Ok(())) => WedgeAction::DowngradeNoKill,
        Ok(Err(_)) => WedgeAction::EscalateKillProbeError,
        Err(_) => WedgeAction::EscalateKill,
    }
}

/// Send a `Ping` to the daemon on a fresh connection with the given
/// budget. Returns the nested `Result` shape that
/// [`classify_probe_outcome`] consumes:
///
///   * `Ok(Ok(()))` — Pong returned within the budget.
///   * `Ok(Err(IpcError))` — transport-level error before the budget
///     expired (broken pipe, connect refused, version mismatch).
///   * `Err(Elapsed)` — budget expired with no response, daemon is
///     genuinely wedged.
///
/// Production caller for [`classify_probe_outcome`] in the Wedged
/// arm. Issue #753.
pub(crate) async fn probe_daemon_responsive(
    endpoint: &str,
    budget: std::time::Duration,
) -> Result<Result<(), crate::ipc::IpcError>, tokio::time::error::Elapsed> {
    tokio::time::timeout(budget, async {
        crate::ipc::daemon_control_roundtrip(
            endpoint,
            crate::ipc::DaemonControlRequest::Ping,
            Some(budget),
        )
        .await?;
        // We don't need to parse Pong out — receiving any response
        // within budget is enough to know the daemon is alive and
        // serving. Drop the connection on the way out.
        Ok::<(), crate::ipc::IpcError>(())
    })
    .await
}

/// Default short budget for the probe sent before sending `Shutdown`
/// in the Wedged arm. Issue #753.
///
/// 3 s is long enough that a daemon serving N other workers' link
/// requests still has a fresh tokio task slot to handle a Ping
/// (each connection is its own task in the multi-thread runtime), but
/// short enough that adding a probe doesn't materially extend the
/// total wedge-detection latency from the user's perspective. Override
/// with `ZCCACHE_WEDGE_PROBE_BUDGET_MS`. Set to `0` to disable the
/// probe entirely (pre-#753 unconditional kill behavior — useful for
/// diagnostic A/B against the fix).
pub(crate) const WEDGE_PROBE_DEFAULT_MS: u64 = 3_000;

/// Returns the probe budget configured for this run. `None` means
/// "probe disabled — kill unconditionally" (the pre-#753 behavior).
pub(crate) fn wedge_probe_budget() -> Option<std::time::Duration> {
    let ms = std::env::var("ZCCACHE_WEDGE_PROBE_BUDGET_MS")
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(WEDGE_PROBE_DEFAULT_MS);
    if ms == 0 {
        None
    } else {
        Some(std::time::Duration::from_millis(ms))
    }
}

impl ConnRecv for crate::ipc::IpcConnection {
    async fn recv_with_timeout(
        &mut self,
        timeout: std::time::Duration,
        wire: crate::protocol::wire_prost::WireFormat,
    ) -> Result<Option<crate::protocol::Response>, crate::ipc::IpcError> {
        crate::ipc::IpcConnection::recv_response_for_wire_with_timeout(self, timeout, wire).await
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
    cmd_compile_ephemeral_with_stdin(endpoint, compiler, args, cwd, client_env, stdin_bytes).await
}

async fn cmd_compile_ephemeral_with_stdin(
    endpoint: &str,
    compiler: &Path,
    args: Vec<String>,
    cwd: NormalizedPath,
    client_env: Vec<(String, String)>,
    stdin_bytes: Vec<u8>,
) -> ExitCode {
    let request = crate::protocol::Request::CompileEphemeral {
        client_pid: std::process::id(),
        working_dir: cwd.clone(),
        compiler: compiler.into(),
        args: args.clone(),
        cwd: cwd.clone(),
        env: Some(client_env.clone()),
        stdin: stdin_bytes.clone(),
    };

    // #1161: identity of the instance this attempt targets, read before the
    // exchange so a later wedge kill cannot land on a replacement.
    let served_by = current_daemon_instance();

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
            report_relay_outcome(relay_compile_response_to_stdio(recv_result))
        }
        // #1170 change 2: this arm used to kill unconditionally. A busy
        // daemon under a `-j16` burst looks identical to a hung one from a
        // single client's timeout, and killing it takes down the daemon every
        // other worker is waiting on. Classify first, exactly as the session
        // arm has since #753.
        CompileRecvOutcome::Wedged => match wedge_action(endpoint).await {
            WedgeAction::DowngradeNoKill => {
                eprintln!(
                    "zccache[err][W]: daemon at {endpoint} answered a probe but the original \
                     compile has ambiguous delivery; failing without replaying or killing the \
                     responsive daemon — issues #753/#1170/#1417"
                );
                ExitCode::FAILURE
            }
            WedgeAction::EscalateKill | WedgeAction::EscalateKillProbeError => {
                eprintln!(
                    "zccache[err][W]: daemon at {endpoint} stopped responding within \
                     the wedge budget and failed the follow-up probe; killing it so the \
                     next compile starts fresh — issue #666"
                );
                stop_wedged_daemon(endpoint, served_by.as_ref()).await;
                ExitCode::FAILURE
            }
        },
        CompileRecvOutcome::Failed(msg) => match msg.phase {
            // #1170: this used to run the compiler directly, uncached, and
            // mirror its exit code — so a daemon outage that compiled fine
            // exited 0 and stayed invisible.
            FailurePhase::PreDispatch => {
                super::unavailable::refuse_uncached_run(endpoint, compiler, &cwd, &msg.message)
            }
            FailurePhase::DeliveryUnknown => {
                eprintln!("zccache[err][R]: {}", msg.message);
                ExitCode::FAILURE
            }
        },
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
    // #1170 removed the local-fallback arm, which was the only consumer of
    // the pre-move clones of `args`/`client_env` — nothing runs the tool
    // locally any more, so nothing needs a second copy of its argv.
    let request = crate::protocol::Request::LinkEphemeral {
        client_pid: std::process::id(),
        tool: tool.into(),
        args,
        cwd: cwd.clone(),
        env: Some(client_env),
    };

    // #1161: see `cmd_compile_ephemeral` — identity before the exchange.
    let served_by = current_daemon_instance();

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
            report_relay_outcome(relay_link_response_to_stdio(recv_result))
        }
        // #1170 change 2: classify before killing, as in `cmd_compile_ephemeral`.
        // Links are the requests most likely to be slow for legitimate reasons,
        // so this arm is if anything the more important of the two.
        CompileRecvOutcome::Wedged => match wedge_action(endpoint).await {
            WedgeAction::DowngradeNoKill => {
                eprintln!(
                    "zccache[err][W]: daemon at {endpoint} answered a probe but the original \
                     link has ambiguous delivery; failing without replaying or killing the \
                     responsive daemon — issues #753/#1170/#1417"
                );
                ExitCode::FAILURE
            }
            WedgeAction::EscalateKill | WedgeAction::EscalateKillProbeError => {
                eprintln!(
                    "zccache[err][W]: daemon at {endpoint} stopped responding within \
                     the wedge budget on a Link and failed the follow-up probe; killing it \
                     so the next request starts fresh — issue #666"
                );
                stop_wedged_daemon(endpoint, served_by.as_ref()).await;
                ExitCode::FAILURE
            }
        },
        CompileRecvOutcome::Failed(msg) => match msg.phase {
            // #1170: as in `cmd_compile_ephemeral` — a link/archive step is
            // no more entitled to silently bypass the daemon than a compile.
            FailurePhase::PreDispatch => {
                super::unavailable::refuse_uncached_run(endpoint, tool, &cwd, &msg.message)
            }
            FailurePhase::DeliveryUnknown => {
                eprintln!("zccache[err][R]: {}", msg.message);
                ExitCode::FAILURE
            }
        },
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
            FailurePhase::PreDispatch,
            format!("cannot start daemon at {endpoint}: {e}"),
        );
    }
    let mut conn = match connect(endpoint).await {
        Ok(c) => c,
        Err(e) => {
            return failed_with_disconnect_event(
                endpoint,
                crate::core::lifecycle::CAUSE_COMM_ERROR,
                FailurePhase::PreDispatch,
                format!("cannot connect to daemon at {endpoint}: {e}"),
            );
        }
    };
    let selection = crate::protocol::wire_prost::full_family_wire_selection_from_env();
    let wire = selection.preferred_format();
    if let Err(e) = conn.send_request(request, wire).await {
        return failed_with_disconnect_event(
            endpoint,
            crate::core::lifecycle::CAUSE_PIPE_CLOSED_MID_WRITE,
            FailurePhase::DeliveryUnknown,
            format!("failed to send to daemon: {e}"),
        );
    }
    let outcome = compile_recv_with_wedge_detection(&mut conn, wedge_recv_timeout(), wire).await;
    let outcome =
        retry_bincode_on_explicit_wire_mismatch(endpoint, request, selection, outcome).await;
    if let CompileRecvOutcome::Failed(msg) = &outcome {
        emit_client_disconnected_event(
            endpoint,
            crate::core::lifecycle::CAUSE_COMM_ERROR,
            &msg.message,
        );
    }
    outcome
}

/// Build a `Failed` outcome and emit the matching `client-disconnected`
/// event in one call so the JSONL row is written at the exact moment
/// the dropout was observed. #755 acceptance #3.
fn failed_with_disconnect_event(
    endpoint: &str,
    cause: &str,
    phase: FailurePhase,
    msg: String,
) -> CompileRecvOutcome {
    emit_client_disconnected_event(endpoint, cause, &msg);
    CompileRecvOutcome::Failed(TransportFailure {
        message: msg,
        phase,
        explicit_wire_mismatch: false,
    })
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

#[cfg(test)]
fn relay_compile_response<W: Write, E: Write>(
    recv_result: Option<crate::protocol::Response>,
    stdout: &mut W,
    stderr: &mut E,
) -> RelayOutcome {
    relay_compile_response_with_color(recv_result, stdout, stderr, false)
}

fn relay_compile_response_to_stdio(recv_result: Option<crate::protocol::Response>) -> RelayOutcome {
    let mut stdout = std::io::stdout();
    let mut stderr = std::io::stderr();
    let color = stderr.is_terminal() && std::env::var_os("NO_COLOR").is_none();
    relay_compile_response_with_color(recv_result, &mut stdout, &mut stderr, color)
}

fn relay_compile_response_with_color<W: Write, E: Write>(
    recv_result: Option<crate::protocol::Response>,
    stdout: &mut W,
    stderr: &mut E,
    color_unknown_warning: bool,
) -> RelayOutcome {
    match recv_result {
        Some(crate::protocol::Response::CompileResult {
            exit_code,
            stdout: out,
            stderr: err,
            ..
        }) => {
            let _ = stdout.write_all(&out);
            let _ = write_relay_stderr(stderr, &err, color_unknown_warning);
            RelayOutcome::Verdict(exit_code_from_i32(exit_code))
        }
        Some(crate::protocol::Response::Error { message }) => {
            RelayOutcome::NoVerdict(format!("zccache[err][E]: daemon error: {message}"))
        }
        None => RelayOutcome::NoVerdict(LOST_CONNECTION_MSG.to_string()),
        Some(other) => RelayOutcome::NoVerdict(format!(
            "zccache[err][U]: unexpected response from daemon: {other:?}"
        )),
    }
}

#[cfg(test)]
fn relay_link_response<W: Write, E: Write>(
    recv_result: Option<crate::protocol::Response>,
    stdout: &mut W,
    stderr: &mut E,
) -> RelayOutcome {
    relay_link_response_with_color(recv_result, stdout, stderr, false)
}

fn relay_link_response_to_stdio(recv_result: Option<crate::protocol::Response>) -> RelayOutcome {
    let mut stdout = std::io::stdout();
    let mut stderr = std::io::stderr();
    let color = stderr.is_terminal() && std::env::var_os("NO_COLOR").is_none();
    relay_link_response_with_color(recv_result, &mut stdout, &mut stderr, color)
}

fn relay_link_response_with_color<W: Write, E: Write>(
    recv_result: Option<crate::protocol::Response>,
    stdout: &mut W,
    stderr: &mut E,
    color_unknown_warning: bool,
) -> RelayOutcome {
    match recv_result {
        Some(crate::protocol::Response::LinkResult {
            exit_code,
            stdout: out,
            stderr: err,
            warning,
            ..
        }) => {
            let _ = stdout.write_all(&out);
            let _ = write_relay_stderr(stderr, &err, color_unknown_warning);
            if let Some(w) = warning {
                let _ = writeln!(stderr, "zccache warning: {w}");
            }
            RelayOutcome::Verdict(exit_code_from_i32(exit_code))
        }
        Some(crate::protocol::Response::Error { message }) => {
            RelayOutcome::NoVerdict(format!("zccache[err][E]: daemon error: {message}"))
        }
        None => RelayOutcome::NoVerdict(LOST_CONNECTION_MSG.to_string()),
        Some(other) => RelayOutcome::NoVerdict(format!(
            "zccache[err][U]: unexpected response from daemon: {other:?}"
        )),
    }
}

fn write_relay_stderr(
    writer: &mut dyn Write,
    bytes: &[u8],
    color_unknown_warning: bool,
) -> std::io::Result<()> {
    let marker = crate::protocol::UNKNOWN_MISS_WARNING_PREFIX.as_bytes();
    if !color_unknown_warning {
        return writer.write_all(bytes);
    }

    let mut remaining = bytes;
    while let Some(start) = remaining
        .windows(marker.len())
        .position(|window| window == marker)
    {
        writer.write_all(&remaining[..start])?;
        let warning = &remaining[start..];
        let end = warning
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(warning.len(), |index| index + 1);
        super::write_wrapper_warning_line(writer, &warning[..end], true)?;
        remaining = &warning[end..];
    }
    writer.write_all(remaining)
}

#[cfg(test)]
#[path = "ipc/tests.rs"]
mod tests;
