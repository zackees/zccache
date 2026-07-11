//! Explicit-argv dispatch coverage for embedded CLI hosts (soldr#1593).

use std::process::ExitCode;
use std::time::{Duration, Instant};

use super::super::run_with_args;

#[test]
fn perf_explicit_argv_dispatches_in_process_under_250ms() {
    // Regression/perf contract for soldr#1593: an explicit embedded dispatch
    // must stay a local function call rather than regaining process-spawn cost.
    let args = [
        "zccache".to_string(),
        "cache-root".to_string(),
        "--json".to_string(),
    ];
    let started = Instant::now();
    assert_eq!(run_with_args(&args), ExitCode::SUCCESS);
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_millis(250),
        "embedded cache-root dispatch took {elapsed:?}"
    );
}

#[test]
fn empty_explicit_argv_is_a_failure_not_a_panic() {
    assert_eq!(run_with_args(&[]), ExitCode::FAILURE);
}
