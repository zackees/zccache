//! Wrapper IPC request construction and response relay.

use crate::core::NormalizedPath;
use std::io::Write;
use std::path::Path;
use std::process::ExitCode;

use super::super::daemon::ensure_daemon;
use super::super::util::{connect, exit_code_from_i32, slurp_stdin_if_piped};

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

    if let Err(e) = conn
        .send(&crate::protocol::Request::Compile {
            session_id: session_id.to_string(),
            args,
            cwd,
            compiler,
            env: Some(client_env),
            stdin: stdin_bytes,
        })
        .await
    {
        eprintln!("zccache[err][S]: failed to send to daemon: {e}");
        return ExitCode::FAILURE;
    }

    let recv_result = match conn.recv().await {
        Ok(r) => r,
        Err(e) => {
            eprintln!("zccache[err][R]: broken connection to daemon: {e}");
            return ExitCode::FAILURE;
        }
    };
    relay_compile_response(recv_result, &mut std::io::stdout(), &mut std::io::stderr())
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
    if let Err(e) = conn
        .send(&crate::protocol::Request::CompileEphemeral {
            client_pid: std::process::id(),
            working_dir: cwd.clone(),
            compiler: compiler.into(),
            args,
            cwd,
            env: Some(client_env),
            stdin: stdin_bytes,
        })
        .await
    {
        eprintln!("zccache[err][S]: failed to send to daemon: {e}");
        return ExitCode::FAILURE;
    }

    let recv_result = match conn.recv().await {
        Ok(r) => r,
        Err(e) => {
            eprintln!("zccache[err][R]: broken connection to daemon: {e}");
            return ExitCode::FAILURE;
        }
    };
    relay_compile_response(recv_result, &mut std::io::stdout(), &mut std::io::stderr())
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

    if let Err(e) = conn
        .send(&crate::protocol::Request::LinkEphemeral {
            client_pid: std::process::id(),
            tool: tool.into(),
            args,
            cwd,
            env: Some(client_env),
        })
        .await
    {
        eprintln!("zccache[err][S]: failed to send to daemon: {e}");
        return ExitCode::FAILURE;
    }

    let recv_result = match conn.recv().await {
        Ok(r) => r,
        Err(e) => {
            eprintln!("zccache[err][R]: broken connection to daemon: {e}");
            return ExitCode::FAILURE;
        }
    };
    relay_link_response(recv_result, &mut std::io::stdout(), &mut std::io::stderr())
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
            let _ = writeln!(
                stderr,
                "zccache[err][R]: lost connection to daemon (no response). Often a daemon-CLI protocol version mismatch - try `zccache stop`"
            );
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
            let _ = writeln!(
                stderr,
                "zccache[err][R]: lost connection to daemon (no response). Often a daemon-CLI protocol version mismatch - try `zccache stop`"
            );
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
