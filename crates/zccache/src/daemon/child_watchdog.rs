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
    wait_with_output_watchdog_with_grace(child, cmd_desc, post_exit_grace()).await
}

/// [`wait_with_output_watchdog`] with an explicit drain grace, so tests can pin
/// the grace without mutating the process-global environment (which would race
/// across parallel tests). A `grace` of zero disables the watchdog and falls
/// back to the historical unbounded `wait_with_output`.
pub(crate) async fn wait_with_output_watchdog_with_grace(
    mut child: Child,
    cmd_desc: &str,
    grace: Duration,
) -> std::io::Result<Output> {
    // Grace of zero == watchdog disabled: fall back to the historical behavior
    // so a host can opt out if some exotic pipeline needs strict EOF semantics.
    if grace.is_zero() {
        return child.wait_with_output().await;
    }

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
                Ok(n) => out.extend_from_slice(&sbuf[..n]),
                Err(_) => stdout_done = true,
            },
            r = read_opt(stderr.as_mut(), &mut ebuf), if !stderr_done => match r {
                Ok(0) => stderr_done = true,
                Ok(n) => err.extend_from_slice(&ebuf[..n]),
                Err(_) => stderr_done = true,
            },
            () = grace_deadline, if exited.is_some() => {
                if let Some((status, at)) = exited {
                    emit_orphan_pipe_diagnostics(
                        cmd_desc,
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
}
