//! Linker, archiver, and post-link child-process execution.

use super::*;

pub(super) fn with_link_warning(result: Response, warning: Option<String>) -> Response {
    match (result, warning) {
        (
            Response::LinkResult {
                exit_code,
                stdout,
                stderr,
                cached,
                ..
            },
            warning @ Some(_),
        ) => Response::LinkResult {
            exit_code,
            stdout,
            stderr,
            cached,
            warning,
        },
        (result, _) => result,
    }
}

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

    let mut builder = kernal_api::async_process::AsyncProcessBuilder::new(tool);
    if let Some(ref response_file) = response_file {
        builder = builder.arg(response_file.at_arg());
    } else {
        builder = builder.args(args.iter().cloned());
    }
    builder = builder.current_dir(cwd);
    builder = apply_client_env_builder(builder, &env, lineage);

    let priority = CompilePriority::from_client_env(env.as_deref());
    match super::super::process::async_builder_output_with_priority(builder, priority).await {
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
    let builder = apply_client_env_builder(
        kernal_api::async_process::AsyncProcessBuilder::new(tool)
            .args(args.iter().cloned())
            .current_dir(cwd),
        &env,
        lineage,
    );
    let priority = CompilePriority::from_client_env(env.as_deref());
    match super::super::process::async_builder_output_with_priority_and_post_exit_grace(
        builder, priority, None,
    )
    .await
    {
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

    let mut builder = kernal_api::async_process::AsyncProcessBuilder::new(program)
        .args(extra_args)
        .arg(output_path.as_os_str());
    if let Some(parent) = output_path.parent() {
        builder = builder.current_dir(parent);
    }
    if let Some(vars) = env {
        builder = builder.clear_env(true);
        for (key, val) in vars {
            if client_env_var_is_safe_to_replay(key) {
                builder = builder.env(key, val);
            }
        }
    }
    builder = lineage.apply_to_async_builder(builder, env);

    tracing::debug!(
        program = %program,
        output = %output_path.display(),
        "running post-link deploy hook"
    );

    let priority = CompilePriority::from_client_env(env);
    match super::super::process::async_builder_output_with_priority(builder, priority).await {
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
