//! Daemon lifecycle: start, stop, version probing, ensure-running, binary discovery.

use crate::core::NormalizedPath;
use std::process::ExitCode;

use super::util::{connect, resolve_endpoint, run_async, LOST_CONNECTION_MSG};

const DAEMON_PROFILE_ENV: &str = "ZCCACHE_DAEMON_PROFILE";
const TOKIO_CONSOLE_PROFILE: &str = "tokio-console";
const TOKIO_CONSOLE_BIND_ENV: &str = "TOKIO_CONSOLE_BIND";
const TOKIO_CONSOLE_OPEN_ENV: &str = "ZCCACHE_TOKIO_CONSOLE_OPEN";
const TOKIO_CONSOLE_DEFAULT_BIND: &str = "127.0.0.1:6669";
const PROFILE_START_REASON: &str = "tokio-console-profile-start";

/// Find the daemon binary. Looks next to the CLI binary first, then on PATH.
pub(crate) fn find_daemon_binary() -> Option<NormalizedPath> {
    let name = if cfg!(windows) {
        "zccache-daemon.exe"
    } else {
        "zccache-daemon"
    };

    // Look next to the CLI binary
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let candidate = dir.join(name);
            if candidate.exists() {
                return Some(candidate.into());
            }
        }
    }

    // Fall back to PATH
    which_on_path(name)
}

/// Simple PATH lookup (no external crate needed).
/// On Windows, also tries appending `.exe` if the name has no extension.
pub(crate) fn which_on_path(name: &str) -> Option<NormalizedPath> {
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate.into());
        }
        // On Windows, try with .exe suffix
        #[cfg(windows)]
        if std::path::Path::new(name).extension().is_none() {
            let with_exe = dir.join(format!("{name}.exe"));
            if with_exe.is_file() {
                return Some(with_exe.into());
            }
        }
    }
    None
}

pub(crate) async fn cmd_start(endpoint: &str) -> ExitCode {
    match crate::cli::runtime::ensure_daemon(endpoint).await {
        Ok(()) => {
            eprintln!("daemon running at {endpoint}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("failed to start daemon: {e}");
            ExitCode::FAILURE
        }
    }
}

pub(crate) async fn cmd_profile_start(endpoint: &str, bind: Option<&str>, open: bool) -> ExitCode {
    let open = open || env_truthy(TOKIO_CONSOLE_OPEN_ENV);
    let bind = tokio_console_bind(bind);
    let env = profile_env_overrides(&bind, open);
    let _guard = ScopedEnv::apply(&env);

    if let Err(e) =
        crate::cli::runtime::replace_running_daemon(endpoint, PROFILE_START_REASON).await
    {
        eprintln!("failed to start tokio-console daemon profile: {e}");
        return ExitCode::FAILURE;
    }
    eprintln!("daemon running with tokio-console profile at {bind}");

    if open {
        if let Err(e) = launch_tokio_console(&bind) {
            eprintln!("{e}");
            return ExitCode::FAILURE;
        }
    }

    ExitCode::SUCCESS
}

pub(crate) fn tokio_console_bind(bind: Option<&str>) -> String {
    bind.map(str::to_string)
        .or_else(|| std::env::var(TOKIO_CONSOLE_BIND_ENV).ok())
        .unwrap_or_else(|| TOKIO_CONSOLE_DEFAULT_BIND.to_string())
}

pub(crate) fn profile_env_overrides(bind: &str, open: bool) -> Vec<(String, String)> {
    let mut env = vec![
        (
            DAEMON_PROFILE_ENV.to_string(),
            TOKIO_CONSOLE_PROFILE.to_string(),
        ),
        (TOKIO_CONSOLE_BIND_ENV.to_string(), bind.to_string()),
    ];
    if open {
        env.push((TOKIO_CONSOLE_OPEN_ENV.to_string(), "1".to_string()));
    }
    env
}

fn launch_tokio_console(bind: &str) -> Result<(), String> {
    let mut cmd = std::process::Command::new("tokio-console");
    #[cfg(windows)]
    cmd.args(["--lang", "en_US.UTF-8"]);
    cmd.arg(bind);
    cmd.spawn()
        .map(|_| {
            eprintln!("launched tokio-console {bind}");
        })
        .map_err(|e| {
            format!(
                "daemon profile is running at {bind}, but failed to launch `tokio-console`: {e}. \
                 Install it with `cargo install --locked tokio-console` and run `tokio-console {bind}`."
            )
        })
}

fn env_truthy(name: &str) -> bool {
    std::env::var(name).is_ok_and(|value| {
        let value = value.trim();
        !value.is_empty()
            && !matches!(
                value.to_ascii_lowercase().as_str(),
                "0" | "false" | "no" | "off" | "n"
            )
    })
}

struct ScopedEnv {
    previous: Vec<(String, Option<String>)>,
}

impl ScopedEnv {
    fn apply(overrides: &[(String, String)]) -> Self {
        let previous = overrides
            .iter()
            .map(|(key, value)| {
                let old = std::env::var(key).ok();
                std::env::set_var(key, value);
                (key.clone(), old)
            })
            .collect();
        Self { previous }
    }
}

impl Drop for ScopedEnv {
    fn drop(&mut self) {
        for (key, value) in self.previous.iter().rev() {
            match value {
                Some(value) => std::env::set_var(key, value),
                None => std::env::remove_var(key),
            }
        }
    }
}

pub(crate) async fn cmd_stop(endpoint: &str) -> ExitCode {
    let recv_result = match crate::ipc::daemon_control_roundtrip(
        endpoint,
        crate::ipc::DaemonControlRequest::Shutdown,
        None,
    )
    .await
    {
        Ok(response) => response,
        Err(e) if crate::cli::client::is_daemon_unreachable_err(&e) => {
            // #1161: this used to fall back to `.or(read_lock_file_pid())`.
            // `check_running_daemon` returning `None` is precisely the
            // stale-lock / recycled-PID signal — it removes the lock file in
            // that case — so falling back to the raw number force-killed a PID
            // that had already failed verification, which is the #132 defense
            // being bypassed by the one path that always kills.
            let Some(pid) = crate::ipc::check_running_daemon() else {
                if let Some(stale) = crate::ipc::read_lock_file_pid() {
                    eprintln!(
                        "daemon not running at {endpoint}; lock file named process {stale}, \
                         which is not a live zccache daemon — leaving it alone and clearing \
                         the stale lock"
                    );
                    crate::ipc::remove_lock_file();
                } else {
                    eprintln!("daemon not running at {endpoint}");
                }
                // No daemon — but the index file might still be there from a
                // crashed prior run. Probe once so callers (CI tar) can rely
                // on the lock being gone after `zccache stop` returns.
                wait_for_daemon_teardown(endpoint).await;
                return ExitCode::SUCCESS;
            };

            match crate::ipc::force_kill_process(pid) {
                Ok(()) => {
                    for _ in 0..50 {
                        if !crate::ipc::is_process_alive(pid) {
                            crate::ipc::remove_lock_file();
                            eprintln!(
                                "daemon process {pid} terminated after IPC connection failed"
                            );
                            wait_for_daemon_teardown(endpoint).await;
                            return ExitCode::SUCCESS;
                        }
                        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    }
                    eprintln!(
                        "zccache: sent termination to daemon process {pid}, but it did not exit"
                    );
                    return ExitCode::FAILURE;
                }
                Err(e) => {
                    eprintln!(
                        "zccache: cannot connect to daemon at {endpoint}, and failed to kill \
                         locked process {pid}: {e}"
                    );
                    return ExitCode::FAILURE;
                }
            }
        }
        Err(e) => {
            eprintln!("zccache[err][R]: broken connection to daemon: {e}");
            return ExitCode::FAILURE;
        }
    };
    match recv_result {
        Some(crate::protocol::Response::ShuttingDown) => {
            // The daemon acknowledges `Shutdown` immediately and continues
            // teardown asynchronously. On Windows the redb index lock is held
            // until the daemon process actually exits and `Drop` fires. Wait
            // for the IPC endpoint to drop and for `index.redb` to be
            // openable (i.e. no exclusive share lock) so callers like the CI
            // post-step tar do not race the daemon. See issue #182.
            wait_for_daemon_teardown(endpoint).await;
            eprintln!("daemon stopped");
            ExitCode::SUCCESS
        }
        None => {
            eprintln!("{LOST_CONNECTION_MSG}");
            ExitCode::FAILURE
        }
        Some(other) => {
            eprintln!("zccache[err][U]: unexpected response from daemon: {other:?}");
            ExitCode::FAILURE
        }
    }
}

/// Default cap on how long `zccache stop` will wait after the daemon ACKs
/// `Shutdown` for the IPC endpoint to disappear and `index.redb` to become
/// openable. Overridable with `ZCCACHE_STOP_TIMEOUT_SECS`.
const STOP_WAIT_DEFAULT_SECS: u64 = 10;
/// Poll cadence inside the bounded wait loop.
const STOP_WAIT_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(100);

/// Returns the bounded total wait duration for `zccache stop`, honoring
/// `ZCCACHE_STOP_TIMEOUT_SECS` if it parses as a non-negative `u64`.
fn stop_wait_timeout() -> std::time::Duration {
    let secs = std::env::var("ZCCACHE_STOP_TIMEOUT_SECS")
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(STOP_WAIT_DEFAULT_SECS);
    std::time::Duration::from_secs(secs)
}

/// Poll until the IPC endpoint is unreachable. Emits a warning on timeout
/// but never fails the caller — the worst case is that the caller (e.g. CI
/// cache tar) sees the same error it would have seen without this wait.
///
/// The legacy redb-era version of this routine also waited for the index
/// file's exclusive share lock to drop on Windows. With the bincode blob
/// there is no file lock — `flush()` writes via temp+rename, holding the
/// file handle only briefly during the rename — so endpoint reachability
/// is the only signal we need.
pub(crate) async fn wait_for_daemon_teardown(endpoint: &str) {
    let deadline = std::time::Instant::now() + stop_wait_timeout();
    loop {
        if !is_ipc_endpoint_reachable(endpoint).await {
            return;
        }
        if std::time::Instant::now() >= deadline {
            eprintln!(
                "zccache: timed out waiting for daemon endpoint to disappear after stop; \
                 continuing anyway. set ZCCACHE_STOP_TIMEOUT_SECS to override."
            );
            return;
        }
        tokio::time::sleep(STOP_WAIT_POLL_INTERVAL).await;
    }
}

/// True if a fresh `connect()` to the daemon IPC endpoint succeeds.
async fn is_ipc_endpoint_reachable(endpoint: &str) -> bool {
    connect(endpoint).await.is_ok()
}

// Trampolines for top-level flags / `start`/`stop` so the dispatch
// match in `cli::mod` doesn't need its own runtime plumbing.
pub(crate) fn run_start() -> ExitCode {
    let endpoint = resolve_endpoint(None);
    run_async(cmd_start(&endpoint))
}

pub(crate) fn run_stop() -> ExitCode {
    let endpoint = resolve_endpoint(None);
    run_async(cmd_stop(&endpoint))
}
