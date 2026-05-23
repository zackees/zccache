//! Small process/CLI-wide helpers shared by every subcommand module.
//!
//! Anything here is `pub(crate)` and called from multiple `cli::*` modules.

use std::path::Path;
use std::process::ExitCode;
use crate::core::NormalizedPath;

pub(crate) fn absolute_path(path: &str) -> NormalizedPath {
    let path = Path::new(path);
    if path.is_absolute() {
        path.into()
    } else {
        std::env::current_dir()
            .unwrap_or_default()
            .join(path)
            .into()
    }
}

/// Convert an i32 exit code to ExitCode without silent truncation.
/// A bare `exit_code as u8` wraps: 256 → 0 (success), masking failures.
/// This preserves success/failure semantics: non-zero stays non-zero.
pub(crate) fn exit_code_from_i32(code: i32) -> ExitCode {
    let truncated = (code & 0xFF) as u8;
    if code != 0 && truncated == 0 {
        ExitCode::from(1)
    } else {
        ExitCode::from(truncated)
    }
}

/// Matches setup-soldr's boolean env-var normalization: `1`, `true`, `yes`,
/// `on` (case-insensitive) are truthy; anything else (including `None`,
/// empty, `0`, `false`, `no`, `off`) is falsy. See zccache#184.
pub(crate) fn flag_truthy(value: Option<&str>) -> bool {
    let Some(raw) = value else { return false };
    let trimmed = raw.trim();
    matches!(trimmed, "1")
        || trimmed.eq_ignore_ascii_case("true")
        || trimmed.eq_ignore_ascii_case("yes")
        || trimmed.eq_ignore_ascii_case("on")
}

pub(crate) fn env_flag_truthy(name: &str) -> bool {
    flag_truthy(std::env::var(name).ok().as_deref())
}

pub(crate) fn resolve_endpoint(explicit: Option<&str>) -> String {
    if let Some(ep) = explicit {
        return ep.to_string();
    }
    if let Ok(ep) = std::env::var("ZCCACHE_ENDPOINT") {
        return ep;
    }
    crate::ipc::default_endpoint()
}

/// Platform-correct connect (returns different types on Unix vs Windows).
///
/// All in-process IPC sites route through this helper, so a single
/// `set_recv_timeout` call here applies the 5-minute default to every CLI
/// subcommand: Status, Shutdown, Clear, SessionStart, SessionStats,
/// FingerprintCheck/Mark/Invalidate, and — critically — the Compile /
/// CompileEphemeral / LinkEphemeral hot paths where the daemon does the
/// actual rustc/clang invocation and only responds when done. The 300s
/// budget accommodates the slowest legitimate unity / LTO workload while
/// still bounding "alive but stuck" hangs.
#[cfg(unix)]
pub(crate) async fn connect(
    endpoint: &str,
) -> Result<crate::ipc::IpcConnection, crate::ipc::IpcError> {
    let mut conn = crate::ipc::connect(endpoint).await?;
    conn.set_recv_timeout(crate::ipc::DEFAULT_CLIENT_RECV_TIMEOUT);
    Ok(conn)
}

#[cfg(windows)]
pub(crate) async fn connect(
    endpoint: &str,
) -> Result<crate::ipc::IpcClientConnection, crate::ipc::IpcError> {
    let mut conn = crate::ipc::connect(endpoint).await?;
    conn.set_recv_timeout(crate::ipc::DEFAULT_CLIENT_RECV_TIMEOUT);
    Ok(conn)
}

/// Cap on stdin bytes the wrapper will buffer before forwarding to the
/// daemon. 16 MiB matches the IPC frame budget — sources bigger than this
/// don't fit in a single Compile request anyway.
const MAX_STDIN_BYTES: usize = 16 * 1024 * 1024;

/// Read the wrapper's stdin to EOF when it's not a terminal (i.e. cargo or
/// some other parent has piped or redirected stdin into us), returning the
/// raw bytes. Interactive shells (stdin is a TTY) return an empty payload
/// without blocking on a read.
///
/// The cargo RUSTC_WRAPPER scenario normally hands the wrapper an
/// already-closed stdin (cargo opens `/dev/null` or an immediately-EOF pipe),
/// so the read returns `Ok(0)` and the cost is one syscall. The bytes flow
/// over IPC to the daemon, which forwards them to the compiler child so
/// invocations like `rustc -` (read source from stdin) still work.
pub(crate) fn slurp_stdin_if_piped() -> Vec<u8> {
    use std::io::IsTerminal;
    use std::io::Read;

    let mut stdin = std::io::stdin();
    if stdin.is_terminal() {
        return Vec::new();
    }
    let mut buf = Vec::new();
    let _ = stdin
        .by_ref()
        .take(MAX_STDIN_BYTES as u64)
        .read_to_end(&mut buf);
    buf
}

pub(crate) fn run_async(future: impl std::future::Future<Output = ExitCode>) -> ExitCode {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("failed to create tokio runtime")
        .block_on(future)
}

pub(crate) fn format_uptime(secs: u64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m {}s", secs / 60, secs % 60)
    } else {
        let h = secs / 3600;
        let m = (secs % 3600) / 60;
        format!("{h}h {m}m")
    }
}

pub(crate) fn format_duration_ms(ms: u64) -> String {
    if ms < 1000 {
        format!("{ms}ms")
    } else if ms < 60_000 {
        format!("{:.1}s", ms as f64 / 1000.0)
    } else {
        let secs = ms / 1000;
        format!("{}m {}s", secs / 60, secs % 60)
    }
}

pub(crate) fn format_bytes(bytes: u64) -> String {
    if bytes == 0 {
        "0 B".to_string()
    } else if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else if bytes < 1024 * 1024 * 1024 {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    } else {
        format!("{:.1} GB", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
    }
}

pub(crate) fn init_tracing() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .init();
}

pub(crate) fn print_json_value(value: &serde_json::Value) {
    match serde_json::to_string_pretty(value) {
        Ok(s) => println!("{s}"),
        Err(err) => {
            eprintln!("zccache: failed to encode JSON output: {err}");
            println!(r#"{{"status":"error","error":"failed to encode JSON output"}}"#);
        }
    }
}
