//! Direct execution paths used when wrapper caching is disabled or unsupported.

use std::path::Path;
use std::process::ExitCode;

use super::super::util::exit_code_from_i32;
use super::tool_resolution::resolve_compiler_path;

/// Run the compiler/tool directly without caching (`ZCCACHE_DISABLE` mode).
pub(super) fn run_passthrough(args: &[String]) -> ExitCode {
    let tool = &args[0];
    let tool_args = args.get(1..).unwrap_or(&[]);
    let resolved = resolve_compiler_path(tool);

    match std::process::Command::new(&resolved)
        .args(tool_args)
        .status()
    {
        Ok(status) => exit_code_from_i32(status.code().unwrap_or(1)),
        Err(e) => {
            eprintln!("zccache: failed to run {}: {e}", resolved.display());
            ExitCode::FAILURE
        }
    }
}

/// Run a tool directly and return its exit code.
pub(super) fn run_tool_direct(tool: &Path, args: &[String]) -> ExitCode {
    match std::process::Command::new(tool).args(args).status() {
        Ok(status) => exit_code_from_i32(status.code().unwrap_or(1)),
        Err(e) => {
            eprintln!("zccache: failed to run {}: {e}", tool.display());
            ExitCode::FAILURE
        }
    }
}
