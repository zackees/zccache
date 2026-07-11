//! `zccache <compiler> ...` and `zccache wrap` compiler/linker/archiver wrapping.
//!
//! The facade owns only the wrapper flow. Routing, environment policy, tool
//! resolution, rustfmt caching, and IPC request/response handling live in
//! focused submodules so soldr-facing wrapper changes do not touch every layer.

mod diag;
mod env;
mod ipc;
mod passthrough;
mod routing;
mod rustfmt;
mod tool_resolution;

use crate::compiler::strict_paths::StrictPathsMode;
use std::process::ExitCode;

use super::util::{resolve_endpoint, run_async};

pub(crate) use env::{parse_optional_strict_paths, strip_leading_strict_paths_flags};
use routing::WrapperRoute;

/// Wrap a compiler or tool invocation.
///
/// `args` is the full command: ["clang++", "-c", "foo.cpp", "-o", "foo.o"]
/// or ["ar", "rcs", "libfoo.a", "a.o", "b.o"].
///
/// If `ZCCACHE_SESSION_ID` is set, uses that session and sends the tool as a
/// per-request override. If unset, auto-creates an ephemeral session.
pub(crate) fn run_wrap(
    args: &[String],
    strict_paths_override: Option<StrictPathsMode>,
) -> ExitCode {
    diag::emit(args);

    if args.is_empty() {
        eprintln!("usage: zccache <compiler|tool> <args...>");
        return ExitCode::FAILURE;
    }

    if env::wrapper_disabled() {
        return passthrough::run_passthrough(args);
    }

    let strict_paths_mode = match env::effective_strict_paths_mode(strict_paths_override) {
        Ok(mode) => mode,
        Err(err) => {
            eprintln!("zccache: {err}");
            return ExitCode::FAILURE;
        }
    };

    let wrapped_tool = tool_resolution::resolve_compiler_path(&args[0]);
    let tool_args: Vec<String> = args.get(1..).unwrap_or(&[]).to_vec();
    let cwd = std::env::current_dir().unwrap_or_default();
    let client_env = env::client_env(strict_paths_override);
    let endpoint = resolve_endpoint(None);

    // Release the CWD handle on the build directory. On Windows, a process's
    // CWD holds an implicit kernel handle that prevents the directory from
    // being deleted. We've captured everything we need into local variables.
    let _ = std::env::set_current_dir(std::env::temp_dir());

    match routing::classify_invocation(&args[0], &tool_args) {
        WrapperRoute::Formatter => {
            rustfmt::run_rustfmt_cached(&wrapped_tool, &tool_args, &cwd, None)
        }
        WrapperRoute::LinkOrArchive => run_async(ipc::cmd_link_ephemeral(
            &endpoint,
            &wrapped_tool,
            tool_args,
            cwd.into(),
            client_env,
        )),
        WrapperRoute::ProbeBypass => passthrough::run_passthrough(args),
        WrapperRoute::Compile => run_compile_route(
            &endpoint,
            &args[0],
            &tool_args,
            strict_paths_mode,
            wrapped_tool,
            cwd.into(),
            client_env,
        ),
    }
}

pub(crate) fn run_embedded_rustfmt(
    rustfmt_path: &std::path::Path,
    args: &[String],
    cwd: &std::path::Path,
    cache_root: &std::path::Path,
) -> ExitCode {
    rustfmt::run_rustfmt_cached(rustfmt_path, args, cwd, Some(cache_root))
}

fn run_compile_route(
    endpoint: &str,
    raw_tool: &str,
    tool_args: &[String],
    strict_paths_mode: StrictPathsMode,
    wrapped_tool: crate::core::NormalizedPath,
    cwd: crate::core::NormalizedPath,
    client_env: Vec<(String, String)>,
) -> ExitCode {
    if let Err(err) = crate::compiler::strict_paths::validate_args(tool_args, strict_paths_mode) {
        eprintln!("{}", err.diagnostic(raw_tool, tool_args));
        return ExitCode::FAILURE;
    }

    match std::env::var("ZCCACHE_SESSION_ID") {
        Ok(session_id) => {
            if session_id.is_empty() {
                eprintln!("ZCCACHE_SESSION_ID is empty");
                return ExitCode::FAILURE;
            }
            run_async(ipc::cmd_compile(
                endpoint,
                &session_id,
                tool_args.to_vec(),
                cwd,
                wrapped_tool,
                client_env,
            ))
        }
        Err(_) => run_async(ipc::cmd_compile_ephemeral(
            endpoint,
            &wrapped_tool,
            tool_args.to_vec(),
            cwd,
            client_env,
        )),
    }
}
