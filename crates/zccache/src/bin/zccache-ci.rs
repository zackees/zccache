//! Stop hook: runs full workspace lint and tests.
//!
//! Smart mode: only runs if files were actually changed during this session.
//! Session fingerprint is captured at session start (check-on-start.py) and
//! compared here. If nothing changed during the session, everything is skipped.
//!
//! Exit codes:
//!   0 - All passed or skipped (no changes during session)
//!   2 - Lint or test failures (stderr fed back to Claude)
//!
//! Safety nets (see issue #141 and `lib.rs`):
//!   * Wall-clock timeout (default 300s, override `ZCCACHE_CI_TIMEOUT_SECS`)
//!   * Per-stage progress markers
//!   * Orphan zccache-daemon reaper at startup

use std::env;
use std::fs;
use std::path::Path;
use std::process::{Command, ExitCode};

use zccache::ci::{reap_orphan_daemons, resolve_timeout, StageOutcome, StageRunner};
use zccache::core::NormalizedPath;

fn project_root() -> NormalizedPath {
    let current = env::current_dir().expect("cannot determine working directory");
    let mut dir = current.as_path();
    loop {
        if dir.join("Cargo.toml").exists() {
            if let Ok(content) = fs::read_to_string(dir.join("Cargo.toml")) {
                if content.contains("[workspace]") {
                    return dir.into();
                }
            }
        }
        dir = match dir.parent() {
            Some(p) => p,
            None => return current.into(),
        };
    }
}

fn current_fingerprint(root: &Path) -> Option<String> {
    let output = Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(root)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    if stdout.trim().is_empty() {
        return None;
    }
    // Match Python's check-on-start.py: md5 of the stdout string as UTF-8
    Some(format!("{:x}", md5::compute(stdout.as_bytes())))
}

fn session_fingerprint(root: &Path) -> Option<String> {
    let path = root.join(".cache").join("session_fingerprint.json");
    let content = fs::read_to_string(path).ok()?;
    let v: serde_json::Value = serde_json::from_str(&content).ok()?;
    v.get("fingerprint")?.as_str().map(String::from)
}

/// Returns `Skip` when repo is totally clean, `QuickCheck` when dirty files
/// exist but haven't changed this session, or `Full` when new changes detected.
fn check_level(root: &Path) -> CheckLevel {
    match current_fingerprint(root) {
        None => CheckLevel::Skip, // repo is clean
        Some(current) => match session_fingerprint(root) {
            None => CheckLevel::Full, // repo was clean at start → changes are new
            Some(session) if current == session => CheckLevel::QuickCheck, // pre-existing dirty files
            Some(_) => CheckLevel::Full, // fingerprint changed → new changes this session
        },
    }
}

#[derive(Debug, PartialEq)]
enum CheckLevel {
    /// No uncommitted changes — skip everything.
    Skip,
    /// Dirty files exist but unchanged this session — run `cargo check` only.
    QuickCheck,
    /// New changes detected — full lint + tests.
    Full,
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

/// Prefer the repo-local rustup installation so spawned cargo/rustc calls use
/// the pinned workspace toolchain. Fall back to the user home for compatibility.
fn activate_rustup_toolchain(root: &Path) {
    let project_cargo_home = root.join(".cargo");
    let project_rustup_home = root.join(".rustup");
    env::set_var("CARGO_HOME", &project_cargo_home);
    env::set_var("RUSTUP_HOME", &project_rustup_home);

    let mut candidates: Vec<NormalizedPath> =
        vec![NormalizedPath::new(project_cargo_home.join("bin"))];
    if let Some(home) = if cfg!(windows) {
        env::var("USERPROFILE").ok()
    } else {
        env::var("HOME").ok()
    } {
        candidates.push(NormalizedPath::new(
            Path::new(&home).join(".cargo").join("bin"),
        ));
    }

    for cargo_bin in candidates {
        if cargo_bin.is_dir() {
            let sep = if cfg!(windows) { ";" } else { ":" };
            let current = env::var("PATH").unwrap_or_default();
            env::set_var("PATH", format!("{}{sep}{current}", cargo_bin.display()));
            break;
        }
    }
}

/// Check whether a rustup cross-compilation target is installed.
fn is_target_installed(target: &str) -> bool {
    Command::new("rustup")
        .args(["target", "list", "--installed"])
        .output()
        .map(|o| {
            o.status.success()
                && String::from_utf8_lossy(&o.stdout)
                    .lines()
                    .any(|line| line.trim() == target)
        })
        .unwrap_or(false)
}

/// Build a `Command` configured to run `program args...` with `cwd = root`,
/// inheriting stdout/stderr.
fn build_cmd(root: &Path, program: &str, args: &[&str]) -> Command {
    use std::process::Stdio;
    let mut c = Command::new(program);
    c.args(args)
        .current_dir(root)
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    c
}

fn handle_outcome(stage: &str, outcome: StageOutcome) -> Option<ExitCode> {
    match outcome {
        StageOutcome::Exited(0) => None,
        StageOutcome::Exited(code) => {
            eprintln!("{stage} failed (exit {code})");
            Some(ExitCode::from(2))
        }
        StageOutcome::GlobalTimeout => {
            eprintln!("{stage} hit the wall-clock timeout — failing");
            Some(ExitCode::from(2))
        }
        StageOutcome::SpawnFailed => {
            eprintln!("{stage} could not be spawned — failing");
            Some(ExitCode::from(2))
        }
    }
}

fn main() -> ExitCode {
    let root = project_root();

    let level = check_level(&root);

    if level == CheckLevel::Skip {
        eprintln!("Skipping stop checks (no uncommitted changes)");
        return ExitCode::SUCCESS;
    }

    // Ensure all spawned cargo/rustc processes find the rustup toolchain
    activate_rustup_toolchain(&root);

    // Reap any zccache-daemon whose parent PID is gone — these are true
    // orphans from a previous force-killed agent turn. We deliberately do
    // NOT also kill live daemons: every stage below is `cargo check` /
    // `clippy` / `cargo test --exclude zccache-daemon`, none of which
    // rebuild `zccache-daemon.exe`, so there is no binary to unlock. The
    // prior blanket `kill_daemon()` here took down the soldr-managed daemon
    // mid-build and broke session continuity. If a future stage rebuilds
    // the daemon crate, call `kill_daemon()` immediately before that stage
    // only — never globally at startup. See issues #166 and #167.
    let _ = reap_orphan_daemons();

    // Run every stage under a shared wall-clock deadline (default 300s,
    // overridable via env var). On timeout the entire process tree is killed
    // and a diagnostic snapshot is dumped to stderr.
    let timeout = resolve_timeout();
    let mut runner = StageRunner::new(timeout);

    if level == CheckLevel::QuickCheck {
        eprintln!("Pre-existing dirty files — running cargo check");
        let mut cmd = build_cmd(
            &root,
            "soldr",
            &["cargo", "check", "--workspace", "--all-targets"],
        );
        let outcome = runner.run("quick-check", &mut cmd);
        if let Some(code) = handle_outcome("quick-check", outcome) {
            return code;
        }
        runner.finish();
        eprintln!("Quick check passed");
        return ExitCode::SUCCESS;
    }

    eprintln!("Running full workspace checks (changes detected)");

    // Run lint first. If it fails, skip tests entirely.
    let mut lint_cmd = build_cmd(&root, "uv", &["run", "python", "-m", "ci.lint", "--fix"]);
    let lint_outcome = runner.run("lint", &mut lint_cmd);
    if let Some(code) = handle_outcome("lint", lint_outcome) {
        return code;
    }

    // Cross-compile check: on Windows, verify code also compiles for Linux.
    // Catches #[cfg(windows)]-gated code called from cross-platform tests/code,
    // which local-only clippy/check cannot detect.
    if cfg!(windows) {
        let target = "x86_64-unknown-linux-musl";
        if is_target_installed(target) {
            let mut xcheck_cmd = build_cmd(
                &root,
                "soldr",
                &[
                    "cargo",
                    "check",
                    "--target",
                    target,
                    "--workspace",
                    "--all-targets",
                ],
            );
            let xcheck_outcome = runner.run("cross-check-linux", &mut xcheck_cmd);
            if let Some(code) = handle_outcome("cross-check-linux", xcheck_outcome) {
                return code;
            }
        } else {
            eprintln!(
                "Skipping cross-compile check: target {target} not installed. \
                 Run `rustup target add {target}` to enable."
            );
        }
    }

    // Doc check is intentionally skipped here. `cargo doc --workspace --no-deps`
    // takes 5–15s, isn't intercepted by the rustc wrapper cache, and so runs
    // cold every Stop turn. The dedicated CI workflow (.github/workflows/ci.yml)
    // already runs it on every PR, and developers can still invoke it manually
    // with `RUSTDOCFLAGS="-D warnings" soldr cargo doc --workspace --no-deps`.
    // See #139 fix 2.

    // Unit tests only — skip integration/stress tests (which need
    // a fully compiled binary and are gated behind --include-ignored).
    // Exclude zccache-daemon: its server tests start a real daemon with
    // IPC + file watcher and are effectively integration tests.
    let mut test_cmd = build_cmd(
        &root,
        "soldr",
        &[
            "cargo",
            "test",
            "--workspace",
            "--lib",
            "--exclude",
            "zccache-daemon",
        ],
    );
    let test_outcome = runner.run("test", &mut test_cmd);
    if let Some(code) = handle_outcome("test", test_outcome) {
        return code;
    }

    runner.finish();
    eprintln!("All checks passed");
    ExitCode::SUCCESS
}
