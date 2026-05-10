//! Stop hook: runs full workspace lint and tests.
//!
//! Smart mode: only runs if files were actually changed during this session.
//! Session fingerprint is captured at session start (check-on-start.py) and
//! compared here. If nothing changed during the session, everything is skipped.
//!
//! Exit codes:
//!   0 - All passed or skipped (no changes during session)
//!   2 - Lint or test failures (stderr fed back to Claude)

use std::env;
use std::fs;
use std::path::Path;
use std::process::{Command, ExitCode, Stdio};
use std::time::Duration;

use wait_timeout::ChildExt;
use zccache_core::NormalizedPath;

const TIMEOUT_SECS: u64 = 120;

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
// Process tree + thread dumping (replaces Python's tasklist/wmic/ps calls)
// ---------------------------------------------------------------------------

fn dump_process_tree(pid: u32, label: &str) {
    eprintln!("Process tree for {label} (PID {pid}):");

    let mut sys = sysinfo::System::new();
    sys.refresh_processes(sysinfo::ProcessesToUpdate::All, true);

    let target = sysinfo::Pid::from_u32(pid);

    // Dump target process
    if let Some(p) = sys.process(target) {
        eprintln!(
            "  PID={pid} name={} status={:?} cpu={:.1}% mem={}KB",
            p.name().to_string_lossy(),
            p.status(),
            p.cpu_usage(),
            p.memory() / 1024,
        );
        eprintln!("  cmd={:?}", p.cmd());
    }

    // Dump children recursively
    dump_children(&sys, target, 1);
}

fn dump_children(sys: &sysinfo::System, parent: sysinfo::Pid, depth: usize) {
    let indent = "  ".repeat(depth + 1);
    for (pid, p) in sys.processes() {
        if p.parent() == Some(parent) {
            eprintln!(
                "{indent}child PID={pid} name={} status={:?} cpu={:.1}% mem={}KB",
                p.name().to_string_lossy(),
                p.status(),
                p.cpu_usage(),
                p.memory() / 1024,
            );
            eprintln!("{indent}cmd={:?}", p.cmd());

            // Dump thread info where available
            dump_threads(*pid);

            // Recurse into grandchildren
            dump_children(sys, *pid, depth + 1);
        }
    }
}

/// Dump thread-level information for a process.
///
/// On Linux: enumerates `/proc/[pid]/task/` for thread IDs and status.
/// On other platforms: no-op (process-level info from sysinfo is sufficient).
fn dump_threads(pid: sysinfo::Pid) {
    #[cfg(target_os = "linux")]
    {
        let task_dir = format!("/proc/{pid}/task");
        if let Ok(entries) = fs::read_dir(&task_dir) {
            let tids: Vec<u32> = entries
                .filter_map(|e| e.ok())
                .filter_map(|e| e.file_name().to_str()?.parse::<u32>().ok())
                .collect();

            if tids.len() > 1 {
                eprintln!("    threads ({}):", tids.len());
                for tid in &tids {
                    let status_path = format!("/proc/{pid}/task/{tid}/status");
                    if let Ok(status) = fs::read_to_string(&status_path) {
                        let name = status
                            .lines()
                            .find(|l| l.starts_with("Name:"))
                            .map(|l| l.trim_start_matches("Name:").trim())
                            .unwrap_or("?");
                        let state = status
                            .lines()
                            .find(|l| l.starts_with("State:"))
                            .map(|l| l.trim_start_matches("State:").trim())
                            .unwrap_or("?");
                        eprintln!("      tid={tid} name={name} state={state}");
                    }
                }
            }
        }
    }

    #[cfg(not(target_os = "linux"))]
    {
        let _ = pid;
    }
}

// ---------------------------------------------------------------------------
// Pre-check: kill running daemon so cargo can replace the exe
// ---------------------------------------------------------------------------

fn kill_daemon() {
    use sysinfo::ProcessesToUpdate;

    let mut sys = sysinfo::System::new();
    sys.refresh_processes(ProcessesToUpdate::All, true);

    for (pid, process) in sys.processes() {
        let name = process.name().to_string_lossy();
        if name == "zccache-daemon" || name == "zccache-daemon.exe" {
            eprintln!("Killing running daemon (PID {pid}) to unlock target binaries");
            process.kill();
        }
    }
}

// ---------------------------------------------------------------------------
// Subprocess execution with timeout
// ---------------------------------------------------------------------------

fn run_streaming(root: &Path, cmd: &[String], label: &str) -> (i32, bool) {
    let mut child = match Command::new(&cmd[0])
        .args(&cmd[1..])
        .current_dir(root)
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{label}: failed to spawn: {e}");
            return (-1, false);
        }
    };

    match child.wait_timeout(Duration::from_secs(TIMEOUT_SECS)) {
        Ok(Some(status)) => (status.code().unwrap_or(-1), false),
        Ok(None) => {
            eprintln!("\n{}", "=".repeat(60));
            eprintln!("TIMEOUT: {label} exceeded {TIMEOUT_SECS}s — dumping process tree");
            eprintln!("{}", "=".repeat(60));

            dump_process_tree(child.id(), label);

            let _ = child.kill();
            let _ = child.wait();
            (-1, true)
        }
        Err(e) => {
            eprintln!("{label}: wait error: {e}");
            (-1, false)
        }
    }
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

fn main() -> ExitCode {
    let root = project_root();

    let level = check_level(&root);

    if level == CheckLevel::Skip {
        eprintln!("Skipping stop checks (no uncommitted changes)");
        return ExitCode::SUCCESS;
    }

    // Ensure all spawned cargo/rustc processes find the rustup toolchain
    activate_rustup_toolchain(&root);

    // Kill any running daemon so cargo can replace the exe on Windows
    kill_daemon();

    if level == CheckLevel::QuickCheck {
        eprintln!("Pre-existing dirty files — running cargo check");
        let check_cmd: Vec<String> = vec![
            "uv".into(),
            "run".into(),
            "cargo".into(),
            "check".into(),
            "--workspace".into(),
            "--all-targets".into(),
        ];
        let (rc, timed_out) = run_streaming(&root, &check_cmd, "Quick check");
        if timed_out {
            eprintln!("Quick check timed out");
            return ExitCode::from(2);
        }
        if rc != 0 {
            eprintln!("Quick check failed — uncommitted files do not compile");
            return ExitCode::from(2);
        }
        eprintln!("Quick check passed");
        return ExitCode::SUCCESS;
    }

    eprintln!("Running full workspace checks (changes detected)");

    // Run lint first. If it fails, skip tests entirely.
    let lint_cmd: Vec<String> = vec![
        "uv".into(),
        "run".into(),
        "python".into(),
        "-m".into(),
        "ci.lint".into(),
        "--fix".into(),
    ];
    let (lint_rc, lint_timeout) = run_streaming(&root, &lint_cmd, "Lint");

    if lint_timeout {
        eprintln!("Lint timed out — skipping tests");
        return ExitCode::from(2);
    }
    if lint_rc != 0 {
        eprintln!("Lint failed — skipping tests");
        return ExitCode::from(2);
    }

    // Cross-compile check: on Windows, verify code also compiles for Linux.
    // Catches #[cfg(windows)]-gated code called from cross-platform tests/code,
    // which local-only clippy/check cannot detect.
    if cfg!(windows) {
        let target = "x86_64-unknown-linux-musl";
        if is_target_installed(target) {
            let xcheck_cmd: Vec<String> = vec![
                "cargo".into(),
                "check".into(),
                "--target".into(),
                target.into(),
                "--workspace".into(),
                "--all-targets".into(),
            ];
            let (xcheck_rc, xcheck_timeout) =
                run_streaming(&root, &xcheck_cmd, "Cross-compile check (Linux)");
            if xcheck_timeout {
                eprintln!("Cross-compile check timed out — skipping remaining checks");
                return ExitCode::from(2);
            }
            if xcheck_rc != 0 {
                eprintln!("Cross-compile check failed — code does not compile for Linux");
                return ExitCode::from(2);
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
    let test_cmd: Vec<String> = vec![
        "cargo".into(),
        "test".into(),
        "--workspace".into(),
        "--lib".into(),
        "--exclude".into(),
        "zccache-daemon".into(),
    ];
    let (test_rc, test_timeout) = run_streaming(&root, &test_cmd, "Tests");

    if test_timeout {
        eprintln!("Tests timed out — failing");
        return ExitCode::from(2);
    }
    if test_rc != 0 {
        eprintln!("Tests failed");
        return ExitCode::from(2);
    }

    eprintln!("All checks passed");
    ExitCode::SUCCESS
}
