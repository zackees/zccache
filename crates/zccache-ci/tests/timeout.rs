//! Integration tests for the wall-clock timeout exposed by `zccache-ci`.
//! Uses parameterized timeouts of a few hundred milliseconds so the suite
//! runs in well under a second.

use std::process::{Command, Stdio};
use std::time::Duration;

use zccache_ci::{StageOutcome, StageRunner};

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
fn run_returns_exit_code_for_normal_child() {
    let mut runner = StageRunner::new(Duration::from_secs(5));
    let outcome = runner.run("ok", &mut quick_exit_cmd(0));
    assert_eq!(outcome, StageOutcome::Exited(0));

    let outcome = runner.run("fail", &mut quick_exit_cmd(7));
    assert_eq!(outcome, StageOutcome::Exited(7));
}

#[test]
fn timeout_kills_runaway_child_within_budget() {
    let mut runner = StageRunner::new(Duration::from_millis(250));

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
}
