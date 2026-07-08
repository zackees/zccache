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
#[cfg(windows)]
fn child_cpu_ticks(child: &Child) -> Option<u64> {
    use windows_sys::Win32::Foundation::FILETIME;
    use windows_sys::Win32::System::Threading::GetProcessTimes;

    let handle = child.raw_handle()?;
    let mut creation = FILETIME {
        dwLowDateTime: 0,
        dwHighDateTime: 0,
    };
    let mut exit = creation;
    let mut kernel = creation;
    let mut user = creation;
    // SAFETY: `handle` is a live process handle owned by `child` (the child has
    // not been dropped); the four FILETIME out-params are valid stack storage.
    let ok = unsafe {
        GetProcessTimes(
            handle.cast::<std::ffi::c_void>(),
            &mut creation,
            &mut exit,
            &mut kernel,
            &mut user,
        )
    };
    if ok == 0 {
        return None;
    }
    let filetime_to_u64 =
        |ft: FILETIME| ((ft.dwHighDateTime as u64) << 32) | ft.dwLowDateTime as u64;
    Some(filetime_to_u64(kernel).wrapping_add(filetime_to_u64(user)))
}

#[cfg(target_os = "linux")]
fn child_cpu_ticks(child: &Child) -> Option<u64> {
    // /proc/<pid>/stat: `pid (comm) state ...`. `comm` can contain spaces and
    // parens, so split after the LAST ')' before tokenizing. utime is field 14
    // and stime is field 15 (1-based) overall → indices 11 and 12 of the
    // post-')' tokens (which start at field 3, `state`).
    let pid = child.id()?;
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let after_comm = stat.rsplit_once(')')?.1;
    let fields: Vec<&str> = after_comm.split_whitespace().collect();
    let utime: u64 = fields.get(11)?.parse().ok()?;
    let stime: u64 = fields.get(12)?.parse().ok()?;
    Some(utime.wrapping_add(stime))
}

#[cfg(target_os = "macos")]
fn child_cpu_ticks(child: &Child) -> Option<u64> {
    // macOS: `proc_pid_rusage(RUSAGE_INFO_V2)` reports the process's cumulative
    // user + system CPU time (nanoseconds, monotonic). Sufficient for the
    // delta-only "did it burn CPU?" check.
    let pid = child.id()? as libc::c_int;
    // SAFETY: zeroed POD struct; `proc_pid_rusage` fills it and returns 0 on
    // success. We pass a valid `&mut` and the matching V2 flavor.
    let mut info: libc::rusage_info_v2 = unsafe { std::mem::zeroed() };
    let rc = unsafe {
        libc::proc_pid_rusage(
            pid,
            libc::RUSAGE_INFO_V2,
            &mut info as *mut libc::rusage_info_v2 as *mut _,
        )
    };
    if rc != 0 {
        return None;
    }
    Some(info.ri_user_time.wrapping_add(info.ri_system_time))
}

#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
fn child_cpu_ticks(_child: &Child) -> Option<u64> {
    // Exotic non-CI targets (e.g. the BSDs) with no wired-up per-process CPU
    // accounting. Mode B relies on the output-progress signal alone here (the
    // `None` == "assume progress" rule), so it never false-kills; Mode A
    // (orphan-pipe) still applies.
    None
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
    mut child: Child,
    cmd_desc: &str,
    grace: Duration,
    stall_window: Duration,
    stall_tick: Duration,
) -> std::io::Result<Output> {
    // Both modes disabled: historical behavior (host opt-out for exotic
    // pipelines needing strict EOF semantics).
    if grace.is_zero() && stall_window.is_zero() {
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
                    out.extend_from_slice(&sbuf[..n]);
                    last_progress = Instant::now();
                }
                Err(_) => stdout_done = true,
            },
            r = read_opt(stderr.as_mut(), &mut ebuf), if !stderr_done => match r {
                Ok(0) => stderr_done = true,
                Ok(n) => {
                    err.extend_from_slice(&ebuf[..n]);
                    last_progress = Instant::now();
                }
                Err(_) => stderr_done = true,
            },
            () = grace_deadline, if exited.is_some() => {
                if let Some((status, at)) = exited {
                    emit_orphan_pipe_diagnostics(
                        cmd_desc,
                        child_pid,
                        grace,
                        at.elapsed(),
                        out.len(),
                        err.len(),
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
                        out.len(),
                        err.len(),
                    );
                    // Kill the wedged child and reap it to recover the real
                    // (killed) exit status; the compile-concurrency permit the
                    // caller holds is freed as soon as we return.
                    let _ = child.start_kill();
                    return match child.wait().await {
                        Ok(status) => Ok(Output {
                            status,
                            stdout: out,
                            stderr: err,
                        }),
                        Err(e) => Err(e),
                    };
                }
            }
        }
    }
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
        "child_wait_watchdog_fired",
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
        "child_wait_watchdog_fired",
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
mod tests {
    use super::*;
    use std::process::Stdio;

    fn piped(mut cmd: tokio::process::Command) -> tokio::process::Command {
        cmd.stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .stdin(Stdio::null())
            .kill_on_drop(true);
        cmd
    }

    /// Happy path: a well-behaved child that prints and exits must return its
    /// full output and real status, exactly like `wait_with_output`.
    #[tokio::test]
    async fn well_behaved_child_returns_full_output() {
        crate::test_support::test_timeout(async {
            #[cfg(windows)]
            let mut cmd = tokio::process::Command::new("cmd");
            #[cfg(windows)]
            cmd.args(["/c", "echo hello"]);
            #[cfg(unix)]
            let mut cmd = tokio::process::Command::new("sh");
            #[cfg(unix)]
            cmd.args(["-c", "echo hello"]);

            let child = piped(cmd).spawn().expect("spawn");
            let out = wait_with_output_watchdog_with_grace(child, "echo", Duration::from_secs(2))
                .await
                .expect("watchdog wait");
            assert!(out.status.success(), "status: {:?}", out.status);
            assert!(
                String::from_utf8_lossy(&out.stdout).contains("hello"),
                "stdout was: {:?}",
                String::from_utf8_lossy(&out.stdout)
            );
        })
        .await;
    }

    /// A child that exits nonzero still returns its captured output + status.
    #[tokio::test]
    async fn nonzero_exit_is_reported() {
        crate::test_support::test_timeout(async {
            #[cfg(windows)]
            let mut cmd = tokio::process::Command::new("cmd");
            #[cfg(windows)]
            cmd.args(["/c", "exit 3"]);
            #[cfg(unix)]
            let mut cmd = tokio::process::Command::new("sh");
            #[cfg(unix)]
            cmd.args(["-c", "exit 3"]);

            let child = piped(cmd).spawn().expect("spawn");
            let out = wait_with_output_watchdog_with_grace(child, "exit3", Duration::from_secs(2))
                .await
                .expect("watchdog wait");
            assert_eq!(out.status.code(), Some(3));
        })
        .await;
    }

    /// The #962 orphan-pipe wedge: the direct child exits immediately but leaves
    /// a backgrounded grandchild holding the stdout write handle open. The naive
    /// `wait_with_output` would block until the grandchild dies (30 s here); the
    /// watchdog must return within roughly the drain grace, carrying the output
    /// the child did produce.
    #[cfg(unix)]
    #[tokio::test]
    async fn orphan_holding_pipe_does_not_wedge() {
        use std::time::Instant;
        crate::test_support::test_timeout(async {
            // `sleep 30 &` inherits the shell's stdout write end and outlives
            // the shell, which prints `hi` and exits immediately.
            let mut cmd = tokio::process::Command::new("sh");
            cmd.args(["-c", "sleep 30 & echo hi"]);
            let child = piped(cmd).spawn().expect("spawn");

            // Tiny grace so the test is fast; still far above a real drain.
            let start = Instant::now();
            let out =
                wait_with_output_watchdog_with_grace(child, "orphan", Duration::from_millis(300))
                    .await
                    .expect("watchdog wait");

            assert!(
                start.elapsed() < Duration::from_secs(10),
                "watchdog did not fire; wait took {:?} (orphan wedge not bounded)",
                start.elapsed()
            );
            assert!(
                String::from_utf8_lossy(&out.stdout).contains("hi"),
                "captured output before firing should include the child's stdout"
            );
        })
        .await;
    }

    /// With the watchdog disabled (grace = 0) the wrapper is a straight
    /// pass-through to `wait_with_output` — used to opt out of the behavior.
    #[tokio::test]
    async fn zero_grace_disables_watchdog() {
        crate::test_support::test_timeout(async {
            #[cfg(windows)]
            let mut cmd = tokio::process::Command::new("cmd");
            #[cfg(windows)]
            cmd.args(["/c", "echo ok"]);
            #[cfg(unix)]
            let mut cmd = tokio::process::Command::new("sh");
            #[cfg(unix)]
            cmd.args(["-c", "echo ok"]);

            let child = piped(cmd).spawn().expect("spawn");
            let out = wait_with_output_watchdog_with_grace(child, "echo", Duration::ZERO)
                .await
                .expect("watchdog wait");
            assert!(out.status.success());
            assert!(String::from_utf8_lossy(&out.stdout).contains("ok"));
        })
        .await;
    }

    // ── Mode B: alive-hung / CPU-progress watchdog (issue #891) ──────────

    /// A process that sleeps: no output, ~0 CPU — the canonical wedge Mode B
    /// must catch. `>nul` keeps `ping` from writing to our captured stdout.
    fn sleeper_cmd() -> tokio::process::Command {
        #[cfg(windows)]
        {
            let mut c = tokio::process::Command::new("cmd");
            c.args(["/c", "ping -n 31 127.0.0.1 >nul"]);
            c
        }
        #[cfg(unix)]
        {
            let mut c = tokio::process::Command::new("sh");
            c.args(["-c", "sleep 30"]);
            c
        }
    }

    #[test]
    fn should_kill_stalled_only_when_silent_and_cpu_flat() {
        let w = Duration::from_secs(300);
        assert!(
            should_kill_stalled(Duration::from_secs(301), w, false),
            "no output past the window AND cpu flat → wedged"
        );
        assert!(
            !should_kill_stalled(Duration::from_secs(301), w, true),
            "cpu still advancing → never killed, even past the window"
        );
        assert!(
            !should_kill_stalled(Duration::from_secs(10), w, false),
            "within the window → never killed"
        );
        assert!(
            !should_kill_stalled(Duration::from_secs(10), w, true),
            "recent progress + cpu → never killed"
        );
    }

    /// Per-platform integration: per-process CPU sampling must actually work on
    /// every CI platform (Windows / Linux / macOS), not silently no-op.
    #[cfg(any(windows, target_os = "linux", target_os = "macos"))]
    #[tokio::test]
    async fn child_cpu_ticks_reports_for_live_process() {
        crate::test_support::test_timeout(async {
            let mut child = piped(sleeper_cmd()).spawn().expect("spawn");
            let ticks = super::child_cpu_ticks(&child);
            let _ = child.start_kill();
            let _ = child.wait().await;
            assert!(
                ticks.is_some(),
                "per-process CPU sampling must be wired up on this platform (#891)"
            );
        })
        .await;
    }

    /// End-to-end Mode B: a still-running child with no output and no CPU is
    /// killed within the (tiny, for the test) stall window instead of hanging.
    #[tokio::test]
    async fn alive_hung_no_progress_child_is_killed() {
        crate::test_support::test_timeout(async {
            let child = piped(sleeper_cmd()).spawn().expect("spawn");
            let start = Instant::now();
            // Mode A off (grace 0); Mode B on with a tiny window + tick.
            let out = watchdog_inner(
                child,
                "sleeper",
                Duration::ZERO,
                Duration::from_millis(150),
                Duration::from_millis(50),
            )
            .await
            .expect("watchdog wait");
            assert!(
                start.elapsed() < Duration::from_secs(15),
                "Mode B must kill the wedged child promptly (took {:?})",
                start.elapsed()
            );
            assert!(
                !out.status.success(),
                "a killed wedged child must not report success"
            );
        })
        .await;
    }

    // ── Windows pipe-deadlock regression harness (issue #892) ────────────

    /// A child that floods stderr past the OS pipe buffer (~64 KiB) *before*
    /// writing stdout, then exits. A sequential "read stdout to EOF, then
    /// stderr" drainer would deadlock — the child blocks on the full stderr
    /// pipe while the drainer waits for stdout that never comes. The watchdog
    /// drains both concurrently, so it must capture the full stderr flood + the
    /// stdout marker and return promptly. This is the pipe-saturation /
    /// missing-concurrent-drain case #892 asks for; on Windows it exercises the
    /// named-pipe stdio path specifically.
    #[tokio::test]
    async fn concurrent_drain_survives_pipe_saturation() {
        crate::test_support::test_timeout(async {
            const FLOOD: usize = 256 * 1024; // 4x a 64 KiB pipe buffer
            #[cfg(windows)]
            let cmd = {
                let mut c = tokio::process::Command::new("powershell");
                c.args([
                    "-NoProfile",
                    "-Command",
                    &format!("[Console]::Error.Write('b' * {FLOOD}); [Console]::Out.Write('done')"),
                ]);
                c
            };
            #[cfg(unix)]
            let cmd = {
                let mut c = tokio::process::Command::new("sh");
                c.args([
                    "-c",
                    &format!("yes b | tr -d '\\n' | head -c {FLOOD} 1>&2; printf done"),
                ]);
                c
            };

            let child = piped(cmd).spawn().expect("spawn");
            let start = Instant::now();
            let out = wait_with_output_watchdog(child, "saturate")
                .await
                .expect("watchdog wait");
            assert!(
                start.elapsed() < Duration::from_secs(20),
                "concurrent drain deadlocked on a saturated pipe (took {:?})",
                start.elapsed()
            );
            assert!(
                out.stderr.len() >= FLOOD,
                "full stderr flood must be captured: got {} of {FLOOD} bytes",
                out.stderr.len()
            );
            assert!(
                String::from_utf8_lossy(&out.stdout).contains("done"),
                "the post-flood stdout marker must be captured"
            );
        })
        .await;
    }

    // ── Concurrency preserved (issue #894) ───────────────────────────────

    /// A ~1s sleeper with no output — long enough to overlap, short enough for a
    /// fast test.
    fn short_sleep_cmd() -> tokio::process::Command {
        #[cfg(windows)]
        {
            let mut c = tokio::process::Command::new("cmd");
            c.args(["/c", "ping -n 2 127.0.0.1 >nul"]);
            c
        }
        #[cfg(unix)]
        {
            let mut c = tokio::process::Command::new("sh");
            c.args(["-c", "sleep 1"]);
            c
        }
    }

    /// The watchdog must not serialize concurrent child waits: running N of them
    /// at once should take about as long as one, not N times as long. Guards
    /// against a regression where the per-wait select loop / CPU sampling
    /// accidentally holds a shared lock or blocks a worker (acceptance for
    /// #894 — the bridge preserves compile concurrency).
    #[tokio::test]
    async fn concurrent_waits_are_not_serialized() {
        crate::test_support::test_timeout(async {
            const N: usize = 4;
            let start = Instant::now();
            let handles: Vec<_> = (0..N)
                .map(|_| {
                    let child = piped(short_sleep_cmd()).spawn().expect("spawn");
                    tokio::spawn(async move { wait_with_output_watchdog(child, "sleep1").await })
                })
                .collect();
            for h in handles {
                h.await.expect("join").expect("watchdog wait");
            }
            let elapsed = start.elapsed();
            // Serial would be ~N seconds; concurrent is ~1s. A generous 3s bound
            // (< N s) still fails loudly on any serialization while tolerating CI
            // scheduling jitter.
            assert!(
                elapsed < Duration::from_secs(3),
                "watchdog serialized {N} concurrent ~1s waits (took {elapsed:?}); \
                 concurrency was reduced (#894)"
            );
        })
        .await;
    }
}
