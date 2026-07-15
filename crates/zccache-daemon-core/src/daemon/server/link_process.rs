//! Linker, archiver, and post-link child-process execution.

use super::*;

/// Run a tool directly (passthrough) and return a LinkResult response.
///
/// `tmp_dir` is where the synthesized Windows response file lands when the
/// command line exceeds the OS limit. Production callers pass the daemon's
/// `state.depfile_tmpdir` (under the cache root) so the contents are
/// covered by the wrapper's Defender exclusion - see issue #275.
pub(super) async fn run_tool_passthrough(
    tool: &Path,
    args: &[String],
    cwd: &Path,
    env: Option<Vec<(String, String)>>,
    lineage: &super::super::lineage::Lineage,
    tmp_dir: &Path,
) -> Response {
    let family_hint = crate::compiler::detect_family(&tool.to_string_lossy());
    let response_file = match crate::compiler::response_file::write_response_file_if_needed(
        args,
        tmp_dir,
        family_hint,
    ) {
        Ok(guard) => guard,
        Err(error) => {
            return Response::Error {
                message: format!(
                    "failed to write response file for {}: {error}",
                    tool.display()
                ),
            };
        }
    };

    let mut cmd = tokio::process::Command::new(tool);
    if let Some(ref response_file) = response_file {
        cmd.arg(response_file.at_arg());
    } else {
        cmd.args(args);
    }
    cmd.current_dir(cwd);
    apply_client_env(&mut cmd, &env, lineage);

    let priority = CompilePriority::from_client_env(env.as_deref());
    match super::super::process::tokio_command_output_with_priority(&mut cmd, priority).await {
        Ok(output) => Response::LinkResult {
            exit_code: output.status.code().unwrap_or(1),
            stdout: Arc::new(output.stdout),
            stderr: Arc::new(output.stderr),
            cached: false,
            warning: None,
        },
        Err(error) => Response::Error {
            message: format!("failed to run {}: {error}", tool.display()),
        },
    }
}

/// Run a parsed pure archiver without the general compiler-child watchdog.
///
/// Archivers are leaf processes: they only read declared inputs and write the
/// requested archive. Waiting on the Tokio child directly avoids watchdog and
/// blocking-pool setup costs that are material for a 10-20 ms `ar` run.
pub(super) async fn run_archive_tool_passthrough(
    tool: &Path,
    args: &[String],
    cwd: &Path,
    env: Option<Vec<(String, String)>>,
    lineage: &super::super::lineage::Lineage,
) -> Response {
    let mut cmd = tokio::process::Command::new(tool);
    cmd.args(args);
    cmd.current_dir(cwd);
    apply_client_env(&mut cmd, &env, lineage);
    let priority = CompilePriority::from_client_env(env.as_deref());
    match super::super::process::tokio_leaf_command_output_with_priority(&mut cmd, priority).await {
        Ok(output) => Response::LinkResult {
            exit_code: output.status.code().unwrap_or(1),
            stdout: Arc::new(output.stdout),
            stderr: Arc::new(output.stderr),
            cached: false,
            warning: None,
        },
        Err(error) => Response::Error {
            message: format!("failed to run {}: {error}", tool.display()),
        },
    }
}

/// Run an optional post-link deploy command on successful link output.
pub(super) async fn run_post_link_deploy_hook(
    cmd_str: &str,
    output_path: &Path,
    env: Option<&[(String, String)]>,
    lineage: &super::super::lineage::Lineage,
) {
    let mut parts = cmd_str.split_whitespace();
    let program = match parts.next() {
        Some(program) => program,
        None => {
            tracing::warn!("ZCCACHE_LINK_DEPLOY_CMD is empty - skipping deploy hook");
            return;
        }
    };
    let extra_args: Vec<&str> = parts.collect();

    let mut cmd = tokio::process::Command::new(program);
    cmd.args(&extra_args);
    cmd.arg(output_path);
    if let Some(parent) = output_path.parent() {
        cmd.current_dir(parent);
    }
    if let Some(vars) = env {
        cmd.env_clear();
        for (key, val) in vars {
            if client_env_var_is_safe_to_replay(key) {
                cmd.env(key, val);
            }
        }
    }
    lineage.apply_to_tokio(&mut cmd, env);

    tracing::debug!(
        program = %program,
        output = %output_path.display(),
        "running post-link deploy hook"
    );

    let priority = CompilePriority::from_client_env(env);
    match super::super::process::tokio_command_output_with_priority(&mut cmd, priority).await {
        Ok(out) if out.status.success() => {
            tracing::debug!(program = %program, "post-link deploy hook succeeded");
        }
        Ok(out) => {
            tracing::warn!(
                program = %program,
                exit_code = out.status.code().unwrap_or(-1),
                stderr = %String::from_utf8_lossy(&out.stderr),
                "post-link deploy hook exited non-zero"
            );
        }
        Err(error) => {
            tracing::warn!(
                program = %program,
                %error,
                "post-link deploy hook failed to start"
            );
        }
    }
}
