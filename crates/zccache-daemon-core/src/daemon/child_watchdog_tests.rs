use super::*;
use std::process::Stdio;

fn piped(mut cmd: tokio::process::Command) -> tokio::process::Command {
    cmd.stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(Stdio::null())
        .kill_on_drop(true);
    cmd
}

/// soldr#1857: a failing child that explained nothing must come back with a
/// synthesized cause, so the caller is never left with a bare non-zero exit.
#[tokio::test]
async fn silent_failure_gets_a_synthesized_cause() {
    let status = status_of(1).await;
    let mut err = Vec::new();
    deliver_fault_note(&status, 0, None, &mut err, "we killed it for testing").await;

    let text = String::from_utf8_lossy(&err).into_owned();
    assert!(
        text.contains("produced no diagnostics"),
        "expected the silence to be named; got {text:?}"
    );
    assert!(
        text.contains("we killed it for testing"),
        "expected the reason to be carried through; got {text:?}"
    );
}

/// The guard that keeps this from becoming noise: a child that already
/// explained itself is left byte-for-byte alone.
#[tokio::test]
async fn failure_with_diagnostics_is_not_annotated() {
    let status = status_of(1).await;
    let mut err = b"error[E0308]: mismatched types
"
    .to_vec();
    let before = err.clone();
    // stderr_bytes > 0 == the child said something.
    deliver_fault_note(&status, err.len(), None, &mut err, "irrelevant").await;

    assert_eq!(err, before);
}

/// A successful compile is silent by design and must never be annotated.
#[tokio::test]
async fn success_is_never_annotated() {
    let status = status_of(0).await;
    let mut err = Vec::new();
    deliver_fault_note(&status, 0, None, &mut err, "irrelevant").await;

    assert!(err.is_empty(), "exit 0 must stay silent, got {err:?}");
}

/// A real `ExitStatus` with the requested code. `ExitStatus` has no
/// portable constructor, so spawn a shell that exits with it.
async fn status_of(code: i32) -> ExitStatus {
    #[cfg(windows)]
    let mut cmd = tokio::process::Command::new("cmd");
    #[cfg(windows)]
    cmd.args(["/c", &format!("exit {code}")]);
    #[cfg(unix)]
    let mut cmd = tokio::process::Command::new("sh");
    #[cfg(unix)]
    cmd.args(["-c", &format!("exit {code}")]);
    cmd.status().await.expect("spawn child")
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

#[cfg(unix)]
#[tokio::test]
async fn streaming_sink_preserves_output_and_orphan_watchdog() {
    crate::test_support::test_timeout(async {
        let mut cmd = tokio::process::Command::new("sh");
        cmd.args(["-c", "sleep 30 & printf 'live\\n'"]);
        let child = piped(cmd).spawn().expect("spawn");
        let (sender, mut receiver) = mpsc::channel(8);
        let wait = watchdog_inner_impl(
            child,
            "stream-orphan",
            Duration::from_millis(300),
            Duration::ZERO,
            STALL_TICK,
            Some(sender),
        );
        let collect = async {
            let mut stdout = Vec::new();
            while let Some(chunk) = receiver.recv().await {
                if let RawOutputChunk::Stdout(bytes) = chunk {
                    stdout.extend(bytes);
                }
            }
            stdout
        };
        let started = Instant::now();
        let (output, stdout) = tokio::join!(wait, collect);
        let output = output.expect("watchdog wait");

        assert!(started.elapsed() < Duration::from_secs(10));
        assert!(output.status.success());
        assert!(
            output.stdout.is_empty(),
            "streaming sink owns captured bytes"
        );
        assert_eq!(stdout, b"live\n");
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
        let out = wait_with_output_watchdog_with_grace(child, "orphan", Duration::from_millis(300))
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

#[tokio::test]
async fn streaming_sink_preserves_alive_hung_watchdog() {
    crate::test_support::test_timeout(async {
        let child = piped(sleeper_cmd()).spawn().expect("spawn");
        let (sender, mut receiver) = mpsc::channel(8);
        let wait = watchdog_inner_impl(
            child,
            "stream-sleeper",
            Duration::ZERO,
            Duration::from_millis(150),
            Duration::from_millis(50),
            Some(sender),
        );
        let drain = async { while receiver.recv().await.is_some() {} };
        let (output, ()) = tokio::join!(wait, drain);
        assert!(!output.expect("watchdog wait").status.success());
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
