//! Tool-name detection helpers for linkers and compiler drivers.

use super::types::LinkerFamily;

/// Extract the filename from a tool path, handling both `/` and `\` separators
/// so that Windows-style paths work correctly on all platforms.
pub(super) fn cross_platform_file_name(tool: &str) -> &str {
    tool.rsplit(['/', '\\']).next().unwrap_or(tool)
}

/// Extract the stem (filename without last extension) from a filename.
pub(super) fn file_stem(filename: &str) -> &str {
    match filename.rfind('.') {
        Some(pos) if pos > 0 => &filename[..pos],
        _ => filename,
    }
}

/// Detect the linker family from the tool path/name.
pub(super) fn detect_family(tool: &str) -> Option<LinkerFamily> {
    // Handle both `/` and `\` as path separators so Windows-style paths
    // (e.g. "C:\emsdk\upstream\bin\wasm-ld.exe") work on all platforms.
    let full_name = cross_platform_file_name(tool);
    let stem = file_stem(full_name);

    // MSVC link.exe (case-insensitive) — check stem so "link.exe" matches
    if stem.eq_ignore_ascii_case("link") {
        return Some(LinkerFamily::MsvcLink);
    }

    // LLVM lld variants: lld, ld.lld, ld.lld-17, lld-17, wasm-ld, etc.
    // Check full_name first for dotted names, then stem for simple names.
    // Must come before GNU ld to avoid "ld.lld" matching as "ld".
    if full_name == "ld.lld"
        || full_name.starts_with("ld.lld-")
        || stem == "lld"
        || stem.starts_with("lld-")
        || stem == "wasm-ld"
    {
        return Some(LinkerFamily::Lld);
    }

    // GNU ld variants: ld, ld.bfd, ld.gold, x86_64-linux-gnu-ld, etc.
    // Check full_name for dotted names (ld.bfd, ld.gold), stem for plain "ld".
    if full_name == "ld.bfd" || full_name == "ld.gold" || stem == "ld" || stem.ends_with("-ld") {
        return Some(LinkerFamily::Ld);
    }

    None
}

/// Check if a tool name is a compiler driver (gcc, g++, clang, clang++, cc, c++).
pub(super) fn is_compiler_driver(tool: &str) -> bool {
    let stem = file_stem(cross_platform_file_name(tool));

    // clang++, clang-17, x86_64-w64-mingw32-gcc, emcc, em++, etc.
    matches!(stem, "cc" | "c++" | "emcc" | "em++")
        || stem == "gcc"
        || stem == "g++"
        || stem.ends_with("-gcc")
        || stem.ends_with("-g++")
        || stem.contains("clang")
}

/// Check if a tool name is a known linker (not archiver).
#[must_use]
pub fn is_linker(tool: &str) -> bool {
    detect_family(tool).is_some()
}

/// Check if a tool invocation is a link operation (shared lib, DLL, or executable).
///
/// This checks both direct linkers (ld, lld, link.exe) and compiler drivers
/// used for linking. For compiler drivers, returns true when no compile-only
/// flag (`-c`, `-E`, `-S`) is present — this routes exe links to the link path.
/// Cases like `gcc main.c -o main` (compile+link) will be routed here too,
/// but the parser will find no object inputs and return NonCacheable → passthrough.
///
/// Response files (`@file`) are expanded before checking for compile-only flags,
/// since build systems may place all flags (including `-c`) inside a response file.
#[must_use]
pub fn is_link_invocation(tool: &str, args: &[String]) -> bool {
    if detect_family(tool).is_some() {
        return true;
    }
    // Compiler driver: it's a link invocation if no compile-only flag is present.
    // `-x c++-header` / `-x c-header` imply compilation (PCH generation), not linking.
    if !is_compiler_driver(tool) {
        return false;
    }

    // Expand response files so we can see flags like -c that may be inside them.
    // If expansion fails (e.g. file not found), fall back to raw args.
    let expanded;
    let effective_args = if args.iter().any(|a| a.starts_with('@') && a.len() > 1) {
        expanded = super::super::response_file::expand_response_files(args).unwrap_or_default();
        if expanded.is_empty() {
            args
        } else {
            &expanded
        }
    } else {
        args
    };

    if effective_args
        .iter()
        .any(|a| a == "-c" || a == "-E" || a == "-S" || a == "--precompile")
    {
        return false;
    }
    // Check for `-x` language modes that imply compilation (not linking):
    // header (PCH) and header-unit (C++20) imply compilation without `-c`.
    // Module mode does NOT imply compilation — it needs `-c` or `--precompile`.
    for pair in effective_args.windows(2) {
        if pair[0] == "-x" {
            if let Some(mode) = super::super::parse::source_mode_from_language(&pair[1]) {
                if mode.implies_compilation() {
                    return false;
                }
            }
        }
    }
    true
}
