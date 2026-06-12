//! Wrapper IPC request construction and response relay.

use crate::core::NormalizedPath;
use std::io::Write;
use std::path::Path;
use std::process::ExitCode;

use super::super::super::wedge_recv_timeout;
use super::super::daemon::{ensure_daemon, stop_stale_daemon};
use super::super::util::{connect, exit_code_from_i32, slurp_stdin_if_piped, LOST_CONNECTION_MSG};

pub(super) async fn cmd_compile(
    endpoint: &str,
    session_id: &str,
    args: Vec<String>,
    cwd: NormalizedPath,
    compiler: NormalizedPath,
    client_env: Vec<(String, String)>,
) -> ExitCode {
    let stdin_bytes = slurp_stdin_if_piped();
    let mut conn = match connect(endpoint).await {
        Ok(c) => c,
        Err(e) => {
            eprintln!("zccache[err][C]: cannot connect to daemon at {endpoint}: {e}");
            return ExitCode::FAILURE;
        }
    };

    let wire = crate::protocol::wire_prost::full_family_wire_format_from_env();
    let request = crate::protocol::Request::Compile {
        session_id: session_id.to_string(),
        args: args.clone(),
        cwd: cwd.clone(),
        compiler: compiler.clone(),
        env: Some(client_env.clone()),
        stdin: stdin_bytes.clone(),
    };
    if let Err(e) = conn.send_request(&request, wire).await {
        eprintln!("zccache[err][S]: failed to send to daemon: {e}");
        return ExitCode::FAILURE;
    }

    match compile_recv_with_wedge_detection(&mut conn).await {
        CompileRecvOutcome::Done(recv_result) => {
            relay_compile_response(recv_result, &mut std::io::stdout(), &mut std::io::stderr())
        }
        CompileRecvOutcome::Wedged => {
            // Daemon wedged mid-compile (issue #666). Recovery: force-kill
            // the wedged daemon (releases the IPC endpoint and frees other
            // workers waiting on `pipe.connect()`), spawn a fresh one, and
            // retry the compile in ephemeral mode (the new daemon doesn't
            // have the old session). The fall-through to ephemeral loses
            // session-stats accounting for this single compile but keeps
            // the build alive — a much better trade than the pre-#666
            // 300 s wall × N workers behaviour.
            eprintln!(
                "zccache[warn][W]: daemon at {endpoint} appears wedged \
                 (no response within wedge budget); recovering — issue #666"
            );
            drop(conn);
            stop_stale_daemon(endpoint).await;
            cmd_compile_ephemeral(endpoint, compiler.as_path(), args, cwd, client_env).await
        }
        CompileRecvOutcome::Failed(msg) => {
            eprintln!("zccache[err][R]: {msg}");
            ExitCode::FAILURE
        }
    }
}

#[allow(clippy::large_enum_variant)]
enum CompileRecvOutcome {
    // `Response` is large (cached compile result holds 2× Arc<Vec<u8>>),
    // but `CompileRecvOutcome` is only ever stack-local for one match arm
    // before being dropped — the extra indirection of Box would add an
    // allocation per request on the hot wrapper path for no real gain.
    Done(Option<crate::protocol::Response>),
    /// Daemon stopped responding within the configured wedge budget.
    Wedged,
    /// Non-timeout recv failure (broken pipe, deserialization error, etc.).
    Failed(String),
}

/// Wrap a compile-response recv with the [`wedge_recv_timeout`] budget.
///
/// Returns [`CompileRecvOutcome::Wedged`] only for the specific
/// `IpcError::Timeout` signal — everything else (graceful close, broken
/// pipe, protocol error) maps to [`CompileRecvOutcome::Failed`] so the
/// caller does not respawn the daemon on errors that have nothing to do
/// with a wedge.
async fn compile_recv_with_wedge_detection<C: ConnRecv>(conn: &mut C) -> CompileRecvOutcome {
    match wedge_recv_timeout() {
        Some(budget) => match conn.recv_with_timeout(budget).await {
            Ok(opt) => CompileRecvOutcome::Done(opt),
            Err(crate::ipc::IpcError::Timeout(_)) => CompileRecvOutcome::Wedged,
            Err(e) => CompileRecvOutcome::Failed(format!("broken connection to daemon: {e}")),
        },
        None => match conn.recv().await {
            Ok(opt) => CompileRecvOutcome::Done(opt),
            Err(e) => CompileRecvOutcome::Failed(format!("broken connection to daemon: {e}")),
        },
    }
}

/// Tiny seam over the platform-specific IPC connection types so the
/// wedge-detection helper can be unit-tested without spinning up a real
/// pipe/socket. Two impls live below — one for Unix `IpcConnection`, one
/// for the Windows client-side `IpcClientConnection`.
trait ConnRecv {
    async fn recv(&mut self) -> Result<Option<crate::protocol::Response>, crate::ipc::IpcError>;
    async fn recv_with_timeout(
        &mut self,
        timeout: std::time::Duration,
    ) -> Result<Option<crate::protocol::Response>, crate::ipc::IpcError>;
}

#[cfg(unix)]
impl ConnRecv for crate::ipc::IpcConnection {
    async fn recv(&mut self) -> Result<Option<crate::protocol::Response>, crate::ipc::IpcError> {
        crate::ipc::IpcConnection::recv_response(self).await
    }
    async fn recv_with_timeout(
        &mut self,
        timeout: std::time::Duration,
    ) -> Result<Option<crate::protocol::Response>, crate::ipc::IpcError> {
        crate::ipc::IpcConnection::recv_response_with_timeout(self, timeout).await
    }
}

#[cfg(windows)]
impl ConnRecv for crate::ipc::IpcClientConnection {
    async fn recv(&mut self) -> Result<Option<crate::protocol::Response>, crate::ipc::IpcError> {
        crate::ipc::IpcClientConnection::recv_response(self).await
    }
    async fn recv_with_timeout(
        &mut self,
        timeout: std::time::Duration,
    ) -> Result<Option<crate::protocol::Response>, crate::ipc::IpcError> {
        crate::ipc::IpcClientConnection::recv_response_with_timeout(self, timeout).await
    }
}

/// Ephemeral session: single-roundtrip compile (session start + compile +
/// session end in one IPC message). Used when `ZCCACHE_SESSION_ID` is not set.
pub(super) async fn cmd_compile_ephemeral(
    endpoint: &str,
    compiler: &Path,
    args: Vec<String>,
    cwd: NormalizedPath,
    client_env: Vec<(String, String)>,
) -> ExitCode {
    if let Err(e) = ensure_daemon(endpoint).await {
        eprintln!("zccache[err][D]: cannot start daemon at {endpoint}: {e}");
        return ExitCode::FAILURE;
    }
    let mut conn = match connect(endpoint).await {
        Ok(c) => c,
        Err(e) => {
            eprintln!("zccache[err][C]: cannot connect to daemon at {endpoint}: {e}");
            return ExitCode::FAILURE;
        }
    };

    let stdin_bytes = slurp_stdin_if_piped();
    let wire = crate::protocol::wire_prost::full_family_wire_format_from_env();
    let request = crate::protocol::Request::CompileEphemeral {
        client_pid: std::process::id(),
        working_dir: cwd.clone(),
        compiler: compiler.into(),
        args,
        cwd,
        env: Some(client_env),
        stdin: stdin_bytes,
    };
    if let Err(e) = conn.send_request(&request, wire).await {
        eprintln!("zccache[err][S]: failed to send to daemon: {e}");
        return ExitCode::FAILURE;
    }

    // Wedge detection only — no retry. CompileEphemeral is already on a
    // fresh-daemon path (ensure_daemon ran above); if the daemon wedges
    // mid-recv we surface a fast failure (~90 s instead of 300 s) so the
    // build framework's per-job retry budget isn't blown.
    match compile_recv_with_wedge_detection(&mut conn).await {
        CompileRecvOutcome::Done(recv_result) => {
            relay_compile_response(recv_result, &mut std::io::stdout(), &mut std::io::stderr())
        }
        CompileRecvOutcome::Wedged => {
            eprintln!(
                "zccache[err][W]: daemon at {endpoint} stopped responding within \
                 the wedge budget; killing it so the next compile starts fresh — issue #666"
            );
            drop(conn);
            stop_stale_daemon(endpoint).await;
            ExitCode::FAILURE
        }
        CompileRecvOutcome::Failed(msg) => {
            eprintln!("zccache[err][R]: {msg}");
            ExitCode::FAILURE
        }
    }
}

/// Ephemeral link/archive: single-roundtrip for `zccache ar ...` etc.
pub(super) async fn cmd_link_ephemeral(
    endpoint: &str,
    tool: &Path,
    args: Vec<String>,
    cwd: NormalizedPath,
    client_env: Vec<(String, String)>,
) -> ExitCode {
    if let Err(e) = ensure_daemon(endpoint).await {
        eprintln!("zccache[err][D]: cannot start daemon at {endpoint}: {e}");
        return ExitCode::FAILURE;
    }
    let mut conn = match connect(endpoint).await {
        Ok(c) => c,
        Err(e) => {
            eprintln!("zccache[err][C]: cannot connect to daemon at {endpoint}: {e}");
            return ExitCode::FAILURE;
        }
    };

    let wire = crate::protocol::wire_prost::full_family_wire_format_from_env();
    let request = crate::protocol::Request::LinkEphemeral {
        client_pid: std::process::id(),
        tool: tool.into(),
        args,
        cwd,
        env: Some(client_env),
    };
    if let Err(e) = conn.send_request(&request, wire).await {
        eprintln!("zccache[err][S]: failed to send to daemon: {e}");
        return ExitCode::FAILURE;
    }

    // Wedge detection only — see `cmd_compile_ephemeral` for the rationale.
    match compile_recv_with_wedge_detection(&mut conn).await {
        CompileRecvOutcome::Done(recv_result) => {
            relay_link_response(recv_result, &mut std::io::stdout(), &mut std::io::stderr())
        }
        CompileRecvOutcome::Wedged => {
            eprintln!(
                "zccache[err][W]: daemon at {endpoint} stopped responding within \
                 the wedge budget on a Link; killing it so the next request starts \
                 fresh — issue #666"
            );
            drop(conn);
            stop_stale_daemon(endpoint).await;
            ExitCode::FAILURE
        }
        CompileRecvOutcome::Failed(msg) => {
            eprintln!("zccache[err][R]: {msg}");
            ExitCode::FAILURE
        }
    }
}

fn relay_compile_response<W: Write, E: Write>(
    recv_result: Option<crate::protocol::Response>,
    stdout: &mut W,
    stderr: &mut E,
) -> ExitCode {
    match recv_result {
        Some(crate::protocol::Response::CompileResult {
            exit_code,
            stdout: out,
            stderr: err,
            ..
        }) => {
            let _ = stdout.write_all(&out);
            let _ = stderr.write_all(&err);
            exit_code_from_i32(exit_code)
        }
        Some(crate::protocol::Response::Error { message }) => {
            let _ = writeln!(stderr, "zccache[err][E]: daemon error: {message}");
            ExitCode::FAILURE
        }
        None => {
            let _ = writeln!(stderr, "{LOST_CONNECTION_MSG}");
            ExitCode::FAILURE
        }
        Some(other) => {
            let _ = writeln!(
                stderr,
                "zccache[err][U]: unexpected response from daemon: {other:?}"
            );
            ExitCode::FAILURE
        }
    }
}

fn relay_link_response<W: Write, E: Write>(
    recv_result: Option<crate::protocol::Response>,
    stdout: &mut W,
    stderr: &mut E,
) -> ExitCode {
    match recv_result {
        Some(crate::protocol::Response::LinkResult {
            exit_code,
            stdout: out,
            stderr: err,
            warning,
            ..
        }) => {
            let _ = stdout.write_all(&out);
            let _ = stderr.write_all(&err);
            if let Some(w) = warning {
                let _ = writeln!(stderr, "zccache warning: {w}");
            }
            exit_code_from_i32(exit_code)
        }
        Some(crate::protocol::Response::Error { message }) => {
            let _ = writeln!(stderr, "zccache[err][E]: daemon error: {message}");
            ExitCode::FAILURE
        }
        None => {
            let _ = writeln!(stderr, "{LOST_CONNECTION_MSG}");
            ExitCode::FAILURE
        }
        Some(other) => {
            let _ = writeln!(
                stderr,
                "zccache[err][U]: unexpected response from daemon: {other:?}"
            );
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn compile_response_relay_writes_stdout_stderr_and_exit_code() {
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        let exit = relay_compile_response(
            Some(crate::protocol::Response::CompileResult {
                exit_code: 7,
                stdout: Arc::new(b"compiler-out".to_vec()),
                stderr: Arc::new(b"compiler-err".to_vec()),
                cached: false,
            }),
            &mut stdout,
            &mut stderr,
        );

        assert_eq!(exit, ExitCode::from(7));
        assert_eq!(stdout, b"compiler-out");
        assert_eq!(stderr, b"compiler-err");
    }

    // ── Issue #666: wedge-detection helper ──────────────────────────────
    //
    // Verifies that `compile_recv_with_wedge_detection`:
    //   • returns `Done` on a normal response,
    //   • returns `Wedged` only when the underlying recv times out,
    //   • returns `Failed` (not `Wedged`) on a non-timeout transport error,
    //   • respects the disabled (`secs == 0`) configuration.

    struct FakeConn {
        behavior: FakeBehavior,
    }

    #[allow(clippy::large_enum_variant)]
    enum FakeBehavior {
        Ok(crate::protocol::Response),
        TimesOut,
        BrokenPipe,
    }

    impl ConnRecv for FakeConn {
        async fn recv(
            &mut self,
        ) -> Result<Option<crate::protocol::Response>, crate::ipc::IpcError> {
            match &self.behavior {
                FakeBehavior::Ok(r) => Ok(Some(r.clone())),
                FakeBehavior::TimesOut => {
                    // Sleep forever; the outer timeout wrapper handles it.
                    futures::future::pending::<()>().await;
                    unreachable!()
                }
                FakeBehavior::BrokenPipe => Err(crate::ipc::IpcError::ConnectionClosed),
            }
        }
        async fn recv_with_timeout(
            &mut self,
            timeout: std::time::Duration,
        ) -> Result<Option<crate::protocol::Response>, crate::ipc::IpcError> {
            match &self.behavior {
                FakeBehavior::Ok(r) => Ok(Some(r.clone())),
                FakeBehavior::TimesOut => {
                    tokio::time::sleep(timeout).await;
                    Err(crate::ipc::IpcError::Timeout(timeout))
                }
                FakeBehavior::BrokenPipe => Err(crate::ipc::IpcError::ConnectionClosed),
            }
        }
    }

    #[tokio::test]
    async fn wedge_detection_returns_done_on_normal_response() {
        // Use a short budget so the test stays snappy even if the fake regresses.
        std::env::set_var("ZCCACHE_WEDGE_RECV_TIMEOUT_SECS", "1");
        let mut conn = FakeConn {
            behavior: FakeBehavior::Ok(crate::protocol::Response::Pong),
        };
        let outcome = compile_recv_with_wedge_detection(&mut conn).await;
        std::env::remove_var("ZCCACHE_WEDGE_RECV_TIMEOUT_SECS");
        assert!(matches!(
            outcome,
            CompileRecvOutcome::Done(Some(crate::protocol::Response::Pong))
        ));
    }

    #[tokio::test]
    async fn wedge_detection_returns_wedged_on_recv_timeout() {
        // Force a 1 s budget so a wedged daemon surfaces within the test
        // window. Pre-#666 this path inherited the 300 s global default and
        // the whole build paid that wall × N workers.
        std::env::set_var("ZCCACHE_WEDGE_RECV_TIMEOUT_SECS", "1");
        let mut conn = FakeConn {
            behavior: FakeBehavior::TimesOut,
        };
        let started = std::time::Instant::now();
        let outcome = compile_recv_with_wedge_detection(&mut conn).await;
        let elapsed = started.elapsed();
        std::env::remove_var("ZCCACHE_WEDGE_RECV_TIMEOUT_SECS");
        assert!(matches!(outcome, CompileRecvOutcome::Wedged));
        assert!(
            elapsed < std::time::Duration::from_secs(3),
            "wedge detection took {elapsed:?} against a never-responding fake — \
             issue #666 expects bounded fail-fast at the configured budget"
        );
    }

    #[tokio::test]
    async fn wedge_detection_does_not_misclassify_broken_pipe_as_wedge() {
        // A non-timeout transport error must NOT trigger the recovery path
        // (force-killing the daemon on every protocol mismatch would be a
        // worse cure than the disease).
        std::env::set_var("ZCCACHE_WEDGE_RECV_TIMEOUT_SECS", "1");
        let mut conn = FakeConn {
            behavior: FakeBehavior::BrokenPipe,
        };
        let outcome = compile_recv_with_wedge_detection(&mut conn).await;
        std::env::remove_var("ZCCACHE_WEDGE_RECV_TIMEOUT_SECS");
        assert!(matches!(outcome, CompileRecvOutcome::Failed(_)));
    }

    #[tokio::test]
    async fn wedge_detection_disabled_when_env_is_zero() {
        // `ZCCACHE_WEDGE_RECV_TIMEOUT_SECS=0` opts back into the pre-#666
        // unbounded recv (useful for huge LTO links that legitimately
        // exceed the default budget). The fake's BrokenPipe path returns
        // immediately so the test doesn't hang.
        std::env::set_var("ZCCACHE_WEDGE_RECV_TIMEOUT_SECS", "0");
        let mut conn = FakeConn {
            behavior: FakeBehavior::BrokenPipe,
        };
        let outcome = compile_recv_with_wedge_detection(&mut conn).await;
        std::env::remove_var("ZCCACHE_WEDGE_RECV_TIMEOUT_SECS");
        // Disabled → falls through to `conn.recv()` unbounded, which the
        // fake reports as a broken pipe. Crucially: not classified as Wedged.
        assert!(matches!(outcome, CompileRecvOutcome::Failed(_)));
    }

    #[test]
    fn link_response_relay_preserves_warning_after_tool_stderr() {
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        let exit = relay_link_response(
            Some(crate::protocol::Response::LinkResult {
                exit_code: 0,
                stdout: Arc::new(b"link-out".to_vec()),
                stderr: Arc::new(b"link-err\n".to_vec()),
                cached: true,
                warning: Some("non-deterministic archive flags".to_string()),
            }),
            &mut stdout,
            &mut stderr,
        );

        assert_eq!(exit, ExitCode::SUCCESS);
        assert_eq!(stdout, b"link-out");
        assert_eq!(
            String::from_utf8(stderr).unwrap(),
            "link-err\nzccache warning: non-deterministic archive flags\n"
        );
    }
}
