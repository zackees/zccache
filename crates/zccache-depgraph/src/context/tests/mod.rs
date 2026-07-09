//! `#[cfg(test)]` modules for `context/`, split per surface so every file
//! stays under 1,000 LOC.
//!
//! See the directory's `README.md` for the layout overview.

use zccache_core::NormalizedPath;

use super::{CompileContext, RustcCompileContext};
use crate::search_paths::IncludeSearchPaths;

mod cc;
mod rustc;

/// Minimal C/C++ `CompileContext` with the given source, optional user-include
/// dirs and defines. Defines are sorted to match `from_parsed_args` invariant.
pub(super) fn make_context(source: &str, user_dirs: &[&str], defines: &[&str]) -> CompileContext {
    CompileContext {
        source_file: NormalizedPath::from(source),
        include_search: IncludeSearchPaths {
            user: user_dirs
                .iter()
                .map(|dir| NormalizedPath::from(*dir))
                .collect(),
            ..Default::default()
        },
        defines: {
            let mut d: Vec<String> = defines.iter().map(|s| s.to_string()).collect();
            d.sort();
            d
        },
        flags: Vec::new(),
        force_includes: Vec::new(),
        unknown_flags: Vec::new(),
    }
}

/// Minimal `RustcCompileContext` with the given source, fixed crate name
/// (`mylib`), `--crate-type lib`, single `link` emit, and everything else
/// defaulted. The shared shape lets per-test deltas show clearly.
pub(super) fn make_rustc_context(source: &str, edition: &str) -> RustcCompileContext {
    RustcCompileContext {
        source_file: NormalizedPath::from(source),
        crate_name: Some("mylib".to_string()),
        crate_types: vec!["lib".to_string()],
        edition: Some(edition.to_string()),
        emit_types: vec!["link".to_string()],
        cfgs: Vec::new(),
        check_cfgs: Vec::new(),
        codegen_flags: Vec::new(),
        cargo_metadata: None,
        extra_filename: None,
        target: None,
        cap_lints: None,
        extern_crates: Vec::new(),
        lint_flags: Vec::new(),
        unknown_flags: Vec::new(),
        remap_path_prefixes: Vec::new(),
        env_vars: Vec::new(),
        compiler_hash: None,
    }
}

/// Convenience wrapper around [`make_rustc_context`] that attaches a sorted
/// `env_vars` list — used by the `CARGO_*` filter regression tests.
pub(super) fn make_rustc_context_with_env(env: Vec<(String, String)>) -> RustcCompileContext {
    let mut ctx = make_rustc_context("/src/lib.rs", "2021");
    ctx.env_vars = env;
    ctx.env_vars.sort();
    ctx
}
