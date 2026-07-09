//! Compiler detection and argument parsing for zccache.
//!
//! Handles identifying compilers, parsing their command-line arguments
//! to determine cacheability, and extracting cache-relevant information.
//!
//! The implementation is split across focused submodules:
//!
//! - `detect` (private) — `detect_family` + extension-classification helpers
//! - `parse` (private) — Clang/GCC/MSVC dispatch entry point ([`parse_invocation`])
//! - `parse_rustc` (private) — Rustc invocation parser (different model: crate types, --emit, etc.)
//! - [`parse_msvc`] — MSVC / clang-cl argument parser
//! - [`parse_archiver`], [`parse_linker`], [`parse_rustfmt`] — sibling tool parsers
//! - [`response_file`], [`strict_paths`], [`arduino`] — utility modules
//!
//! Public surface (re-exported from this module): [`CompilerFamily`],
//! [`ParsedInvocation`], [`CacheableCompilation`], [`detect_family`],
//! [`parse_invocation`].

#![allow(clippy::missing_errors_doc)]

pub mod arduino;
mod detect;
mod parse;
pub mod parse_archiver;
pub mod parse_linker;
pub mod parse_msvc;
mod parse_rustc;
pub mod parse_rustfmt;
pub mod response_file;
pub mod strict_paths;

#[cfg(test)]
mod tests;

use std::sync::Arc;
use zccache_core::NormalizedPath;

pub use detect::detect_family;
pub use parse::parse_invocation;

/// Supported compiler families.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompilerFamily {
    /// GCC (gcc, g++)
    Gcc,
    /// Clang (clang, clang++)
    Clang,
    /// MSVC (cl.exe)
    Msvc,
    /// Rust compiler (rustc)
    Rustc,
    /// Rust formatter (rustfmt) — not a compiler, but cacheable as a tool.
    Rustfmt,
}

impl CompilerFamily {
    /// Whether this compiler supports `-MD -MF` for depfile generation.
    /// MSVC uses `/showIncludes` instead. Rustc uses `--emit=dep-info`.
    #[must_use]
    pub fn supports_depfile(&self) -> bool {
        matches!(self, CompilerFamily::Gcc | CompilerFamily::Clang)
    }

    /// Default PCH output extension (without dot) for this compiler family.
    /// Returns `None` for MSVC (uses /Yc + /Fp mechanism instead), Rustc, and Rustfmt.
    #[must_use]
    pub fn pch_extension(&self) -> Option<&'static str> {
        match self {
            CompilerFamily::Gcc => Some("gch"),
            CompilerFamily::Clang => Some("pch"),
            CompilerFamily::Msvc | CompilerFamily::Rustc | CompilerFamily::Rustfmt => None,
        }
    }

    /// Whether this is a formatter tool (not a compiler).
    #[must_use]
    pub fn is_formatter(&self) -> bool {
        matches!(self, CompilerFamily::Rustfmt)
    }

    /// Whether the daemon should probe this compiler for system include
    /// paths on first use (via `depgraph::discovery_args()`).
    ///
    /// True for C/C++ family compilers (Gcc, Clang, Msvc) — the discovery
    /// args (`-v -E -x c++ NUL`) are C/C++-preprocessor flags. False for
    /// the rust toolchain (Rustc, Rustfmt) because (a) rust has no concept
    /// of system include paths and (b) rustc rejects the C/C++ flags and
    /// the spawn cost is wasted ~30-50 ms on every first-after-clear
    /// invocation. See issue #517.
    #[must_use]
    pub fn needs_system_include_discovery(&self) -> bool {
        matches!(
            self,
            CompilerFamily::Gcc | CompilerFamily::Clang | CompilerFamily::Msvc,
        )
    }
}

/// The result of parsing a compiler invocation.
#[derive(Debug, Clone)]
pub enum ParsedInvocation {
    /// A cacheable compilation (single source to single object).
    Cacheable(CacheableCompilation),
    /// Multiple source files with `-c` — each is independently cacheable.
    MultiFile {
        /// One entry per source file, each with its own output path.
        compilations: Vec<CacheableCompilation>,
        /// The original full argument list (for batched compiler invocation of misses).
        original_args: Arc<[String]>,
        /// Indices of source files in `original_args`, so the daemon can filter
        /// out cache-hit sources without reconstructing args.
        source_indices: Vec<usize>,
    },
    /// A non-cacheable invocation (linking, preprocessing, etc.).
    NonCacheable {
        /// Reason why this invocation is not cacheable.
        reason: String,
    },
}

/// A cacheable compilation invocation.
#[derive(Debug, Clone)]
pub struct CacheableCompilation {
    /// The compiler executable path.
    pub compiler: NormalizedPath,
    /// The detected compiler family.
    pub family: CompilerFamily,
    /// The source file being compiled.
    pub source_file: NormalizedPath,
    /// The output file path.
    pub output_file: NormalizedPath,
    /// The full original argument list — always passed to the compiler as-is.
    pub original_args: Arc<[String]>,
    /// Flags not recognized by the parser but still part of the invocation.
    /// Preserved for completeness and consistency with the linker/archiver/
    /// depgraph parsers which all track unknown flags.
    pub unknown_flags: Vec<String>,
}

/// The language mode for a source file, as determined by `-x <lang>` or file extension.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceMode {
    /// Normal C/C++ source (`.c`, `.cpp`, etc.) — compiles to `.o`.
    Normal,
    /// PCH header (`-x c-header` / `-x c++-header`) — compiles to `.pch`/`.gch`.
    Header,
    /// Header unit (`-x c-header-unit` / `-x c++-header-unit`) — compiles to `.pcm`.
    HeaderUnit,
    /// Module interface (`-x c++-module` or `.cppm`/`.ixx`) — `.pcm` with `--precompile`, `.o` with `-c`.
    Module,
}

impl SourceMode {
    /// Whether this mode implies compilation without an explicit `-c` or `--precompile` flag.
    /// Header and header-unit modes imply compilation (like PCH generation).
    /// Module mode does NOT — it requires `-c` or `--precompile`.
    pub(crate) fn implies_compilation(self) -> bool {
        matches!(self, SourceMode::Header | SourceMode::HeaderUnit)
    }
}
