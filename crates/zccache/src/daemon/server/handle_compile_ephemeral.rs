//! Single-roundtrip ephemeral compile dispatch + direct (uncached) compiler
//! invocation.
//!
//! `handle_compile_ephemeral` is the wrapper-mode fast path: it inlines
//! session start, compile, and session end into one IPC roundtrip.
//!
//! `run_compiler_direct` is the bypass for non-cacheable invocations — it
//! shells out to the compiler with no caching, while still tracking lineage
//! and replaying the client's env.

use super::*;

/// Handle a single-roundtrip ephemeral compile: session start + compile + session end.
/// Avoids 3 IPC roundtrips for drop-in wrapper mode.
#[allow(clippy::too_many_arguments)] // Single dispatch hop; ergonomic refactor unblocked once we stop adding new client-side fields.
pub(super) async fn handle_compile_ephemeral(
    state: &Arc<SharedState>,
    client_pid: u32,
    working_dir: &Path,
    compiler: &Path,
    args: &[String],
    cwd: &Path,
    env: Option<Vec<(String, String)>>,
    stdin: Vec<u8>,
) -> Response {
    // 1. Start ephemeral session (inline, no IPC roundtrip)
    state.stats.record_session();
    let session_resp = handle_session_start(
        state,
        SessionStartArgs {
            client_pid,
            working_dir,
            log_file: None,
            track_stats: false,
            journal_path: None,
            profile: false,
            private_daemon: None,
        },
    )
    .await;
    let session_id = match session_resp {
        Response::SessionStarted { session_id, .. } => session_id,
        Response::Error { message } => return Response::Error { message },
        other => {
            return Response::Error {
                message: format!("unexpected session start response: {other:?}"),
            };
        }
    };

    // 2. Compile — pass the compiler from the ephemeral request
    let result = handle_compile(state, &session_id, args, cwd, compiler, env, stdin).await;

    // 3. End session (best-effort, no response needed)
    if let Ok(sid) = session_id.parse::<SessionId>() {
        state.session_worktree_roots.remove(&sid);
        state.sessions.end(&sid);
    }

    result
}

/// Run the compiler directly without caching.
///
/// `tmp_dir` is where the synthesized Windows response file lands when the
/// command line exceeds the OS limit. Production callers pass the daemon's
/// `state.depfile_tmpdir` (under the cache root) so the contents are
/// covered by the wrapper's Defender exclusion — see issue #275.
#[allow(clippy::too_many_arguments)] // Mirrors handle_compile's surface — refactor parked.
pub(super) async fn run_compiler_direct(
    compiler: &NormalizedPath,
    args: &[String],
    cwd: &Path,
    sessions: &SessionManager,
    sid: &SessionId,
    client_env: &Option<Vec<(String, String)>>,
    stdin_bytes: &[u8],
    tmp_dir: &Path,
) -> Response {
    let _rsp_guard =
        match crate::compiler::response_file::write_response_file_if_needed(args, tmp_dir) {
            Ok(guard) => guard,
            Err(e) => {
                return Response::Error {
                    message: format!("failed to write response file: {e}"),
                };
            }
        };

    let lineage = super::super::lineage::Lineage::current(
        sessions.get(sid).map(|s| s.client_pid),
        Some(sid.to_string()),
    );
    let mut cmd = tokio::process::Command::new(compiler);
    if let Some(ref rsp) = _rsp_guard {
        cmd.arg(rsp.at_arg()).current_dir(cwd);
    } else {
        cmd.args(args).current_dir(cwd);
    }
    apply_client_env(&mut cmd, client_env, &lineage);
    let compiler_priority = CompilePriority::from_client_env(client_env.as_deref());
    let result = super::super::process::tokio_command_output_with_priority_stdin(
        &mut cmd,
        compiler_priority,
        if stdin_bytes.is_empty() {
            None
        } else {
            Some(stdin_bytes)
        },
    )
    .await;

    match result {
        Ok(output) => {
            let exit_code = output.status.code().unwrap_or(-1);
            write_session_log(sessions, sid, &format!("[DIRECT] exit_code={exit_code}"));
            Response::CompileResult {
                exit_code,
                stdout: Arc::new(output.stdout),
                stderr: Arc::new(output.stderr),
                cached: false,
            }
        }
        Err(e) => Response::Error {
            message: format!("failed to run compiler: {e}"),
        },
    }
}
