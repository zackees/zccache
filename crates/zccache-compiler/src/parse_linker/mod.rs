//! Linker detection and argument parsing for zccache.
//!
//! Handles parsing command-line arguments for `ld`, `lld`, MSVC `link.exe`,
//! and compiler drivers (`gcc`, `clang`) to determine cacheability for
//! linking (shared libraries, DLLs, and executables).
//!
//! The implementation is split across focused submodules:
//!
//! - `types` (private) — public types ([`LinkerFamily`],
//!   [`ParsedLinkerInvocation`], [`CacheableLink`]).
//! - `detect` (private) — tool-name detection and [`is_link_invocation`].
//! - `gnu_ld` (private) — GNU `ld` / LLVM `lld` argument parser.
//! - `msvc_link` (private) — MSVC `link.exe` argument parser.
//! - `compiler_driver` (private) — `gcc` / `clang` as-linker parser.
//!
//! Public surface (re-exported here): [`LinkerFamily`],
//! [`ParsedLinkerInvocation`], [`CacheableLink`], [`is_linker`],
//! [`is_link_invocation`], [`parse_linker_invocation`].

mod compiler_driver;
pub(crate) mod detect;
mod dsymutil;
mod gnu_ld;
mod msvc_link;
mod types;

#[cfg(test)]
mod tests;

pub use detect::{is_link_invocation, is_linker};
pub use types::{CacheableLink, LinkOutputKind, LinkerFamily, ParsedLinkerInvocation};

use compiler_driver::parse_compiler_driver_link;
use detect::{detect_family, is_compiler_driver};
use dsymutil::parse_dsymutil;
use gnu_ld::parse_gnu_ld;
use msvc_link::parse_msvc_link;

/// Parse a linker invocation's arguments to determine cacheability.
///
/// Handles direct linkers (ld, lld, link.exe) and compiler drivers used
/// for linking (gcc, clang). Both shared library and executable linking
/// are cacheable.
#[must_use]
pub fn parse_linker_invocation(tool: &str, args: Vec<String>) -> ParsedLinkerInvocation {
    // Try direct linker first
    if let Some(family) = detect_family(tool) {
        return match family {
            LinkerFamily::MsvcLink => parse_msvc_link(tool, args),
            LinkerFamily::Ld | LinkerFamily::Lld => parse_gnu_ld(tool, family, args),
            LinkerFamily::CompilerDriver => parse_compiler_driver_link(tool, args),
            LinkerFamily::Dsymutil => parse_dsymutil(tool, args),
        };
    }

    // Try compiler driver (gcc -shared, clang -shared, etc.)
    if is_compiler_driver(tool) {
        return parse_compiler_driver_link(tool, args);
    }

    ParsedLinkerInvocation::NonCacheable {
        reason: format!("not a recognized linker: {tool}"),
    }
}
