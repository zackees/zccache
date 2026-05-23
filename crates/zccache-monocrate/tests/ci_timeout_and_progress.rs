//! Integration tests for the wall-clock timeout and per-stage progress
//! markers exposed by `zccache-ci`. Uses parameterized timeouts of a few
//! hundred milliseconds so the suite runs in well under a second.

use std::process::{Command, Stdio};
use std::time::Duration;

use zccache_monocrate::ci::{CapturingProgress, StageOutcome, StageRunner};

fn sleep_forever_cmd() -> Command {
    if cfg!(windows) {
        let mut c = Command::new("cmd");
        c.args(["/C", "ping -n 600 127.0.0.1 > NUL"]);
        c.stdout(Stdio::null()).stderr(Stdio::null());
        c
    } else {
        let mut c = Command::new("sh");
        c.args(["-c", "sleep 600"]);
        c.stdout(Stdio::null()).stderr(Stdio::null());
        c
    }
}

fn quick_exit_cmd(rc: i32) -> Command {
    if cfg!(windows) {
        let mut c = Command::new("cmd");
        c.args(["/C", &format!("exit {rc}")]);
        c.stdout(Stdio::null()).stderr(Stdio::null());
        c
    } else {
        let mut c = Command::new("sh");
        c.args(["-c", &format!("exit {rc}")]);
        c.stdout(Stdio::null()).stderr(Stdio::null());
        c
    }
}

#[test]
fn progress_markers_print_one_line_per_stage() {
    let mut runner =
        StageRunner::with_progress(Duration::from_secs(5), CapturingProgress::default());

    runner.start_stage("fmt-check");
    runner.start_stage("clippy");
    runner.start_stage("test");
    runner.finish();

    let lines = &runner.progress_ref().lines;
    assert_eq!(lines.len(), 4, "expected 4 lines, got {lines:?}");
    assert!(lines[0].ends_with("-> fmt-check"));
    assert!(lines[1].ends_with("-> clippy"));
    assert!(lines[2].ends_with("-> test"));
    assert!(lines[3].ends_with("done"));
}

#[test]
fn run_returns_exit_code_for_normal_child() {
    let mut runner = StageRunner::new(Duration::from_secs(5));
    let outcome = runner.run("ok", &mut quick_exit_cmd(0));
    assert_eq!(outcome, StageOutcome::Exited(0));

    let outcome = runner.run("fail", &mut quick_exit_cmd(7));
    assert_eq!(outcome, StageOutcome::Exited(7));
}

#[test]
fn timeout_kills_runaway_child_within_budget() {
    let mut runner =
        StageRunner::with_progress(Duration::from_millis(250), CapturingProgress::default());

    let started = std::time::Instant::now();
    let outcome = runner.run("hang", &mut sleep_forever_cmd());
    let elapsed = started.elapsed();

    assert_eq!(outcome, StageOutcome::GlobalTimeout);
    // Kill must complete promptly. Allow 5s headroom for slow CI hosts even
    // though the budget was 250ms.
    assert!(
        elapsed < Duration::from_secs(5),
        "timeout path took {elapsed:?}, should have been near 250ms"
    );

    assert_eq!(runner.last_stage(), Some("hang"));

    // Progress sink must record the stage that hung — that's the smoking
    // gun for diagnosing which stage was responsible.
    let lines = &runner.progress_ref().lines;
    assert!(
        lines.iter().any(|l| l.contains("-> hang")),
        "missing progress marker, got {lines:?}"
    );
}
