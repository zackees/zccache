//! Shared helpers used by every perf benchmark module.
//!
//! Contents:
//! - `ClientConn` platform-correct connection alias.
//! - Constants (`NUM_FILES`, `WARM_TRIALS`, RSP sizes, RUSTC sizes).
//! - `start_daemon` — boots an in-process daemon on a unique endpoint.
//! - Tool finders: `find_sccache`, `find_empp`, `find_archiver`.
//! - `bench_exe_name`, file-system cleanup helpers, generic tool runners.
//! - Session helpers: `start_zccache_session`, `end_zccache_session`,
//!   `clear_zccache`, `start_fresh_sccache`, `stop_sccache`.
//! - Reporting helpers: `median`, `fmt_dur`, `print_trials*`, `fmt_ratio`.

use std::path::Path;
use std::time::{Duration, Instant};
use zccache_monocrate::core::NormalizedPath;
use zccache_daemon::DaemonServer;
use zccache_protocol::{Request, Response};

#[cfg(unix)]
pub type ClientConn = zccache_ipc::IpcConnection;
#[cfg(windows)]
pub type ClientConn = zccache_ipc::IpcClientConnection;

pub const NUM_FILES: usize = 50;
pub const WARM_TRIALS: usize = 5;

/// Number of synthetic `-D` defines in the large response file.
pub const RSP_NUM_DEFINES: usize = 200;
/// Number of synthetic `-I` include paths in the large response file.
pub const RSP_NUM_INCLUDES: usize = 50;

pub const RUSTC_NUM_FILES: usize = 50;
pub const RUSTC_WARM_TRIALS: usize = 5;

/// Boot a daemon rooted at a fresh per-test cache directory.
///
/// The returned `tempfile::TempDir` must be held by the caller — when it is
/// dropped, the cache root (including the redb index) is deleted. Bind order
/// in the returned tuple is intentional: the `TempDir` is declared first so
/// Rust's reverse-order drop guarantees the daemon (`server_handle`) shuts
/// down before the cache root disappears.
pub async fn start_daemon() -> (
    tempfile::TempDir,
    String,
    tokio::task::JoinHandle<()>,
    std::sync::Arc<tokio::sync::Notify>,
) {
    let cache_dir = zccache_test_support::temp_cache_dir().unwrap();
    let endpoint = zccache_ipc::unique_test_endpoint();
    let normalized = zccache_monocrate::core::NormalizedPath::new(cache_dir.path());
    let mut server = DaemonServer::bind_with_cache_dir(&endpoint, &normalized).unwrap();
    let shutdown = server.shutdown_handle();
    let handle = tokio::spawn(async move {
        server.run(0).await.unwrap();
    });
    (cache_dir, endpoint, handle, shutdown)
}

pub fn find_sccache() -> Option<NormalizedPath> {
    for path in &["sccache", "C:/tools/python13/Scripts/sccache.exe"] {
        let p = NormalizedPath::new(path);
        if p.exists() {
            return Some(p);
        }
        if let Ok(output) = std::process::Command::new(path).arg("--version").output() {
            if output.status.success() {
                return Some(p);
            }
        }
    }
    None
}

pub fn find_empp() -> Option<NormalizedPath> {
    if let Some(p) = zccache_test_support::find_on_path("em++") {
        return Some(p);
    }
    let extra: &[&str] = if cfg!(windows) {
        &[
            "C:/emsdk/upstream/emscripten",
            "C:/Program Files/emsdk/upstream/emscripten",
        ]
    } else {
        &[
            "/usr/local/emsdk/upstream/emscripten",
            "/opt/emsdk/upstream/emscripten",
        ]
    };
    let suffix = if cfg!(windows) { ".bat" } else { "" };
    for dir in extra {
        let candidate = NormalizedPath::new(format!("{dir}/em++{suffix}"));
        if candidate.exists() {
            return Some(candidate);
        }
    }
    None
}

pub fn find_archiver() -> Option<NormalizedPath> {
    zccache_test_support::find_on_path("ar")
        .or_else(|| zccache_test_support::find_on_path("llvm-ar"))
}

pub fn bench_exe_name(stem: &str) -> String {
    if cfg!(windows) {
        format!("{stem}.exe")
    } else {
        stem.to_string()
    }
}

pub fn clean_objects(dir: &Path) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().and_then(|e| e.to_str()) == Some("o") {
            let _ = std::fs::remove_file(&path);
        }
    }
}

pub fn clear_dir_contents(dir: &Path) {
    if !dir.exists() {
        std::fs::create_dir_all(dir).unwrap();
        return;
    }
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            std::fs::remove_dir_all(&path).unwrap();
        } else {
            std::fs::remove_file(&path).unwrap();
        }
    }
}

pub fn remove_path_if_exists(path: &Path) {
    if path.is_dir() {
        let _ = std::fs::remove_dir_all(path);
    } else {
        let _ = std::fs::remove_file(path);
    }
}

pub fn remove_output_and_sidecars(output: &Path) {
    remove_path_if_exists(output);
    let Some(parent) = output.parent() else {
        return;
    };
    let Some(stem) = output.file_stem().and_then(|s| s.to_str()) else {
        return;
    };
    for ext in [
        "a", "data", "dSYM", "exe", "html", "js", "lib", "map", "pdb", "wasm",
    ] {
        remove_path_if_exists(&parent.join(format!("{stem}.{ext}")));
    }
}

pub fn clean_link_outputs(cwd: &Path, outputs: &[String]) {
    for output in outputs {
        let path = Path::new(output);
        if path.is_absolute() {
            remove_output_and_sidecars(path);
        } else {
            remove_output_and_sidecars(&cwd.join(path));
        }
    }
}

pub fn command_failure(description: &str, output: &std::process::Output) -> String {
    format!(
        "{description} failed with status {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

pub fn try_run_tool(
    tool: &Path,
    args: &[String],
    cwd: &Path,
    description: &str,
) -> Result<(), String> {
    let output = std::process::Command::new(tool)
        .args(args)
        .current_dir(cwd)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .map_err(|e| format!("failed to run {description}: {e}"))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(command_failure(description, &output))
    }
}

pub fn run_tool_timed(tool: &Path, args: &[String], cwd: &Path, description: &str) -> Duration {
    let start = Instant::now();
    let output = std::process::Command::new(tool)
        .args(args)
        .current_dir(cwd)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .unwrap_or_else(|e| panic!("failed to run {description}: {e}"));
    let elapsed = start.elapsed();
    assert!(
        output.status.success(),
        "{}",
        command_failure(description, &output)
    );
    elapsed
}

pub fn try_run_sccache_tool_timed(
    sccache: &Path,
    tool: &Path,
    args: &[String],
    cwd: &Path,
    description: &str,
) -> Result<Duration, String> {
    let start = Instant::now();
    let output = std::process::Command::new(sccache)
        .arg(tool)
        .args(args)
        .current_dir(cwd)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .map_err(|e| format!("failed to run {description}: {e}"))?;
    let elapsed = start.elapsed();
    if output.status.success() {
        Ok(elapsed)
    } else {
        Err(command_failure(description, &output))
    }
}

pub fn start_fresh_sccache(sccache: &Path, cache_dir: &Path) -> String {
    let cache_dir_str = cache_dir.to_string_lossy().into_owned();
    std::env::set_var("SCCACHE_DIR", &cache_dir_str);
    let _ = std::process::Command::new(sccache)
        .arg("--stop-server")
        .env("SCCACHE_DIR", &cache_dir_str)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    clear_dir_contents(cache_dir);
    let _ = std::process::Command::new(sccache)
        .arg("--start-server")
        .env("SCCACHE_DIR", &cache_dir_str)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    cache_dir_str
}

pub fn stop_sccache(sccache: &Path) {
    let _ = std::process::Command::new(sccache)
        .arg("--stop-server")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    std::env::remove_var("SCCACHE_DIR");
}

pub async fn clear_zccache(client: &mut ClientConn) {
    client.send(&Request::Clear).await.unwrap();
    match client.recv().await.unwrap() {
        Some(Response::Cleared { .. }) => {}
        other => panic!("expected Cleared, got: {other:?}"),
    }
}

pub async fn start_zccache_session(client: &mut ClientConn, working_dir: &str) -> String {
    client
        .send(&Request::SessionStart {
            client_pid: std::process::id(),
            working_dir: working_dir.into(),
            log_file: None,
            track_stats: true,
            journal_path: None,
            profile: false,
        })
        .await
        .unwrap();
    match client.recv().await.unwrap() {
        Some(Response::SessionStarted { session_id, .. }) => session_id,
        other => panic!("expected SessionStarted, got: {other:?}"),
    }
}

pub async fn end_zccache_session(client: &mut ClientConn, session_id: String) {
    client
        .send(&Request::SessionEnd { session_id })
        .await
        .unwrap();
    let _ = client.recv::<Response>().await;
}

// ── Reporting ───────────────────────────────────────────────────────────

pub fn median(times: &[Duration]) -> Duration {
    let mut sorted: Vec<Duration> = times.to_vec();
    sorted.sort();
    sorted[sorted.len() / 2]
}

pub fn fmt_dur(d: Duration) -> String {
    format!("{:.3}s", d.as_secs_f64())
}

pub fn print_trials(label: &str, times: &[Duration]) {
    print_trials_per(label, times, None);
}

/// Like `print_trials` but also reports per-call latency when `files_per_trial`
/// is known. Useful when a single trial sums N sequential cache lookups so the
/// reader can see whether the per-call cost is in the expected ~1ms range or
/// taking a slow path.
pub fn print_trials_per(label: &str, times: &[Duration], files_per_trial: Option<usize>) {
    let med = median(times);
    let min = times.iter().min().unwrap();
    let max = times.iter().max().unwrap();
    if let Some(n) = files_per_trial {
        let per_call_ms = (med.as_secs_f64() / n as f64) * 1000.0;
        eprintln!(
            "        {label:<14}{} ({} \u{2013} {}) -> {:.2} ms/call \u{00d7} {n}",
            fmt_dur(med),
            fmt_dur(*min),
            fmt_dur(*max),
            per_call_ms,
        );
    } else {
        eprintln!(
            "        {label:<14}{} ({} \u{2013} {})",
            fmt_dur(med),
            fmt_dur(*min),
            fmt_dur(*max),
        );
    }
}

pub fn fmt_ratio(baseline: Duration, test: Duration, bold: bool) -> String {
    let ratio = baseline.as_secs_f64() / test.as_secs_f64();
    let text = if ratio >= 10.0 {
        format!("{ratio:.0}x faster")
    } else if ratio >= 1.05 {
        format!("{ratio:.1}x faster")
    } else if ratio > 0.95 {
        "~same".to_string()
    } else {
        let inv = 1.0 / ratio;
        if inv >= 10.0 {
            format!("{inv:.0}x slower")
        } else {
            format!("{inv:.1}x slower")
        }
    };
    if bold && ratio >= 2.0 {
        format!("**{text}**")
    } else {
        text
    }
}
