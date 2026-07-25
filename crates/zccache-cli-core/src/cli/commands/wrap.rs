//! `zccache <compiler> ...` and `zccache wrap` compiler/linker/archiver wrapping.
//!
//! The facade owns only the wrapper flow. Routing, environment policy, tool
//! resolution, rustfmt caching, and IPC request/response handling live in
//! focused submodules so soldr-facing wrapper changes do not touch every layer.

mod diag;
mod env;
mod fallback;
mod ipc;
mod passthrough;
mod routing;
mod rustfmt;
mod tool_resolution;

use crate::compiler::strict_paths::StrictPathsMode;
use std::io::{IsTerminal, Write};
use std::process::ExitCode;

use super::util::{resolve_endpoint, run_async};

pub(crate) use env::{parse_wrapper_overrides, strip_leading_wrapper_flags, WrapperOverrides};
use routing::WrapperRoute;

const ANSI_YELLOW: &[u8] = b"\x1b[33m";
const ANSI_RESET: &[u8] = b"\x1b[0m";

fn wrapper_stderr_color_enabled() -> bool {
    std::io::stderr().is_terminal() && std::env::var_os("NO_COLOR").is_none()
}

fn write_wrapper_warning_line(
    writer: &mut dyn Write,
    line: &[u8],
    color: bool,
) -> std::io::Result<()> {
    let (body, newline) = line.strip_suffix(b"\r\n").map_or_else(
        || {
            line.strip_suffix(b"\n")
                .map_or((line, &b""[..]), |body| (body, &b"\n"[..]))
        },
        |body| (body, &b"\r\n"[..]),
    );

    if color {
        writer.write_all(ANSI_YELLOW)?;
    }
    writer.write_all(body)?;
    if color {
        writer.write_all(ANSI_RESET)?;
    }
    writer.write_all(newline)
}

/// Emit a single wrapper warning line to stderr, yellow when the terminal
/// supports it (issue #1211: cache/daemon degradations are never silent).
fn emit_wrapper_warning(text: &str) {
    let line = format!("{text}\n");
    let _ = write_wrapper_warning_line(
        &mut std::io::stderr(),
        line.as_bytes(),
        wrapper_stderr_color_enabled(),
    );
}

/// Wrap a compiler or tool invocation.
///
/// `args` is the full command: ["clang++", "-c", "foo.cpp", "-o", "foo.o"]
/// or ["ar", "rcs", "libfoo.a", "a.o", "b.o"].
///
/// If `ZCCACHE_SESSION_ID` is set, uses that session and sends the tool as a
/// per-request override. If unset, auto-creates an ephemeral session.
pub(crate) fn run_wrap(args: &[String], overrides: WrapperOverrides) -> ExitCode {
    diag::emit(args);

    if args.is_empty() {
        eprintln!("usage: zccache <compiler|tool> <args...>");
        return ExitCode::FAILURE;
    }

    if env::wrapper_disabled() {
        // Never silent (issue #1211): the user opted out, but each uncached
        // invocation still announces itself and the reason.
        return passthrough::run_passthrough(args, Some("ZCCACHE_DISABLE is set"));
    }

    let strict_paths_mode = match env::effective_strict_paths_mode(overrides) {
        Ok(mode) => mode,
        Err(err) => {
            eprintln!("zccache: {err}");
            return ExitCode::FAILURE;
        }
    };

    let wrapped_tool = tool_resolution::resolve_compiler_path(&args[0]);
    let tool_args: Vec<String> = args.get(1..).unwrap_or(&[]).to_vec();
    let cwd = std::env::current_dir().unwrap_or_default();
    let client_env = env::client_env(overrides);
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
        // Silent by design: probe callers parse the tool's stderr, so a
        // warning line here would corrupt the probe (see run_passthrough).
        WrapperRoute::ProbeBypass => passthrough::run_passthrough(args, None),
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

pub(crate) fn run_embedded_rustfmt_with_runner<F>(
    rustfmt_path: &std::path::Path,
    args: &[String],
    cwd: &std::path::Path,
    cache_root: &std::path::Path,
    runner: F,
) -> std::io::Result<i32>
where
    F: FnOnce(&mut std::process::Command) -> std::io::Result<i32>,
{
    rustfmt::run_rustfmt_cached_with_runner(rustfmt_path, args, cwd, Some(cache_root), runner)
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
