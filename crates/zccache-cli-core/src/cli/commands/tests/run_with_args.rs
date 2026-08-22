//! Explicit-argv dispatch coverage for embedded CLI hosts (soldr#1593).

use std::path::Path;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use super::super::run_with_args;

/// Spawn-shaped calls. A dispatch that "regains process-spawn cost" has to
/// write one of these somewhere on the path.
const SPAWN_CALLS: [&str; 3] = ["Command::new(", ".spawn(", "exec("];

/// Source of a function body in this crate, brace-matched.
fn fn_body(relative: &str, name: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(relative);
    let source = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("failed to read {}: {e}", path.display()));
    let needle = format!("fn {name}(");
    let at = source
        .find(&needle)
        .unwrap_or_else(|| panic!("{} does not define `{needle}`", path.display()));
    let open = source[at..]
        .find('{')
        .map(|i| at + i)
        .unwrap_or_else(|| panic!("no body for `{needle}`"));
    let mut depth = 0usize;
    for (i, ch) in source[open..].char_indices() {
        match ch {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return source[open..=open + i].to_owned();
                }
            }
            _ => {}
        }
    }
    panic!("unterminated body for `{needle}`")
}

#[test]
fn explicit_argv_dispatch_spawns_no_process() {
    // Regression contract for soldr#1593: an explicit embedded dispatch must
    // stay a local function call rather than regaining process-spawn cost.
    //
    // Asserted structurally rather than by wall clock, because the wall clock
    // could not see it. `cmd_cache_root` does no IPC and spawns nothing --
    // resolve the root, format JSON, print -- so the work is ~1ms, while
    // spawning a debug binary in this repo measures ~145ms control-adjusted.
    // A regression that regained a re-exec would land near 150ms and *pass*
    // the 250ms bound this replaces. See PERF.md "Choosing a deadline" and
    // issue #1452.
    for (file, function) in [
        ("src/cli/commands/mod.rs", "run_with_args"),
        ("src/cli/commands/cache_ops.rs", "cmd_cache_root"),
    ] {
        let body = fn_body(file, function);
        for spawn in SPAWN_CALLS {
            assert!(
                !body.contains(spawn),
                "{function} in {file} must not contain `{spawn}`                  (soldr#1593: the embedded dispatch must stay in-process)"
            );
        }
    }
}

#[test]
fn perf_explicit_argv_dispatch_is_not_catastrophically_slow() {
    // Coarse backstop only. The spawn contract is enforced by
    // `explicit_argv_dispatch_spawns_no_process` above; this bound exists so a
    // dispatch that somehow blocks for seconds still fails, and is deliberately
    // far above both the ~1ms of real work and the ~145ms a re-exec would add.
    // Sized against a loaded CI runner, not this machine (PERF.md).
    let args = [
        "zccache".to_string(),
        "cache-root".to_string(),
        "--json".to_string(),
    ];
    let started = Instant::now();
    assert_eq!(run_with_args(&args), ExitCode::SUCCESS);
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_secs(30),
        "embedded cache-root dispatch took {elapsed:?}"
    );
}

#[test]
fn empty_explicit_argv_is_a_failure_not_a_panic() {
    assert_eq!(run_with_args(&[]), ExitCode::FAILURE);
}
