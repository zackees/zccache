//! Progress-based watchdog around daemon-owned child-process waits.
//!
//! The daemon spawns compiler/linker/tool children and, on a cache miss, awaits
//! their output before replying. The naive `child.wait_with_output().await`
//! drains stdout **and** stderr to EOF and only then returns. That is a wedge
//! hazard (issue #962, meta #968): a killed `rustc` can leave an **orphaned
//! grandchild** (a linker, a codegen backend, a jobserver, a build-script
//! daemon) that inherited the child's stdout/stderr **write handle**. The pipe
//! then never reaches EOF even though the direct child has exited, so
//! `wait_with_output` never returns — the daemon parks forever holding a
//! compile-concurrency permit, and eventually every later compile starves on
//! the shared semaphore. `kill_on_drop(true)` does not save it: the future is
//! never dropped, and even on drop it kills only the direct child, not the
//! orphan.
//!
//! [`wait_with_output_watchdog`] replaces the naive wait with a concurrent
//! drain that separates "child exited" from "pipes reached EOF". Once the child
//! has exited, the remaining drain is bounded by a short grace window — a value
//! that is safe for arbitrarily long compiles/links because the timer starts
//! only **after** the child process exits (the OS pipe buffer that can still be
//! in flight at that point is at most tens of KiB, which drains in microseconds;
//! anything longer means an orphan is holding the write handle). When the grace
//! elapses the watchdog abandons the drain, returns the output captured so far
//! with the real exit status, and — per the daemon's forensics rule — complains
//! loudly (`tracing::warn!`) and writes a durable lifecycle event so the wedge
//! is investigable after the fact.
//!
//! This is deliberately **not** a wall-clock timeout on the compile itself: a
//! large link legitimately runs for minutes with the child alive the whole
//! time, and this watchdog never touches that case. Detecting an
//! alive-but-genuinely-hung child (no exit, no progress) is a complementary
//! CPU/output-progress watchdog tracked separately under #889/#891.

use std::process::{ExitStatus, Output};
use std::time::{Duration, Instant};

use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Child;
use tokio::sync::mpsc;

use super::compile_output::RawOutputChunk;

/// Default post-exit drain grace. Once the child has exited, the daemon waits
/// at most this long for stdout/stderr to reach EOF before concluding an orphan
/// holds the pipe. Two seconds is enormous relative to draining a drained-at-
/// exit OS pipe buffer, so this never truncates a legitimately-exited child's
/// output; it only bounds the orphan-pipe wedge.
const DEFAULT_POST_EXIT_GRACE: Duration = Duration::from_secs(2);

/// Env override for [`DEFAULT_POST_EXIT_GRACE`], in milliseconds. Exposed for
/// slow hosts / debugging; `0` disables the watchdog (restores the historical
/// unbounded `wait_with_output` behavior).
const POST_EXIT_GRACE_ENV: &str = "ZCCACHE_POST_EXIT_DRAIN_MS";

/// Resolve the post-exit drain grace from the environment, falling back to
/// [`DEFAULT_POST_EXIT_GRACE`]. `Some(Duration::ZERO)` means "disabled".
fn post_exit_grace() -> Duration {
    match std::env::var(POST_EXIT_GRACE_ENV) {
        Ok(v) => match v.trim().parse::<u64>() {
            Ok(ms) => Duration::from_millis(ms),
            Err(_) => DEFAULT_POST_EXIT_GRACE,
        },
        Err(_) => DEFAULT_POST_EXIT_GRACE,
    }
}

/// Default alive-hung stall window (Mode B, issue #891). While the child is
/// still running, the watchdog kills it only after this long with BOTH no
/// stdout/stderr output AND no CPU progress. Deliberately generous: a silent
/// but CPU-bound compile (rustc mid-codegen prints nothing) keeps advancing CPU
/// and is never touched; only a process that is genuinely stuck — no output and
/// no CPU for five minutes — is reaped. This is the "progress-based, not a dumb
/// wall-clock timeout" contract: a legitimately long link runs for minutes
/// while burning CPU / emitting output and is left alone.
const DEFAULT_STALL_WINDOW: Duration = Duration::from_secs(300);

/// How often the alive-hung watchdog samples progress (output bytes + child CPU
/// time) while the child is running. Cheap: one `GetProcessTimes` /
/// `/proc/<pid>/stat` read per tick.
const STALL_TICK: Duration = Duration::from_secs(5);

/// Env override for [`DEFAULT_STALL_WINDOW`], in milliseconds. `0` disables the
/// alive-hung (Mode B) watchdog, leaving only the post-exit orphan-pipe (Mode A)
/// watchdog active.
const STALL_WINDOW_ENV: &str = "ZCCACHE_STALL_WINDOW_MS";

/// Resolve the alive-hung stall window from the environment.
fn stall_window() -> Duration {
    match std::env::var(STALL_WINDOW_ENV) {
        Ok(v) => match v.trim().parse::<u64>() {
            Ok(ms) => Duration::from_millis(ms),
            Err(_) => DEFAULT_STALL_WINDOW,
        },
        Err(_) => DEFAULT_STALL_WINDOW,
    }
}

/// The Mode B kill decision: a still-running child is "wedged" only when it has
/// produced no output for at least `stall_window` AND its CPU time has not
/// advanced across the last sample. Requiring BOTH conditions is what keeps a
/// silent-but-CPU-bound compile (advancing CPU) and a chatty-but-slow compile
/// (advancing output) alive. Pure so it is trivially unit-testable.
fn should_kill_stalled(
    since_progress: Duration,
    stall_window: Duration,
    cpu_advanced: bool,
) -> bool {
    since_progress >= stall_window && !cpu_advanced
}

/// Total CPU time (user+kernel) consumed by `child` so far, in an opaque
/// monotonically-increasing unit. Used ONLY for delta comparison ("did the
/// process burn any CPU since the last sample?"), never for absolute timing.
///
/// Returns `None` where per-process CPU accounting is unavailable (an
/// unsupported platform, or the handle/pid is already gone). Callers treat
/// `None` as "assume progress" so Mode B can never false-kill on a platform it
/// cannot measure — it simply falls back to the output-only signal there.
fn child_cpu_ticks(child: &Child) -> Option<u64> {
    child
        .id()
        .and_then(crate::platform::process::inspect::cpu_ticks)
}

/// Await a spawned child, draining stdout/stderr concurrently, with a
/// post-exit orphan-pipe watchdog (issue #962).
///
/// Behaves exactly like [`tokio::process::Child::wait_with_output`] for a
/// well-behaved child: it returns once the process has exited and both pipes
/// have reached EOF, with the full captured output. The only divergence is the
/// wedge case: if the child has exited but a pipe has not reached EOF within
/// the drain grace, the watchdog returns the captured-so-far output with the
/// real exit status instead of blocking forever, and emits loud + durable
/// diagnostics.
///
/// The caller is expected to have spawned `child` with piped stdout/stderr and
/// `kill_on_drop(true)`; `cmd_desc` is a human-readable program identifier used
/// only in diagnostics.
pub(crate) async fn wait_with_output_watchdog(
    child: Child,
    cmd_desc: &str,
) -> std::io::Result<Output> {
    watchdog_inner(
        child,
        cmd_desc,
        post_exit_grace(),
        stall_window(),
        STALL_TICK,
    )
    .await
}

/// Streaming variant of [`wait_with_output_watchdog`]. The watchdog remains
/// the only owner of the child pipes, but forwards each read through a bounded
/// channel instead of retaining stdout/stderr itself.
pub(crate) async fn wait_with_output_watchdog_streaming(
    child: Child,
    cmd_desc: &str,
    sender: mpsc::Sender<RawOutputChunk>,
) -> std::io::Result<Output> {
    watchdog_inner_impl(
        child,
        cmd_desc,
        post_exit_grace(),
        stall_window(),
        STALL_TICK,
        Some(sender),
    )
    .await
}

/// [`wait_with_output_watchdog`] with an explicit post-exit drain grace and
/// Mode B (alive-hung) disabled, so tests can pin the grace without mutating
/// the process-global environment (which would race across parallel tests). A
/// `grace` of zero disables the watchdog and falls back to the historical
/// unbounded `wait_with_output`. Test-only: production callers use
/// [`wait_with_output_watchdog`] (both modes, env-configured).
#[cfg(test)]
async fn wait_with_output_watchdog_with_grace(
    child: Child,
    cmd_desc: &str,
    grace: Duration,
) -> std::io::Result<Output> {
    watchdog_inner(child, cmd_desc, grace, Duration::ZERO, STALL_TICK).await
}

/// Core watchdog loop.
///
/// - `grace` > 0 enables Mode A (issue #962): after the child exits, bound the
///   stdout/stderr EOF drain, abandoning it if an orphaned grandchild holds the
///   pipe.
/// - `stall_window` > 0 enables Mode B (issue #891): while the child is still
///   running, kill it if it makes no progress — no output AND no CPU — for that
///   long. See [`should_kill_stalled`].
///
/// With both zero this is a plain `wait_with_output`.
async fn watchdog_inner(
    child: Child,
    cmd_desc: &str,
    grace: Duration,
    stall_window: Duration,
    stall_tick: Duration,
) -> std::io::Result<Output> {
    watchdog_inner_impl(child, cmd_desc, grace, stall_window, stall_tick, None).await
}

async fn watchdog_inner_impl(
    mut child: Child,
    cmd_desc: &str,
    grace: Duration,
    stall_window: Duration,
    stall_tick: Duration,
    stream: Option<mpsc::Sender<RawOutputChunk>>,
) -> std::io::Result<Output> {
    // Both modes disabled: historical behavior (host opt-out for exotic
    // pipelines needing strict EOF semantics).
    if grace.is_zero() && stall_window.is_zero() && stream.is_none() {
        return child.wait_with_output().await;
    }

    // Capture the pid up front for diagnostics (issue #893): by the time Mode A
    // fires the child has already exited and `child.id()` returns `None`, so we
    // record it now while it is still live.
    let child_pid = child.id();
    let mut stdout = child.stdout.take();
    let mut stderr = child.stderr.take();
    let mut out: Vec<u8> = Vec::new();
    let mut err: Vec<u8> = Vec::new();
    let mut stdout_bytes = 0usize;
    let mut stderr_bytes = 0usize;
    // Heap-allocated read buffers, NOT `[0u8; 64 * 1024]` stack arrays: these
    // are live across the `select!` await, so a stack array would embed 128 KiB
    // in this future — and thus in the whole deeply-nested compile-pipeline
    // future that contains it. Constructing/moving that oversized future
    // overflows the tokio worker-thread stack on Linux (observed as
    // `fatal runtime error: stack overflow` SIGABRT across the daemon
    // integration suite; Windows' larger default stack masked it).
    let mut sbuf = vec![0u8; 64 * 1024];
    let mut ebuf = vec![0u8; 64 * 1024];
    let mut stdout_done = stdout.is_none();
    let mut stderr_done = stderr.is_none();
    // Pipe-read failures, kept rather than discarded (soldr#1857). An
    // `io::Error` here ends the drain exactly like a clean EOF, so without
    // recording it a broken pipe is indistinguishable from "the child printed
    // nothing" — and the caller is left with a non-zero exit and no cause. The
    // module contract above is explicit that the watchdog complains loudly and
    // writes a durable event when it abandons a drain; these two arms were the
    // one path that did neither.
    let mut stdout_read_error: Option<std::io::Error> = None;
    let mut stderr_read_error: Option<std::io::Error> = None;
    // Exit status + the instant the child exited, captured together so the
    // grace deadline never needs an `unwrap`.
    let mut exited: Option<(ExitStatus, Instant)> = None;
    // Mode B (alive-hung, issue #891) progress tracking. `last_progress` is
    // reset on every non-empty read; `last_cpu` is the previous CPU sample so a
    // tick can tell whether the child burned CPU since the last check.
    let mode_b = !stall_window.is_zero();
    let mut last_progress = Instant::now();
    let mut last_cpu = if mode_b {
        child_cpu_ticks(&child)
    } else {
        None
    };

    loop {
        // Clean completion: process exited AND both pipes reached EOF.
        if let (Some((status, _)), true, true) = (exited, stdout_done, stderr_done) {
            if let Some(e) = stderr_read_error.as_ref().or(stdout_read_error.as_ref()) {
                emit_pipe_read_error_diagnostics(
                    cmd_desc,
                    child_pid,
                    e,
                    stdout_read_error.is_some(),
                    stderr_read_error.is_some(),
                    stdout_bytes,
                    stderr_bytes,
                    &status,
                );
                deliver_fault_note(
                    &status,
                    stderr_bytes,
                    stream.as_ref(),
                    &mut err,
                    &format!("reading its output pipe failed ({e})"),
                )
                .await;
            }
            return Ok(Output {
                status,
                stdout: out,
                stderr: err,
            });
        }

        // Post-exit grace: only armed once the child has exited. Until then it
        // is `pending()` so the watchdog can never fire while the child is
        // still running (safe for multi-minute links). The remaining duration
        // is captured as a `Copy` value so the future does not borrow `exited`
        // (which the child-exit branch mutates in the same `select!`).
        let grace_remaining: Option<Duration> =
            exited.map(|(_, at)| grace.saturating_sub(at.elapsed()));
        let grace_deadline = async move {
            match grace_remaining {
                Some(remaining) => tokio::time::sleep(remaining).await,
                None => std::future::pending::<()>().await,
            }
        };

        // Mode B tick: armed only while the child is still running and Mode B
        // is enabled. Fires every `STALL_TICK` to sample progress; `pending()`
        // otherwise so it never competes once the child has exited (Mode A
        // takes over then).
        let stall_armed = mode_b && exited.is_none();
        let stall_tick_fut = async move {
            if stall_armed {
                tokio::time::sleep(stall_tick).await;
            } else {
                std::future::pending::<()>().await;
            }
        };

        tokio::select! {
            // Concurrent drain of both pipes prevents the classic
            // fill-the-pipe-then-block deadlock; the child-exit wait runs
            // alongside so we notice exit promptly.
            status = child.wait(), if exited.is_none() => {
                let status = status?;
                exited = Some((status, Instant::now()));
            }
            r = read_opt(stdout.as_mut(), &mut sbuf), if !stdout_done => match r {
                Ok(0) => stdout_done = true,
                Ok(n) => {
                    stdout_bytes += n;
                    if let Some(sender) = stream.as_ref() {
                        sender
                            .send(RawOutputChunk::Stdout(sbuf[..n].to_vec()))
                            .await
                            .map_err(|_| std::io::Error::new(
                                std::io::ErrorKind::BrokenPipe,
                                "compiler output consumer disconnected",
                            ))?;
                    } else {
                        out.extend_from_slice(&sbuf[..n]);
                    }
                    last_progress = Instant::now();
                }
                Err(e) => {
                    stdout_read_error = Some(e);
                    stdout_done = true;
                }
            },
            r = read_opt(stderr.as_mut(), &mut ebuf), if !stderr_done => match r {
                Ok(0) => stderr_done = true,
                Ok(n) => {
                    stderr_bytes += n;
                    if let Some(sender) = stream.as_ref() {
                        sender
                            .send(RawOutputChunk::Stderr(ebuf[..n].to_vec()))
                            .await
                            .map_err(|_| std::io::Error::new(
                                std::io::ErrorKind::BrokenPipe,
                                "compiler output consumer disconnected",
                            ))?;
                    } else {
                        err.extend_from_slice(&ebuf[..n]);
                    }
                    last_progress = Instant::now();
                }
                Err(e) => {
                    stderr_read_error = Some(e);
                    stderr_done = true;
                }
            },
            () = grace_deadline, if exited.is_some() => {
                if let Some((status, at)) = exited {
                    emit_orphan_pipe_diagnostics(
                        cmd_desc,
                        child_pid,
                        grace,
                        at.elapsed(),
                        stdout_bytes,
                        stderr_bytes,
                        stdout_done,
                        stderr_done,
                    );
                    // Drop `stdout`/`stderr` (and, on return, `child`) so the
                    // read handles are released; the orphan grandchild is
                    // reaped by the daemon job object at daemon exit as the
                    // backstop. Returning here frees the compile-concurrency
                    // permit the caller holds — the whole point of #962.
                    return Ok(Output {
                        status,
                        stdout: out,
                        stderr: err,
                    });
                }
            }
            () = stall_tick_fut, if stall_armed => {
                // Mode B (issue #891): the child is still running. Sample CPU
                // and decide whether it is wedged — no output for the whole
                // stall window AND no CPU burned since the last sample. Either
                // signal advancing (fresh output, or CPU delta) resets/spares
                // it, so a silent-but-CPU-bound compile and a chatty-but-slow
                // one are both left alone.
                let now_cpu = child_cpu_ticks(&child);
                let cpu_advanced = match (last_cpu, now_cpu) {
                    (Some(prev), Some(cur)) => cur > prev,
                    // Unknown on this platform / handle gone → assume progress
                    // so Mode B never false-kills something it cannot measure.
                    _ => true,
                };
                last_cpu = now_cpu;
                if should_kill_stalled(last_progress.elapsed(), stall_window, cpu_advanced) {
                    emit_stall_diagnostics(
                        cmd_desc,
                        child_pid,
                        stall_window,
                        last_progress.elapsed(),
                        stdout_bytes,
                        stderr_bytes,
                    );
                    // Kill the wedged child and reap it to recover the real
                    // (killed) exit status; the compile-concurrency permit the
                    // caller holds is freed as soon as we return.
                    let _ = child.start_kill();
                    return match child.wait().await {
                        Ok(status) => {
                            deliver_fault_note(
                                &status,
                                stderr_bytes,
                                stream.as_ref(),
                                &mut err,
                                &format!(
                                    "zccache killed it after {}s with no output and no CPU                                      progress (ZCCACHE_STALL_WINDOW_MS)",
                                    stall_window.as_secs()
                                ),
                            )
                            .await;
                            Ok(Output {
                                status,
                                stdout: out,
                                stderr: err,
                            })
                        }
                        Err(e) => Err(e),
                    };
                }
            }
        }
    }
}

/// Give a failing child's output a cause when the child itself supplied none.
///
/// Why this is needed at all: on Windows [`Child::start_kill`] is
/// `TerminateProcess(handle, 1)`, so a child **we** killed reports **exactly**
/// `exit code 1` — byte-identical to a genuine `rustc` failure. Mode B only
/// fires when there has been no output, so stderr is empty by construction.
/// The caller therefore saw `error: could not compile <crate>` with an empty
/// cause and no way to tell "your code is broken" from "we shot the compiler".
/// On Unix the same kill yields `status.code() == None` (reported as `-1`),
/// which is precisely why this only ever reproduced on Windows (soldr#1857).
///
/// Only annotates when the child both failed and said nothing: `stderr_bytes`
/// counts bytes actually read from the pipe, so it is the honest signal on the
/// streaming path too (where `err` stays empty because bytes went to the
/// consumer rather than the buffer). A compile that produced real diagnostics
/// is never touched.
async fn deliver_fault_note(
    status: &ExitStatus,
    stderr_bytes: usize,
    stream: Option<&mpsc::Sender<RawOutputChunk>>,
    err: &mut Vec<u8>,
    reason: &str,
) {
    if status.success() || stderr_bytes > 0 {
        return;
    }
    let note = format!(
        "zccache: the compiler produced no diagnostics — {reason}
"
    )
    .into_bytes();
    match stream {
        // Streaming path: the buffered `err` is discarded downstream, so the
        // note has to travel as a chunk or it never reaches the caller.
        Some(sender) => {
            let _ = sender.send(RawOutputChunk::Stderr(note)).await;
        }
        None => err.extend_from_slice(&note),
    }
}

/// Complain about a pipe-read failure, matching the forensics contract in this
/// module's header: warn loudly and write a durable lifecycle event.
///
/// Before soldr#1857 both read arms were `Err(_) => done = true`, which ended
/// the drain exactly like a clean EOF while discarding the error. That made a
/// broken pipe the one output-loss path in this file with **no** telemetry at
/// all, so a non-zero exit with empty stderr was unattributable after the fact.
#[allow(clippy::too_many_arguments)]
fn emit_pipe_read_error_diagnostics(
    cmd_desc: &str,
    pid: Option<u32>,
    error: &std::io::Error,
    stdout_failed: bool,
    stderr_failed: bool,
    stdout_bytes: usize,
    stderr_bytes: usize,
    status: &ExitStatus,
) {
    tracing::warn!(
        event = "child_wait_watchdog_fired",
        stage = "pipe_read_error",
        cmd = %cmd_desc,
        pid = pid.unwrap_or(0),
        error = %error,
        stdout_failed,
        stderr_failed,
        stdout_bytes,
        stderr_bytes,
        exit_code = status.code().unwrap_or(-1),
        "reading the child's output pipe failed; the drain ended early and any          diagnostics still buffered in that pipe are lost. The exit status is          still the child's real one, so a non-zero exit here may carry no          explanation of its own (soldr#1857)."
    );
    crate::core::lifecycle::write_event(
        crate::core::lifecycle::EVENT_CHILD_WAIT_WATCHDOG_FIRED,
        serde_json::json!({
            "stage": "pipe_read_error",
            "cmd": cmd_desc,
            "pid": pid,
            "error": error.to_string(),
            "stdout_failed": stdout_failed,
            "stderr_failed": stderr_failed,
            "stdout_bytes": stdout_bytes,
            "stderr_bytes": stderr_bytes,
            "exit_code": status.code(),
        }),
    );
    // Also record it in the dedicated termination stream so these are
    // countable without grepping the interleaved lifecycle log (#1857).
    crate::core::lifecycle::write_event_to_named_log(
        crate::core::lifecycle::TERMINATION_LOG_FILENAME,
        crate::core::lifecycle::EVENT_CHILD_WAIT_WATCHDOG_FIRED,
        serde_json::json!({
            "stage": "pipe_read_error",
            "cmd": cmd_desc,
            "pid": pid,
            "error": error.to_string(),
            "stdout_failed": stdout_failed,
            "stderr_failed": stderr_failed,
            "stdout_bytes": stdout_bytes,
            "stderr_bytes": stderr_bytes,
            "exit_code": status.code(),
        }),
    );
}

/// Read into `buf` from an optional reader, or pend forever when the reader is
/// gone. Lets a `tokio::select!` branch stay disabled (via its `if` guard)
/// without ever evaluating a missing reader.
async fn read_opt<R: AsyncRead + Unpin>(
    reader: Option<&mut R>,
    buf: &mut [u8],
) -> std::io::Result<usize> {
    match reader {
        Some(reader) => reader.read(buf).await,
        None => std::future::pending().await,
    }
}

/// Loud + durable diagnostics for a fired orphan-pipe watchdog, per the
/// daemon's "every timeout/watchdog fire is logged loud + durable for
/// forensics" rule.
#[allow(clippy::too_many_arguments)]
fn emit_orphan_pipe_diagnostics(
    cmd_desc: &str,
    pid: Option<u32>,
    grace: Duration,
    elapsed_since_exit: Duration,
    stdout_bytes: usize,
    stderr_bytes: usize,
    stdout_done: bool,
    stderr_done: bool,
) {
    tracing::warn!(
        event = "child_wait_watchdog_fired",
        stage = "post_exit_pipe_drain",
        cmd = %cmd_desc,
        pid = pid.unwrap_or(0),
        grace_ms = grace.as_millis() as u64,
        elapsed_since_exit_ms = elapsed_since_exit.as_millis() as u64,
        stdout_bytes,
        stderr_bytes,
        stdout_eof = stdout_done,
        stderr_eof = stderr_done,
        "child exited but a stdout/stderr pipe did not reach EOF within the drain grace — \
         an orphaned grandchild inherited the pipe write handle; abandoning the drain and \
         returning captured output so the daemon does not park forever and leak a \
         compile-concurrency permit (issue #962)"
    );
    crate::core::lifecycle::write_event(
        crate::core::lifecycle::EVENT_CHILD_WAIT_WATCHDOG_FIRED,
        serde_json::json!({
            "stage": "post_exit_pipe_drain",
            "cmd": cmd_desc,
            "pid": pid,
            "grace_ms": grace.as_millis() as u64,
            "elapsed_since_exit_ms": elapsed_since_exit.as_millis() as u64,
            "stdout_bytes": stdout_bytes,
            "stderr_bytes": stderr_bytes,
            "stdout_eof": stdout_done,
            "stderr_eof": stderr_done,
            "reason": "orphaned grandchild inherited the pipe write handle; drain abandoned to free the compile-concurrency permit",
        }),
    );
    // Also record it in the dedicated termination stream so these are
    // countable without grepping the interleaved lifecycle log (#1857).
    crate::core::lifecycle::write_event_to_named_log(
        crate::core::lifecycle::TERMINATION_LOG_FILENAME,
        crate::core::lifecycle::EVENT_CHILD_WAIT_WATCHDOG_FIRED,
        serde_json::json!({
            "stage": "post_exit_pipe_drain",
            "cmd": cmd_desc,
            "pid": pid,
            "grace_ms": grace.as_millis() as u64,
            "elapsed_since_exit_ms": elapsed_since_exit.as_millis() as u64,
            "stdout_bytes": stdout_bytes,
            "stderr_bytes": stderr_bytes,
            "stdout_eof": stdout_done,
            "stderr_eof": stderr_done,
            "reason": "orphaned grandchild inherited the pipe write handle; drain abandoned to free the compile-concurrency permit",
        }),
    );
}

/// Loud + durable diagnostics for a fired alive-hung (Mode B) watchdog, per the
/// forensics rule. Emitted only when the child made no progress — no output AND
/// no CPU — for the whole stall window, so it is a genuine wedge, not a slow
/// build.
fn emit_stall_diagnostics(
    cmd_desc: &str,
    pid: Option<u32>,
    stall_window: Duration,
    since_progress: Duration,
    stdout_bytes: usize,
    stderr_bytes: usize,
) {
    tracing::warn!(
        event = "child_wait_watchdog_fired",
        stage = "alive_hung_no_progress",
        cmd = %cmd_desc,
        pid = pid.unwrap_or(0),
        stall_window_ms = stall_window.as_millis() as u64,
        since_progress_ms = since_progress.as_millis() as u64,
        stdout_bytes,
        stderr_bytes,
        "child is still running but produced no output AND burned no CPU for the \
         stall window — treating it as wedged; killing it so the daemon does not \
         park forever and leak a compile-concurrency permit (issue #891). This is \
         progress-based, not a wall-clock cap: a compile emitting output or burning \
         CPU is never affected."
    );
    crate::core::lifecycle::write_event(
        crate::core::lifecycle::EVENT_CHILD_WAIT_WATCHDOG_FIRED,
        serde_json::json!({
            "stage": "alive_hung_no_progress",
            "cmd": cmd_desc,
            "pid": pid,
            "stall_window_ms": stall_window.as_millis() as u64,
            "since_progress_ms": since_progress.as_millis() as u64,
            "stdout_bytes": stdout_bytes,
            "stderr_bytes": stderr_bytes,
            "reason": "no output and no CPU progress for the stall window; killed as wedged",
        }),
    );
    // Also record it in the dedicated termination stream so these are
    // countable without grepping the interleaved lifecycle log (#1857).
    crate::core::lifecycle::write_event_to_named_log(
        crate::core::lifecycle::TERMINATION_LOG_FILENAME,
        crate::core::lifecycle::EVENT_CHILD_WAIT_WATCHDOG_FIRED,
        serde_json::json!({
            "stage": "alive_hung_no_progress",
            "cmd": cmd_desc,
            "pid": pid,
            "stall_window_ms": stall_window.as_millis() as u64,
            "since_progress_ms": since_progress.as_millis() as u64,
            "stdout_bytes": stdout_bytes,
            "stderr_bytes": stderr_bytes,
            "reason": "no output and no CPU progress for the stall window; killed as wedged",
        }),
    );
}

#[cfg(test)]
#[path = "child_watchdog_tests.rs"]
mod tests;
