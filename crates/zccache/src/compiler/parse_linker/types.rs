//! Public types for the linker parser.

use crate::core::NormalizedPath;

/// Supported linker tool families.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkerFamily {
    /// GNU ld (ld, ld.bfd, ld.gold, x86_64-linux-gnu-ld, etc.)
    Ld,
    /// LLVM lld (lld, ld.lld, lld-link, etc.)
    Lld,
    /// MSVC link.exe
    MsvcLink,
    /// Compiler driver used as linker (gcc, clang, etc.)
    CompilerDriver,
}

/// The result of parsing a linker invocation.
#[derive(Debug, Clone)]
pub enum ParsedLinkerInvocation {
    /// A cacheable link (shared library, DLL, or executable).
    Cacheable(CacheableLink),
    /// A non-cacheable invocation.
    NonCacheable {
        /// Reason why this invocation is not cacheable.
        reason: String,
    },
}

/// A cacheable link invocation (shared library, DLL, or executable).
#[derive(Debug, Clone)]
pub struct CacheableLink {
    /// The linker executable path.
    pub tool: NormalizedPath,
    /// The detected linker family.
    pub family: LinkerFamily,
    /// Input object files and libraries (order preserved — matters for linker).
    pub input_files: Vec<NormalizedPath>,
    /// The output file path (shared library, DLL, or executable).
    pub output_file: NormalizedPath,
    /// Secondary output files produced alongside the primary output.
    /// E.g., MSVC `/IMPLIB:foo.lib` produces `foo.lib` + `foo.exp`.
    /// May not all exist after linking — the server should skip missing ones.
    pub secondary_outputs: Vec<NormalizedPath>,
    /// Flags relevant to cache keying (optimization, target, etc.).
    pub cache_relevant_flags: Vec<String>,
    /// The full original argument list (for fallback execution).
    pub original_args: Vec<String>,
    /// Whether non-deterministic output is detected.
    pub non_deterministic: bool,
}
