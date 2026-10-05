//! Shared per-unit `CompileContext` key for multi-source C/C++ compiles.
//!
//! This module owns the ONE base context that `handle_compile_multi` clones
//! into every compilation unit of a batch. Because every unit inherits it, a
//! missing key input here mis-keys the whole batch, not a single unit.
//!
//! The flags appended below MUST stay in lockstep with the single-source
//! builder in `super::rustc` (`build_cc_compile_context`): the compiler-env
//! salts from `keys::cc_env_key_flags` (issue #1806) and the MSVC
//! `/showIncludes` salt from `keys::msvc_show_includes_key_flags`
//! (issue #1530) are part of the cache key, so a multi-source compile has to
//! carry them too. Omitting them let a batch keyed identically under a
//! different `CPATH`, locale, or `/showIncludes` spelling reuse headers
//! resolved in a different environment — and handed a CMake + Ninja caller an
//! incomplete depfile (issue #1901).

use super::*;

/// Build the ONE shared `CompileContext` cloned into every unit of a
/// multi-source C/C++ compile.
///
/// Mirrors the single-source builder `super::rustc::build_cc_compile_context`:
/// the compiler-env salts from `keys::cc_env_key_flags` and the MSVC
/// `/showIncludes` salt from `keys::msvc_show_includes_key_flags` are part of
/// the cache key, so a multi-source compile must carry them too. Without them
/// every unit of the batch keys identically under a different `CPATH`, locale,
/// or `/showIncludes` spelling (issue #1901).
pub(in crate::daemon::server) fn build_multi_base_context(
    family: crate::compiler::CompilerFamily,
    original_args: &[String],
    cwd: &NormalizedPath,
    compiler_hash: ContentHash,
    system_includes: &[NormalizedPath],
    client_env: &[(String, String)],
    dependency_mode: DependencyDiscoveryMode,
) -> (Arc<CompileContext>, UserDepFlags) {
    let parsed = if family == crate::compiler::CompilerFamily::Msvc
        || crate::compiler::parse_msvc::looks_like_msvc_args(original_args)
    {
        crate::depgraph::msvc_args::parse_msvc_args(original_args, cwd)
    } else {
        crate::depgraph::args::parse_gnu_args(original_args, cwd)
    };
    let dep_flags = parsed.dep_flags.clone();
    let mut base = CompileContext::from_parsed_args(parsed, compiler_hash);
    base.flags.extend(cc_env_key_flags(family, client_env));
    // Issue #1530: a caller-passed `/showIncludes` changes what the stored
    // stdout must contain, but the parser drops the flag, so without this the
    // two shapes would share one entry.
    base.flags.extend(super::keys::msvc_show_includes_key_flags(
        family,
        original_args,
    ));
    // `CompileContext` hashing consumes the flag list, so the order the salts
    // land in must not change the key; the single-source path sorts too.
    base.flags.sort();

    for path in system_includes {
        if !base.include_search.system.contains(path) {
            base.include_search.system.push(path.clone());
        }
    }
    dependency_mode.apply_to_cc_context(&mut base, &dep_flags);
    (Arc::new(base), dep_flags)
}
