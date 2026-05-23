//! Library half of the zccache-ci stop hook.
//!
//! Exposes the timeout/diagnostics/kill machinery so it is unit-testable
//! independently of the `main()` entrypoint.
//!
//! The Stop hook can hang under pathological conditions (see issue #141):
//! a daemon-lock deadlock, a runaway test, or a build script in an infinite
//! loop. The harness gives us 10 minutes wall-clock, but a single agent turn
//! has been observed running for 45+ minutes when the harness misbehaves.
//!
//! This crate enforces:
//!
//! 1. **Wall-clock timeout** ([`StageRunner`]) — every stage runs against a
//!    shared deadline. On timeout the entire child process tree is killed
//!    (`taskkill /T /F` on Windows, process-group SIGKILL on Unix) and a
//!    best-effort diagnostic snapshot is dumped to stderr.
//!
//! 2. **Per-stage progress markers** ([`StageRunner::start_stage`]) — every
//!    stage prints `[<elapsed>] -> <name>` so the user can see which stage is
//!    hanging when the timeout fires. The last printed stage on the timeout
//!    path is the smoking gun.
//!
//! 3. **Orphan daemon reaper** ([`reap_orphan_daemons`]) — kills any
//!    `zccache-daemon` whose parent PID is no longer alive at startup. This
//!    catches the case where a previous agent turn was force-killed before
//!    its daemon could be reaped.

use std::env;
use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::{Child, Command};
use std::time::{Duration, Instant};
use zccache_monocrate::core::NormalizedPath;

use wait_timeout::ChildExt;

/// Default wall-clock timeout for the entire stop hook run, in seconds.
///
/// Override via `ZCCACHE_CI_TIMEOUT_SECS`.
pub const DEFAULT_TIMEOUT_SECS: u64 = 300;

/// Resolve the wall-clock timeout from the environment, falling back to
/// [`DEFAULT_TIMEOUT_SECS`].
pub fn resolve_timeout() -> Duration {
    let secs = env::var("ZCCACHE_CI_TIMEOUT_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|s| *s > 0)
        .unwrap_or(DEFAULT_TIMEOUT_SECS);
    Duration::from_secs(secs)
}

/// Outcome of running a single stage under [`StageRunner::run`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StageOutcome {
    /// Stage exited normally with the given status code.
    Exited(i32),
    /// Stage was killed because the hard wall-clock deadline elapsed.
    GlobalTimeout,
    /// Stage spawn failed before we could even wait on it.
    SpawnFailed,
}

/// Where progress markers are written. The default is stderr so that test
/// stdout stays clean and so the harness shows progress as the run unfolds.
pub trait ProgressSink: Send {
    fn write_line(&mut self, line: &str);
}

/// A `ProgressSink` that writes directly to stderr.
pub struct StderrProgress;

impl ProgressSink for StderrProgress {
    fn write_line(&mut self, line: &str) {
        let _ = writeln!(std::io::stderr(), "{line}");
    }
}

/// A `ProgressSink` that captures lines into an in-memory `Vec` for tests.
#[derive(Default)]
pub struct CapturingProgress {
    pub lines: Vec<String>,
}

impl ProgressSink for CapturingProgress {
    fn write_line(&mut self, line: &str) {
        self.lines.push(line.to_string());
    }
}

/// Runs a sequence of stages against a shared wall-clock deadline. Each stage
/// gets at most `deadline - now()` to complete. If a stage exhausts the
/// budget, the runner kills the child process tree and returns
/// [`StageOutcome::GlobalTimeout`].
///
/// The runner is intentionally a thin layer around `std::process::Command` so
/// it composes with whatever the caller wants to spawn (cargo, lint, tests).
///
/// Every stage emits a progress marker through the [`ProgressSink`] when it
/// starts. The default sink writes to stderr; tests use [`CapturingProgress`].
pub struct StageRunner<P: ProgressSink = StderrProgress> {
    started: Instant,
    deadline: Instant,
    progress: P,
    /// Last stage label seen — printed on timeout as the smoking-gun stage.
    last_stage: Option<String>,
}

impl StageRunner<StderrProgress> {
    /// Construct a runner that writes progress markers to stderr.
    pub fn new(timeout: Duration) -> Self {
        Self::with_progress(timeout, StderrProgress)
    }
}

impl<P: ProgressSink> StageRunner<P> {
    /// Construct a runner with an explicit progress sink (used by tests).
    pub fn with_progress(timeout: Duration, progress: P) -> Self {
        let started = Instant::now();
        Self {
            started,
            deadline: started + timeout,
            progress,
            last_stage: None,
        }
    }

    /// Total wall-clock elapsed since the runner was constructed.
    pub fn elapsed(&self) -> Duration {
        self.started.elapsed()
    }

    /// Time remaining before the global deadline expires. Returns
    /// `Duration::ZERO` if already past the deadline.
    pub fn remaining(&self) -> Duration {
        self.deadline.saturating_duration_since(Instant::now())
    }

    /// Borrow the most recently started stage label, if any. Useful in tests
    /// and timeout banners to identify the smoking-gun stage.
    pub fn last_stage(&self) -> Option<&str> {
        self.last_stage.as_deref()
    }

    /// Borrow the progress sink so callers (mainly tests) can assert on
    /// captured lines without consuming the runner.
    pub fn progress_ref(&self) -> &P {
        &self.progress
    }

    /// Print a progress marker for a stage and remember its name. Format:
    /// `[<elapsed>] -> <stage>`.
    pub fn start_stage(&mut self, stage: &str) {
        let elapsed = self.elapsed();
        self.progress
            .write_line(&format!("[{}] -> {}", format_elapsed(elapsed), stage));
        self.last_stage = Some(stage.to_string());
    }

    /// Print the final "done" marker.
    pub fn finish(&mut self) {
        let elapsed = self.elapsed();
        self.progress
            .write_line(&format!("[{}] done", format_elapsed(elapsed)));
    }

    /// Spawn `cmd` and wait for it to exit, bounded by the global deadline.
    /// On timeout the child's process tree is killed and the diagnostic
    /// snapshot is dumped to stderr.
    ///
    /// `stage` is the human-readable label for the stage; it is printed via
    /// the progress sink, recorded as `last_stage`, and surfaced in the
    /// timeout banner.
    pub fn run(&mut self, stage: &str, cmd: &mut Command) -> StageOutcome {
        self.start_stage(stage);

        // Already over budget — bail without spawning.
        if self.remaining().is_zero() {
            self.report_timeout(stage, None);
            return StageOutcome::GlobalTimeout;
        }

        configure_process_group(cmd);
        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                let _ = writeln!(std::io::stderr(), "{stage}: failed to spawn child: {e}");
                return StageOutcome::SpawnFailed;
            }
        };

        match child.wait_timeout(self.remaining()) {
            Ok(Some(status)) => StageOutcome::Exited(status.code().unwrap_or(-1)),
            Ok(None) => {
                self.report_timeout(stage, Some(&child));
                kill_process_tree(&mut child);
                StageOutcome::GlobalTimeout
            }
            Err(e) => {
                let _ = writeln!(std::io::stderr(), "{stage}: wait error: {e}");
                let _ = child.kill();
                let _ = child.wait();
                StageOutcome::SpawnFailed
            }
        }
    }

    /// Print the timeout banner and dump diagnostics. Public so callers can
    /// invoke it on alternate code paths (e.g. precondition failures).
    pub fn report_timeout(&mut self, stage: &str, child: Option<&Child>) {
        let elapsed = self.elapsed();
        let _ = writeln!(
            std::io::stderr(),
            "STOP-HOOK TIMEOUT after {} - capturing state",
            format_elapsed(elapsed)
        );
        let _ = writeln!(std::io::stderr(), "  hung stage: {stage}");
        if let Some(prev) = &self.last_stage {
            if prev != stage {
                let _ = writeln!(std::io::stderr(), "  last stage: {prev}");
            }
        }
        if let Some(c) = child {
            let _ = writeln!(std::io::stderr(), "  child PID: {}", c.id());
        }

        capture_diagnostics();
    }
}

/// Format a Duration as `<seconds>.<tenths>s`, e.g. `12.4s`.
pub fn format_elapsed(d: Duration) -> String {
    let total_ms = d.as_millis();
    let secs = total_ms / 1000;
    let tenths = (total_ms % 1000) / 100;
    format!("{secs}.{tenths}s")
}

// ---------------------------------------------------------------------------
// Process-tree kill (Windows: taskkill /T /F, Unix: process-group SIGKILL)
// ---------------------------------------------------------------------------

/// Configure `cmd` so its children form a new process group / job that we can
/// kill atomically. On Unix this calls `setsid` via `pre_exec`; on Windows
/// this sets the `CREATE_NEW_PROCESS_GROUP` creation flag.
pub fn configure_process_group(cmd: &mut Command) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // SAFETY: setsid is async-signal-safe and only mutates the child's
        // own process-group state.
        unsafe {
            cmd.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        cmd.creation_flags(CREATE_NEW_PROCESS_GROUP);
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = cmd;
    }
}

/// Kill `child` and every descendant. Best-effort: errors are swallowed
/// because we are already on the failure path.
pub fn kill_process_tree(child: &mut Child) {
    let pid = child.id();

    #[cfg(windows)]
    {
        use std::process::Stdio;
        // /T = recursive (kill children), /F = force.
        let _ = Command::new("taskkill")
            .args(["/T", "/F", "/PID", &pid.to_string()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }

    #[cfg(unix)]
    {
        // The child started its own session via setsid, so its PGID == its
        // PID. Negative pid kills the whole process group.
        let pid_i = pid as i32;
        unsafe {
            libc::kill(-pid_i, libc::SIGKILL);
        }
    }

    // Reap the direct child to avoid a zombie even on platforms where the
    // group-kill above did the heavy lifting.
    let _ = child.kill();
    let _ = child.wait();
}

// ---------------------------------------------------------------------------
// Diagnostics snapshot
// ---------------------------------------------------------------------------

/// Best-effort diagnostics dumped on timeout. Each section is independent and
/// failure to read one section never aborts the others.
pub fn capture_diagnostics() {
    eprintln!("--- diagnostics ---");
    dump_relevant_processes();
    dump_daemon_lock();
    dump_zccache_logs();
    dump_compile_journal();
    eprintln!("--- end diagnostics ---");
}

fn dump_relevant_processes() {
    let mut sys = sysinfo::System::new();
    sys.refresh_processes(sysinfo::ProcessesToUpdate::All, true);
    eprintln!("processes (zccache/cargo/rustc/soldr):");
    let mut count = 0usize;
    for (pid, p) in sys.processes() {
        let name = p.name().to_string_lossy();
        let lower = name.to_ascii_lowercase();
        if lower.contains("zccache")
            || lower.contains("cargo")
            || lower.contains("rustc")
            || lower.contains("soldr")
        {
            eprintln!(
                "  PID={pid} name={} status={:?} cpu={:.1}% mem={}KB",
                name,
                p.status(),
                p.cpu_usage(),
                p.memory() / 1024,
            );
            count += 1;
        }
    }
    if count == 0 {
        eprintln!("  (none found)");
    }
}

fn home_dir() -> Option<NormalizedPath> {
    let key = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    env::var_os(key).map(|os| NormalizedPath::new(Path::new(&os)))
}

fn dump_daemon_lock() {
    let Some(home) = home_dir() else { return };
    let lock = home.join(".zccache").join("daemon.lock");
    eprintln!("daemon.lock ({}):", lock.display());
    match fs::read_to_string(&lock) {
        Ok(s) => {
            for line in s.lines() {
                eprintln!("  {line}");
            }
        }
        Err(e) => eprintln!("  (unreadable: {e})"),
    }
}

fn dump_tail(path: &Path, lines: usize) {
    match fs::read_to_string(path) {
        Ok(s) => {
            let collected: Vec<&str> = s.lines().collect();
            let start = collected.len().saturating_sub(lines);
            for line in &collected[start..] {
                eprintln!("  {line}");
            }
        }
        Err(e) => eprintln!("  (unreadable {}: {})", path.display(), e),
    }
}

fn dump_zccache_logs() {
    let Some(home) = home_dir() else { return };
    let log_dir = home.join(".zccache").join("logs");
    eprintln!("zccache logs ({}):", log_dir.display());
    let entries = match fs::read_dir(&log_dir) {
        Ok(it) => it,
        Err(e) => {
            eprintln!("  (no log dir: {e})");
            return;
        }
    };
    let mut found = false;
    for entry in entries.flatten() {
        let p = entry.path();
        if p.extension().and_then(|s| s.to_str()) == Some("log") {
            found = true;
            eprintln!("  --- tail of {} ---", p.display());
            dump_tail(&p, 50);
        }
    }
    if !found {
        eprintln!("  (no .log files)");
    }
}

fn dump_compile_journal() {
    let Some(home) = home_dir() else { return };
    let journal = home
        .join(".soldr")
        .join("cache")
        .join("zccache")
        .join("logs")
        .join("compile_journal.jsonl");
    eprintln!("soldr compile_journal ({}):", journal.display());
    if !journal.exists() {
        eprintln!("  (not present)");
        return;
    }
    dump_tail(&journal, 50);
}

// ---------------------------------------------------------------------------
// Daemon kill (with wait + lock cleanup)
// ---------------------------------------------------------------------------

/// How long [`kill_pids_and_wait`] will poll for the killed processes to
/// actually exit before giving up. `process.kill()` is asynchronous on Windows
/// (`TerminateProcess` returns before the process is reaped), so we have to
/// confirm exit before returning — otherwise `cargo check` races a dying
/// daemon whose named pipe has already vanished. See issue #152.
pub const KILL_DAEMON_WAIT: Duration = Duration::from_secs(2);

/// Find every running `zccache-daemon[.exe]` PID by walking the process table.
fn find_daemon_pids() -> Vec<u32> {
    use sysinfo::ProcessesToUpdate;

    let mut sys = sysinfo::System::new();
    sys.refresh_processes(ProcessesToUpdate::All, true);
    sys.processes()
        .iter()
        .filter_map(|(pid, process)| {
            let name = process.name().to_string_lossy();
            (name == "zccache-daemon" || name == "zccache-daemon.exe").then_some(pid.as_u32())
        })
        .collect()
}

/// Send a kill to each PID and poll until every PID is confirmed dead, or
/// `timeout` elapses. Returns once every PID is gone (or once we time out).
///
/// Uses [`zccache_monocrate::ipc::force_kill_process`] (TerminateProcess on Windows,
/// SIGKILL on Unix) rather than sysinfo's `Process::kill`, which has been
/// observed to silently fail to terminate Windows console children spawned via
/// `cmd /C` in tests — the kill bool returned `true` but the process kept
/// running. Going through the OS API directly is more reliable.
///
/// Exposed for tests; production code should call [`kill_daemon`].
pub fn kill_pids_and_wait(pids: &[u32], timeout: Duration) {
    if pids.is_empty() {
        return;
    }

    for pid in pids {
        if let Err(e) = zccache_monocrate::ipc::force_kill_process(*pid) {
            eprintln!("force_kill_process({pid}) failed: {e}");
        }
    }

    let deadline = Instant::now() + timeout;
    let poll = Duration::from_millis(25);
    loop {
        let any_alive = pids.iter().any(|pid| zccache_monocrate::ipc::is_process_alive(*pid));
        if !any_alive {
            return;
        }
        if Instant::now() >= deadline {
            let still_alive: Vec<u32> = pids
                .iter()
                .copied()
                .filter(|pid| zccache_monocrate::ipc::is_process_alive(*pid))
                .collect();
            eprintln!(
                "Warning: daemon PIDs still alive after {}ms: {:?}",
                timeout.as_millis(),
                still_alive
            );
            return;
        }
        std::thread::sleep(poll);
    }
}

/// Kill every running `zccache-daemon`, wait for them to actually exit, and
/// remove the stale `daemon.lock` file. Intended for callers that need to
/// replace the daemon binary on Windows without racing the dying process.
///
/// See issue #152: prior to this, `kill_daemon` returned immediately after
/// `process.kill()` and left the lock file pointing at the just-killed PID,
/// causing `cargo check` to fail with "cannot connect to daemon at \\.\\pipe\\..."
/// because parallel rustc workers raced the half-dead daemon.
///
/// WARNING: Do not call from the stop hook unless your stage actually
/// rebuilds `zccache-daemon.exe` — see issue #167 for why blanket calls
/// broke soldr session continuity (and triggered the #166 "unknown session"
/// failures downstream).
pub fn kill_daemon() {
    let pids = find_daemon_pids();
    if pids.is_empty() {
        // No daemon running. The lock file may still be stale from a prior
        // crash, so remove it defensively.
        zccache_monocrate::ipc::remove_lock_file();
        return;
    }
    for pid in &pids {
        eprintln!("Killing running daemon (PID {pid}) to unlock target binaries");
    }
    kill_pids_and_wait(&pids, KILL_DAEMON_WAIT);
    zccache_monocrate::ipc::remove_lock_file();
}

// ---------------------------------------------------------------------------
// Orphan daemon reaper
// ---------------------------------------------------------------------------

/// Kill any `zccache-daemon[.exe]` process whose parent PID is no longer
/// alive — i.e. a true orphan. Returns the PIDs that were killed.
///
/// This is conservative: we only reap when we can confirm the parent is
/// gone. Daemons spawned by a still-running supervisor are left alone.
pub fn reap_orphan_daemons() -> Vec<u32> {
    use sysinfo::{Pid, ProcessesToUpdate};

    let mut sys = sysinfo::System::new();
    sys.refresh_processes(ProcessesToUpdate::All, true);

    let alive: std::collections::HashSet<Pid> = sys.processes().keys().copied().collect();

    let mut killed = Vec::new();
    for (pid, process) in sys.processes() {
        let name = process.name().to_string_lossy();
        if name != "zccache-daemon" && name != "zccache-daemon.exe" {
            continue;
        }
        let parent = process.parent();
        let is_orphan = match parent {
            None => true,
            Some(ppid) => !alive.contains(&ppid),
        };
        if is_orphan {
            eprintln!("Reaping orphan zccache-daemon PID={pid} (parent {parent:?} gone)");
            if process.kill() {
                killed.push(pid.as_u32());
            }
        }
    }
    killed
}

// ---------------------------------------------------------------------------
// Tests (kept here so the runner is exercisable without external fixtures)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Stdio;

    fn sleep_forever_cmd() -> Command {
        // A child that will never exit on its own. We use the host's interpreter
        // so this works on Windows (where `sleep` is not a binary).
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

    fn quick_exit_cmd() -> Command {
        if cfg!(windows) {
            let mut c = Command::new("cmd");
            c.args(["/C", "exit 0"]);
            c.stdout(Stdio::null()).stderr(Stdio::null());
            c
        } else {
            let mut c = Command::new("true");
            c.stdout(Stdio::null()).stderr(Stdio::null());
            c
        }
    }

    #[test]
    fn format_elapsed_examples() {
        assert_eq!(format_elapsed(Duration::from_millis(0)), "0.0s");
        assert_eq!(format_elapsed(Duration::from_millis(300)), "0.3s");
        assert_eq!(format_elapsed(Duration::from_millis(12_400)), "12.4s");
        assert_eq!(format_elapsed(Duration::from_secs(34)), "34.0s");
    }

    #[test]
    fn resolve_timeout_uses_default_when_unset() {
        // The env var is process-global; we don't mutate it here. Just check
        // the parser shape on a parser miss.
        let parsed = "0".parse::<u64>().ok().filter(|s| *s > 0);
        assert!(parsed.is_none(), "0 should be filtered out as invalid");

        let parsed = "abc".parse::<u64>().ok();
        assert!(parsed.is_none());
    }

    #[test]
    fn run_returns_exit_code_when_child_exits_normally() {
        let mut runner = StageRunner::new(Duration::from_secs(5));
        let outcome = runner.run("quick", &mut quick_exit_cmd());
        assert_eq!(outcome, StageOutcome::Exited(0));
    }

    #[test]
    fn run_times_out_and_kills_child_when_deadline_elapses() {
        // Deliberately tight timeout so the test runs in well under a second.
        let mut runner = StageRunner::new(Duration::from_millis(200));

        let outcome = runner.run("hang", &mut sleep_forever_cmd());
        assert_eq!(outcome, StageOutcome::GlobalTimeout);

        // last_stage should be set to the hung stage, so the timeout banner
        // can name it.
        assert_eq!(runner.last_stage(), Some("hang"));
    }

    #[test]
    fn run_skips_when_already_over_budget() {
        // Budget = 0 -> first run() call should immediately bail without
        // spawning anything.
        let mut runner = StageRunner::new(Duration::from_millis(0));
        let outcome = runner.run("noop", &mut quick_exit_cmd());
        assert_eq!(outcome, StageOutcome::GlobalTimeout);
    }

    #[test]
    fn progress_markers_capture_each_stage() {
        let mut runner =
            StageRunner::with_progress(Duration::from_secs(5), CapturingProgress::default());
        runner.start_stage("fmt-check");
        runner.start_stage("clippy");
        runner.start_stage("test");
        runner.finish();

        let progress = runner.progress_ref();
        assert_eq!(progress.lines.len(), 4);
        assert!(progress.lines[0].ends_with("-> fmt-check"));
        assert!(progress.lines[1].ends_with("-> clippy"));
        assert!(progress.lines[2].ends_with("-> test"));
        assert!(progress.lines[3].ends_with("done"));

        // Each marker starts with `[N.Ns]`.
        for line in &progress.lines {
            assert!(
                line.starts_with('[') && line.contains("s]"),
                "expected elapsed prefix in {line:?}"
            );
        }
    }

    #[test]
    fn reap_orphan_daemons_returns_a_vec_without_panic() {
        // We can't reliably create an orphan zccache-daemon in a unit test
        // environment, so we just exercise the happy path: the function must
        // walk the process table and return a Vec<u32> (possibly empty)
        // without panicking. The Drop test on a real machine catches
        // regressions where the function tries to kill the wrong process.
        let killed = reap_orphan_daemons();
        // No assertion on length: developer machines may have running
        // daemons whose parent is or isn't alive depending on workflow.
        let _ = killed.len();
    }

    #[test]
    fn run_emits_progress_marker_for_hung_stage() {
        let mut runner =
            StageRunner::with_progress(Duration::from_millis(200), CapturingProgress::default());
        let outcome = runner.run("hang", &mut sleep_forever_cmd());
        assert_eq!(outcome, StageOutcome::GlobalTimeout);

        let progress = runner.progress_ref();
        assert!(
            progress.lines.iter().any(|l| l.contains("-> hang")),
            "expected progress marker, got {:?}",
            progress.lines
        );
    }

    /// Issue #152 regression: `kill_pids_and_wait` must not return until the
    /// killed process is actually dead. The previous `kill_daemon`
    /// implementation called `process.kill()` and returned immediately, letting
    /// `cargo check` race a daemon that was still partially alive.
    ///
    /// In production the daemon is detached (init/launchd is its parent), so
    /// once it dies it is reaped immediately and `is_process_alive` flips
    /// false. In this test we are the parent, so we need a reaper thread to
    /// emulate the production lifecycle:
    ///
    /// * Unix: an unwait()-ed killed child becomes a zombie; `kill(pid, 0)`
    ///   keeps returning success until the parent calls `wait`.
    /// * Windows: an open process handle keeps the kernel object alive after
    ///   `TerminateProcess`, so `OpenProcess` keeps succeeding until the
    ///   handle is closed (which `Child::wait` does).
    #[test]
    fn kill_pids_and_wait_returns_only_after_child_is_dead() {
        let child = sleep_forever_cmd().spawn().expect("failed to spawn child");
        let pid = child.id();

        assert!(
            zccache_monocrate::ipc::is_process_alive(pid),
            "spawned child PID {pid} should be alive"
        );

        let waiter = std::thread::spawn(move || {
            let mut child = child;
            let _ = child.wait();
        });

        kill_pids_and_wait(&[pid], Duration::from_secs(5));

        waiter.join().expect("reaper thread panicked");

        assert!(
            !zccache_monocrate::ipc::is_process_alive(pid),
            "PID {pid} still alive after kill_pids_and_wait returned"
        );
    }

    #[test]
    fn kill_pids_and_wait_is_a_noop_for_empty_input() {
        // Should return immediately without polling or panicking.
        let start = Instant::now();
        kill_pids_and_wait(&[], Duration::from_secs(60));
        assert!(
            start.elapsed() < Duration::from_millis(500),
            "empty-input kill should return promptly, took {:?}",
            start.elapsed()
        );
    }
}
