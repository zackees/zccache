//! Integration tests for the `ZCCACHE_NO_SPAWN` host guard (issue #982).
//!
//! Embedding hosts (e.g. soldr's compiled-in `zccache` trampoline, which
//! serves compiles through an embedded in-process zccache service) set
//! `ZCCACHE_NO_SPAWN=1` to forbid the CLI from ever spawning a standalone
//! `zccache-daemon` / `zccache-download-daemon` process.
//!
//! Contract under test: a subcommand that would spawn a daemon must
//! - exit non-zero,
//! - name `ZCCACHE_NO_SPAWN` in its error output,
//! - spawn no daemon process, and
//! - leave no `zccache-daemon.*` runtime-binaries copy behind.
//!
//! The tests run the real `zccache` binary as a subprocess with an isolated
//! `ZCCACHE_CACHE_DIR` + `ZCCACHE_DAEMON_NAMESPACE`, so env handling is
//! race-free and an accidental spawn (the RED state of this test) cannot
//! touch the developer's real daemon.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::panic_in_result_fn,
    clippy::unwrap_in_result
)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// Path to the zccache binary built by cargo for this integration test.
///
/// Using `CARGO_BIN_EXE_zccache` makes cargo build the binary automatically
/// before running the test (requires the `zccache-bin` feature, which the
/// integration lane enables via `ZCCACHE_INTEGRATION_FEATURES`).
fn zccache_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_zccache"))
}

/// Isolated environment for one guard test: unique cache root + daemon
/// namespace so nothing here can collide with the developer's real daemon.
struct GuardEnv {
    bin: PathBuf,
    cache_dir: tempfile::TempDir,
    namespace: String,
}

impl GuardEnv {
    fn new(tag: &str) -> Self {
        Self {
            bin: zccache_bin(),
            cache_dir: tempfile::tempdir().expect("create temp cache dir"),
            namespace: format!("no-spawn-{tag}-{}", std::process::id()),
        }
    }

    /// Run `zccache <args>` with the guard env var set to `no_spawn_value`.
    fn run_guarded(&self, args: &[&str], no_spawn_value: &str) -> Output {
        Command::new(&self.bin)
            .args(args)
            .env("ZCCACHE_CACHE_DIR", self.cache_dir.path())
            .env("ZCCACHE_DAEMON_NAMESPACE", &self.namespace)
            .env("ZCCACHE_NO_SPAWN", no_spawn_value)
            .output()
            .expect("run zccache subcommand")
    }

    fn assert_refused(&self, args: &[&str], no_spawn_value: &str) {
        let output = self.run_guarded(args, no_spawn_value);
        let combined = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            !output.status.success(),
            "`zccache {}` must fail under ZCCACHE_NO_SPAWN={no_spawn_value}, got success.\noutput: {combined}",
            args.join(" ")
        );
        assert!(
            combined.contains("ZCCACHE_NO_SPAWN"),
            "error output must name ZCCACHE_NO_SPAWN so operators can find the knob.\noutput: {combined}"
        );
        assert_no_daemon_artifacts(self.cache_dir.path());
    }
}

impl Drop for GuardEnv {
    /// Best-effort cleanup: in the RED state (guard not implemented) the
    /// subcommand really spawns an isolated daemon — stop it so a failing
    /// test run cannot leak a process.
    fn drop(&mut self) {
        let _ = Command::new(&self.bin)
            .arg("stop")
            .env("ZCCACHE_CACHE_DIR", self.cache_dir.path())
            .env("ZCCACHE_DAEMON_NAMESPACE", &self.namespace)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
}

/// No deployed `zccache-daemon` binary may exist anywhere under the isolated
/// cache root (see `materialize_daemon_exe` — the copy is the last step before
/// the actual process spawn, gated behind the same no-spawn guard).
fn assert_no_daemon_artifacts(root: &Path) {
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            let name = entry.file_name().to_string_lossy().to_string();
            assert!(
                !name.starts_with("zccache-daemon"),
                "found deployed daemon copy {path:?} — the guard must refuse before materialize_daemon_exe"
            );
        }
    }
}

/// `zccache start` is the most direct spawn path (`cmd_start` →
/// `spawn_and_wait`). Under the guard it must refuse.
#[test]
fn start_refuses_to_spawn_under_no_spawn() {
    GuardEnv::new("start").assert_refused(&["start"], "1");
}

/// `zccache session-start` reaches the client-side lazy-spawn path
/// (`cmd_session_start` → `ensure_daemon`). Under the guard it must refuse
/// rather than kill/replace/spawn anything.
#[test]
fn session_start_refuses_to_spawn_under_no_spawn() {
    GuardEnv::new("session").assert_refused(&["session-start"], "1");
}

/// The guard accepts the same value grammar as `ZCCACHE_DISABLE`:
/// `1` or case-insensitive `true`.
#[test]
fn guard_accepts_true_value_variant() {
    GuardEnv::new("truevar").assert_refused(&["start"], "true");
}

/// `ZCCACHE_NO_SPAWN=0` must NOT trip the guard: `zccache cache-root` (a
/// daemon-free subcommand) succeeds and the error message machinery stays
/// out of the way.
#[test]
fn zero_value_does_not_trip_guard() {
    let env = GuardEnv::new("zero");
    let output = env.run_guarded(&["cache-root"], "0");
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        output.status.success(),
        "`zccache cache-root` must succeed with ZCCACHE_NO_SPAWN=0.\noutput: {combined}"
    );
    assert!(
        !combined.contains("ZCCACHE_NO_SPAWN"),
        "no guard error may appear when the guard is off.\noutput: {combined}"
    );
}
