//! Linker detection and argument parsing for zccache.
//!
//! Handles parsing command-line arguments for `ld`, `lld`, MSVC `link.exe`,
//! and compiler drivers (`gcc`, `clang`) to determine cacheability for
//! linking (shared libraries, DLLs, and executables).

use zccache_core::NormalizedPath;

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
    /// Input object files and libraries (order preserved â€” matters for linker).
    pub input_files: Vec<NormalizedPath>,
    /// The output file path (shared library, DLL, or executable).
    pub output_file: NormalizedPath,
    /// Secondary output files produced alongside the primary output.
    /// E.g., MSVC `/IMPLIB:foo.lib` produces `foo.lib` + `foo.exp`.
    /// May not all exist after linking â€” the server should skip missing ones.
    pub secondary_outputs: Vec<NormalizedPath>,
    /// Flags relevant to cache keying (optimization, target, etc.).
    pub cache_relevant_flags: Vec<String>,
    /// The full original argument list (for fallback execution).
    pub original_args: Vec<String>,
    /// Whether non-deterministic output is detected.
    pub non_deterministic: bool,
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
/// flag (`-c`, `-E`, `-S`) is present â€” this routes exe links to the link path.
/// Cases like `gcc main.c -o main` (compile+link) will be routed here too,
/// but the parser will find no object inputs and return NonCacheable â†’ passthrough.
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
        expanded = crate::response_file::expand_response_files(args).unwrap_or_default();
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
    // Module mode does NOT imply compilation â€” it needs `-c` or `--precompile`.
    for pair in effective_args.windows(2) {
        if pair[0] == "-x" {
            if let Some(mode) = crate::parse::source_mode_from_language(&pair[1]) {
                if mode.implies_compilation() {
                    return false;
                }
            }
        }
    }
    true
}

/// Detect the linker family from the tool path/name.
/// Extract the filename from a tool path, handling both `/` and `\` separators
/// so that Windows-style paths work correctly on all platforms.
fn cross_platform_file_name(tool: &str) -> &str {
    tool.rsplit(['/', '\\']).next().unwrap_or(tool)
}

/// Extract the stem (filename without last extension) from a filename.
fn file_stem(filename: &str) -> &str {
    match filename.rfind('.') {
        Some(pos) if pos > 0 => &filename[..pos],
        _ => filename,
    }
}

fn detect_family(tool: &str) -> Option<LinkerFamily> {
    // Handle both `/` and `\` as path separators so Windows-style paths
    // (e.g. "C:\emsdk\upstream\bin\wasm-ld.exe") work on all platforms.
    let full_name = cross_platform_file_name(tool);
    let stem = file_stem(full_name);

    // MSVC link.exe (case-insensitive) â€” check stem so "link.exe" matches
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
fn is_compiler_driver(tool: &str) -> bool {
    let stem = file_stem(cross_platform_file_name(tool));

    // clang++, clang-17, x86_64-w64-mingw32-gcc, emcc, em++, etc.
    matches!(stem, "cc" | "c++" | "emcc" | "em++")
        || stem == "gcc"
        || stem == "g++"
        || stem.ends_with("-gcc")
        || stem.ends_with("-g++")
        || stem.contains("clang")
}

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

/// Parse GNU ld / lld arguments for linking.
///
/// Both shared library (`-shared` / `-dylib`) and executable linking are cacheable.
/// `-shared` / `-dylib` are kept as cache-relevant flags since they affect output.
fn parse_gnu_ld(tool: &str, family: LinkerFamily, args: Vec<String>) -> ParsedLinkerInvocation {
    if args.is_empty() {
        return ParsedLinkerInvocation::NonCacheable {
            reason: "no arguments".to_string(),
        };
    }

    let mut output_file: Option<NormalizedPath> = None;
    let mut input_files: Vec<NormalizedPath> = Vec::new();
    let mut cache_relevant_flags: Vec<String> = Vec::new();
    let mut has_build_id_uuid = false;
    let mut secondary_outputs: Vec<NormalizedPath> = Vec::new();

    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];

        // --out-implib=<path> â€” GNU/LLD import library (secondary output on Windows)
        if let Some(implib) = arg.strip_prefix("--out-implib=") {
            secondary_outputs.push(NormalizedPath::new(implib));
            cache_relevant_flags.push(arg.clone());
            i += 1;
            continue;
        }
        // --out-implib <path> (space-separated variant)
        if arg == "--out-implib" {
            cache_relevant_flags.push(arg.clone());
            i += 1;
            if i < args.len() {
                secondary_outputs.push(NormalizedPath::new(&args[i]));
                cache_relevant_flags.push(args[i].clone());
            }
            i += 1;
            continue;
        }

        // -shared or --shared â€” shared library mode (cache-relevant: affects output type)
        if arg == "-shared" || arg == "--shared" {
            cache_relevant_flags.push(arg.clone());
            i += 1;
            continue;
        }

        // macOS: -dylib (cache-relevant: affects output type)
        if arg == "-dylib" {
            cache_relevant_flags.push(arg.clone());
            i += 1;
            continue;
        }

        // -o <output> or --output=<output>
        if arg == "-o" {
            i += 1;
            if i < args.len() {
                output_file = Some(NormalizedPath::new(&args[i]));
            }
            i += 1;
            continue;
        }
        if let Some(rest) = arg.strip_prefix("--output=") {
            output_file = Some(NormalizedPath::new(rest));
            i += 1;
            continue;
        }

        // --build-id=uuid â†’ non-deterministic
        if arg == "--build-id=uuid" {
            has_build_id_uuid = true;
            cache_relevant_flags.push(arg.clone());
            i += 1;
            continue;
        }

        // --build-id=<style> (sha1, md5, none, etc.) â†’ deterministic
        if arg.starts_with("--build-id") {
            cache_relevant_flags.push(arg.clone());
            i += 1;
            continue;
        }

        // -soname <name> or --soname=<name>
        if arg == "-soname" || arg == "-h" {
            cache_relevant_flags.push(arg.clone());
            i += 1;
            if i < args.len() {
                cache_relevant_flags.push(args[i].clone());
            }
            i += 1;
            continue;
        }
        if let Some(rest) = arg.strip_prefix("--soname=") {
            cache_relevant_flags.push(format!("--soname={rest}"));
            i += 1;
            continue;
        }

        // macOS: -install_name <name>
        if arg == "-install_name" {
            cache_relevant_flags.push(arg.clone());
            i += 1;
            if i < args.len() {
                cache_relevant_flags.push(args[i].clone());
            }
            i += 1;
            continue;
        }

        // -L<path> or -L <path> â€” library search path (cache-relevant)
        if arg == "-L" {
            cache_relevant_flags.push(arg.clone());
            i += 1;
            if i < args.len() {
                cache_relevant_flags.push(args[i].clone());
            }
            i += 1;
            continue;
        }
        if arg.starts_with("-L") {
            cache_relevant_flags.push(arg.clone());
            i += 1;
            continue;
        }

        // -l<lib> â€” library dependency (cache-relevant, order matters)
        if arg.starts_with("-l") {
            cache_relevant_flags.push(arg.clone());
            i += 1;
            continue;
        }

        // -Wl, pass-through (from compiler driver)
        if arg.starts_with("-Wl,") {
            cache_relevant_flags.push(arg.clone());
            i += 1;
            continue;
        }

        // Flags that take a value in the next arg
        if arg == "-T" || arg == "--script" || arg == "-z" || arg == "--version-script" {
            cache_relevant_flags.push(arg.clone());
            i += 1;
            if i < args.len() {
                cache_relevant_flags.push(args[i].clone());
                // -T (linker script) and --version-script are input files that affect output
                if arg == "-T" || arg == "--script" || arg == "--version-script" {
                    input_files.push(NormalizedPath::new(&args[i]));
                }
            }
            i += 1;
            continue;
        }

        // Flags with = syntax
        if let Some(rest) = arg.strip_prefix("--version-script=") {
            cache_relevant_flags.push(arg.clone());
            input_files.push(NormalizedPath::new(rest));
            i += 1;
            continue;
        }

        // Other flags
        if arg.starts_with('-') {
            cache_relevant_flags.push(arg.clone());
            i += 1;
            continue;
        }

        // Positional argument â€” input file (object file or library)
        input_files.push(NormalizedPath::new(arg));
        i += 1;
    }

    let output_file = match output_file {
        Some(f) => f,
        None => {
            return ParsedLinkerInvocation::NonCacheable {
                reason: "no output file specified (-o)".to_string(),
            };
        }
    };

    if input_files.is_empty() {
        return ParsedLinkerInvocation::NonCacheable {
            reason: "no input files specified".to_string(),
        };
    }

    ParsedLinkerInvocation::Cacheable(CacheableLink {
        tool: NormalizedPath::new(tool),
        family,
        input_files,
        output_file,
        secondary_outputs,
        cache_relevant_flags,
        original_args: args,
        non_deterministic: has_build_id_uuid,
    })
}

/// Parse MSVC link.exe arguments for linking (DLL or executable).
///
/// Both `/DLL` (DLL) and non-`/DLL` (executable) invocations are cacheable.
/// `/DLL` is kept as a cache-relevant flag since it affects output type.
fn parse_msvc_link(tool: &str, args: Vec<String>) -> ParsedLinkerInvocation {
    if args.is_empty() {
        return ParsedLinkerInvocation::NonCacheable {
            reason: "no arguments".to_string(),
        };
    }

    let mut is_dll = false;
    let mut output_file: Option<NormalizedPath> = None;
    let mut input_files: Vec<NormalizedPath> = Vec::new();
    let mut cache_relevant_flags: Vec<String> = Vec::new();
    let mut has_deterministic = false;
    let mut secondary_outputs: Vec<NormalizedPath> = Vec::new();

    for arg in &args {
        let upper = arg.to_uppercase();

        // /DLL â€” DLL mode (cache-relevant: affects output type)
        if upper == "/DLL" || upper == "-DLL" {
            is_dll = true;
            cache_relevant_flags.push(arg.clone());
            continue;
        }

        // /OUT:filename
        if upper.starts_with("/OUT:") || upper.starts_with("-OUT:") {
            output_file = Some(NormalizedPath::new(&arg[5..]));
            continue;
        }

        // /DETERMINISTIC
        if upper == "/DETERMINISTIC" || upper == "-DETERMINISTIC" {
            has_deterministic = true;
            cache_relevant_flags.push(arg.clone());
            continue;
        }

        // /IMPLIB:filename â€” import library (secondary output)
        // MSVC also auto-generates a .exp alongside the .lib
        if upper.starts_with("/IMPLIB:") || upper.starts_with("-IMPLIB:") {
            let implib_path = NormalizedPath::new(&arg[8..]);
            let exp_path = NormalizedPath::new(implib_path.with_extension("exp"));
            secondary_outputs.push(implib_path);
            secondary_outputs.push(exp_path);
            cache_relevant_flags.push(arg.clone());
            continue;
        }

        // Other flags
        if arg.starts_with('/') || arg.starts_with('-') {
            cache_relevant_flags.push(arg.clone());
            continue;
        }

        // Positional â€” input file
        input_files.push(NormalizedPath::new(arg));
    }

    if input_files.is_empty() {
        return ParsedLinkerInvocation::NonCacheable {
            reason: "no input files specified".to_string(),
        };
    }

    // If no /OUT:, link.exe defaults to first input with .dll/.exe extension
    let output_file = output_file.unwrap_or_else(|| {
        let first = &input_files[0];
        let ext = if is_dll { "dll" } else { "exe" };
        NormalizedPath::new(first.with_extension(ext))
    });

    ParsedLinkerInvocation::Cacheable(CacheableLink {
        tool: NormalizedPath::new(tool),
        family: LinkerFamily::MsvcLink,
        input_files,
        output_file,
        secondary_outputs,
        cache_relevant_flags,
        original_args: args,
        non_deterministic: !has_deterministic,
    })
}

/// Object/library file extensions recognized as linker inputs.
const OBJECT_EXTENSIONS: &[&str] = &["o", "obj", "a", "lib", "lo", "so", "dylib", "dll"];

/// Check if a path looks like a linker input file (object, archive, library).
fn is_linker_input(path: &str) -> bool {
    if let Some(ext) = std::path::Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
    {
        OBJECT_EXTENSIONS.contains(&ext)
    } else {
        false
    }
}

/// Parse a compiler driver invocation used for linking.
///
/// Handles `gcc -shared -o libfoo.so a.o b.o`, `gcc -o main main.o`, and similar.
/// The compiler driver passes flags through to the linker internally,
/// so we treat the full invocation as a link operation. `-shared` is kept as a
/// cache-relevant flag since it affects output type.
fn parse_compiler_driver_link(tool: &str, args: Vec<String>) -> ParsedLinkerInvocation {
    if args.is_empty() {
        return ParsedLinkerInvocation::NonCacheable {
            reason: "no arguments".to_string(),
        };
    }

    let mut has_compile_only = false;
    let mut output_file: Option<NormalizedPath> = None;
    let mut input_files: Vec<NormalizedPath> = Vec::new();
    let mut cache_relevant_flags: Vec<String> = Vec::new();
    let mut has_build_id_uuid = false;
    let mut secondary_outputs: Vec<NormalizedPath> = Vec::new();

    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];

        // -shared â€” shared library mode (cache-relevant: affects output type)
        if arg == "-shared" || arg == "--shared" {
            cache_relevant_flags.push(arg.clone());
            i += 1;
            continue;
        }

        // -c â€” compile only, NOT linking
        if arg == "-c" {
            has_compile_only = true;
            i += 1;
            continue;
        }

        // -o <output>
        if arg == "-o" {
            i += 1;
            if i < args.len() {
                output_file = Some(NormalizedPath::new(&args[i]));
            }
            i += 1;
            continue;
        }

        // -Wl, pass-through to linker â€” check for non-determinism and secondary outputs
        if arg.starts_with("-Wl,") {
            for part in arg.split(',') {
                if part == "--build-id=uuid" {
                    has_build_id_uuid = true;
                }
                // GNU/LLD --out-implib produces an import library (.dll.a) as a side effect.
                // Meson/ninja uses: -Wl,--out-implib=path/to/foo.dll.a
                if let Some(implib) = part.strip_prefix("--out-implib=") {
                    secondary_outputs.push(NormalizedPath::new(implib));
                }
            }
            cache_relevant_flags.push(arg.clone());
            i += 1;
            continue;
        }

        // -L<path> or -L <path>
        if arg == "-L" {
            cache_relevant_flags.push(arg.clone());
            i += 1;
            if i < args.len() {
                cache_relevant_flags.push(args[i].clone());
            }
            i += 1;
            continue;
        }
        if arg.starts_with("-L") {
            cache_relevant_flags.push(arg.clone());
            i += 1;
            continue;
        }

        // -l<lib>
        if arg.starts_with("-l") {
            cache_relevant_flags.push(arg.clone());
            i += 1;
            continue;
        }

        // Flags with value: -target, -isysroot, etc.
        if arg == "-target" || arg == "--target" || arg == "-isysroot" {
            cache_relevant_flags.push(arg.clone());
            i += 1;
            if i < args.len() {
                cache_relevant_flags.push(args[i].clone());
            }
            i += 1;
            continue;
        }

        // Other flags
        if arg.starts_with('-') {
            cache_relevant_flags.push(arg.clone());
            i += 1;
            continue;
        }

        // Positional argument â€” input file (object or source)
        if is_linker_input(arg) {
            input_files.push(NormalizedPath::new(arg));
        }
        // Ignore non-object positional args (e.g., source files passed to gcc
        // during combined compile-and-link â€” too complex to cache)
        i += 1;
    }

    if has_compile_only {
        return ParsedLinkerInvocation::NonCacheable {
            reason: "-c flag present (compilation, not linking)".to_string(),
        };
    }

    let output_file = match output_file {
        Some(f) => f,
        None => {
            return ParsedLinkerInvocation::NonCacheable {
                reason: "no output file specified (-o)".to_string(),
            };
        }
    };

    if input_files.is_empty() {
        return ParsedLinkerInvocation::NonCacheable {
            reason: "no input files specified".to_string(),
        };
    }

    ParsedLinkerInvocation::Cacheable(CacheableLink {
        tool: NormalizedPath::new(tool),
        family: LinkerFamily::CompilerDriver,
        input_files,
        output_file,
        secondary_outputs,
        cache_relevant_flags,
        original_args: args,
        non_deterministic: has_build_id_uuid,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(s: &[&str]) -> Vec<String> {
        s.iter().map(|x| x.to_string()).collect()
    }

    // â”€â”€â”€ Detection â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€

    #[test]
    fn detect_gnu_ld() {
        assert_eq!(detect_family("ld"), Some(LinkerFamily::Ld));
        assert_eq!(detect_family("/usr/bin/ld"), Some(LinkerFamily::Ld));
        assert_eq!(detect_family("ld.bfd"), Some(LinkerFamily::Ld));
        assert_eq!(detect_family("ld.gold"), Some(LinkerFamily::Ld));
        assert_eq!(detect_family("x86_64-linux-gnu-ld"), Some(LinkerFamily::Ld));
        assert_eq!(
            detect_family("aarch64-linux-gnu-ld"),
            Some(LinkerFamily::Ld)
        );
    }

    #[test]
    fn detect_llvm_lld() {
        assert_eq!(detect_family("lld"), Some(LinkerFamily::Lld));
        assert_eq!(detect_family("lld-17"), Some(LinkerFamily::Lld));
        assert_eq!(detect_family("ld.lld"), Some(LinkerFamily::Lld));
        assert_eq!(detect_family("ld.lld-17"), Some(LinkerFamily::Lld));
        assert_eq!(detect_family("/usr/bin/lld"), Some(LinkerFamily::Lld));
    }

    #[test]
    fn detect_wasm_ld() {
        assert_eq!(detect_family("wasm-ld"), Some(LinkerFamily::Lld));
        assert_eq!(detect_family("wasm-ld.exe"), Some(LinkerFamily::Lld));
        assert_eq!(detect_family("/usr/bin/wasm-ld"), Some(LinkerFamily::Lld));
        assert_eq!(
            detect_family("C:\\emsdk\\upstream\\bin\\wasm-ld.exe"),
            Some(LinkerFamily::Lld)
        );
    }

    #[test]
    fn detect_msvc_link() {
        assert_eq!(detect_family("link"), Some(LinkerFamily::MsvcLink));
        assert_eq!(detect_family("link.exe"), Some(LinkerFamily::MsvcLink));
        assert_eq!(detect_family("LINK"), Some(LinkerFamily::MsvcLink));
        assert_eq!(detect_family("LINK.EXE"), Some(LinkerFamily::MsvcLink));
    }

    #[test]
    fn detect_unknown_tool() {
        assert_eq!(detect_family("gcc"), None);
        assert_eq!(detect_family("clang"), None);
        assert_eq!(detect_family("ar"), None);
        assert_eq!(detect_family("lib.exe"), None);
    }

    #[test]
    fn is_linker_works() {
        assert!(is_linker("ld"));
        assert!(is_linker("lld"));
        assert!(is_linker("link.exe"));
        assert!(!is_linker("gcc"));
        assert!(!is_linker("ar"));
        assert!(!is_linker("lib.exe"));
    }

    // â”€â”€â”€ GNU ld shared library parsing â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€

    #[test]
    fn basic_shared_lib() {
        let result =
            parse_linker_invocation("ld", args(&["-shared", "-o", "libfoo.so", "a.o", "b.o"]));
        match result {
            ParsedLinkerInvocation::Cacheable(c) => {
                assert_eq!(c.family, LinkerFamily::Ld);
                assert_eq!(c.output_file, NormalizedPath::new("libfoo.so"));
                assert_eq!(c.input_files.len(), 2);
                assert_eq!(c.input_files[0], NormalizedPath::new("a.o"));
                assert_eq!(c.input_files[1], NormalizedPath::new("b.o"));
                assert!(!c.non_deterministic); // GNU ld is deterministic by default
            }
            other => panic!("expected cacheable, got: {other:?}"),
        }
    }

    #[test]
    fn shared_lib_with_soname() {
        let result = parse_linker_invocation(
            "ld",
            args(&[
                "-shared",
                "-soname",
                "libfoo.so.1",
                "-o",
                "libfoo.so.1.0",
                "a.o",
            ]),
        );
        match result {
            ParsedLinkerInvocation::Cacheable(c) => {
                assert_eq!(c.output_file, NormalizedPath::new("libfoo.so.1.0"));
                assert!(c.cache_relevant_flags.contains(&"-soname".to_string()));
                assert!(c.cache_relevant_flags.contains(&"libfoo.so.1".to_string()));
            }
            other => panic!("expected cacheable, got: {other:?}"),
        }
    }

    #[test]
    fn shared_lib_with_libraries() {
        let result = parse_linker_invocation(
            "ld",
            args(&[
                "-shared",
                "-o",
                "libfoo.so",
                "a.o",
                "-lm",
                "-lpthread",
                "-L/usr/lib",
            ]),
        );
        match result {
            ParsedLinkerInvocation::Cacheable(c) => {
                assert_eq!(c.input_files, vec![NormalizedPath::new("a.o")]);
                assert!(c.cache_relevant_flags.contains(&"-lm".to_string()));
                assert!(c.cache_relevant_flags.contains(&"-lpthread".to_string()));
                assert!(c.cache_relevant_flags.contains(&"-L/usr/lib".to_string()));
            }
            other => panic!("expected cacheable, got: {other:?}"),
        }
    }

    #[test]
    fn exe_link_cacheable() {
        // Executable linking (no -shared) should be cacheable
        let result = parse_linker_invocation("ld", args(&["-o", "a.out", "main.o"]));
        match result {
            ParsedLinkerInvocation::Cacheable(c) => {
                assert_eq!(c.family, LinkerFamily::Ld);
                assert_eq!(c.output_file, NormalizedPath::new("a.out"));
                assert_eq!(c.input_files, vec![NormalizedPath::new("main.o")]);
                assert!(!c.non_deterministic);
            }
            other => panic!("expected cacheable, got: {other:?}"),
        }
    }

    #[test]
    fn no_output_non_cacheable() {
        let result = parse_linker_invocation("ld", args(&["-shared", "a.o"]));
        assert!(matches!(
            result,
            ParsedLinkerInvocation::NonCacheable { .. }
        ));
    }

    #[test]
    fn no_inputs_non_cacheable() {
        let result = parse_linker_invocation("ld", args(&["-shared", "-o", "libfoo.so"]));
        assert!(matches!(
            result,
            ParsedLinkerInvocation::NonCacheable { .. }
        ));
    }

    #[test]
    fn no_args_non_cacheable() {
        let result = parse_linker_invocation("ld", args(&[]));
        assert!(matches!(
            result,
            ParsedLinkerInvocation::NonCacheable { .. }
        ));
    }

    #[test]
    fn preserves_input_order() {
        let result = parse_linker_invocation(
            "ld",
            args(&["-shared", "-o", "libfoo.so", "z.o", "a.o", "m.o"]),
        );
        match result {
            ParsedLinkerInvocation::Cacheable(c) => {
                assert_eq!(c.input_files[0], NormalizedPath::new("z.o"));
                assert_eq!(c.input_files[1], NormalizedPath::new("a.o"));
                assert_eq!(c.input_files[2], NormalizedPath::new("m.o"));
            }
            other => panic!("expected cacheable, got: {other:?}"),
        }
    }

    // â”€â”€â”€ Non-determinism (timestamps, build-id) â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€

    #[test]
    fn build_id_uuid_is_non_deterministic() {
        let result = parse_linker_invocation(
            "ld",
            args(&["-shared", "--build-id=uuid", "-o", "libfoo.so", "a.o"]),
        );
        match result {
            ParsedLinkerInvocation::Cacheable(c) => {
                assert!(
                    c.non_deterministic,
                    "--build-id=uuid produces random output â€” must be flagged"
                );
            }
            other => panic!("expected cacheable, got: {other:?}"),
        }
    }

    #[test]
    fn build_id_sha1_is_deterministic() {
        let result = parse_linker_invocation(
            "ld",
            args(&["-shared", "--build-id=sha1", "-o", "libfoo.so", "a.o"]),
        );
        match result {
            ParsedLinkerInvocation::Cacheable(c) => {
                assert!(
                    !c.non_deterministic,
                    "--build-id=sha1 is content-derived â€” deterministic"
                );
            }
            other => panic!("expected cacheable, got: {other:?}"),
        }
    }

    #[test]
    fn build_id_none_is_deterministic() {
        let result = parse_linker_invocation(
            "ld",
            args(&["-shared", "--build-id=none", "-o", "libfoo.so", "a.o"]),
        );
        match result {
            ParsedLinkerInvocation::Cacheable(c) => {
                assert!(!c.non_deterministic);
            }
            other => panic!("expected cacheable, got: {other:?}"),
        }
    }

    #[test]
    fn default_ld_is_deterministic() {
        // GNU ld without --build-id is deterministic (no random build ID inserted)
        let result = parse_linker_invocation("ld", args(&["-shared", "-o", "libfoo.so", "a.o"]));
        match result {
            ParsedLinkerInvocation::Cacheable(c) => {
                assert!(!c.non_deterministic);
            }
            other => panic!("expected cacheable, got: {other:?}"),
        }
    }

    // â”€â”€â”€ macOS dylib â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€

    #[test]
    fn macos_dylib() {
        let result =
            parse_linker_invocation("ld", args(&["-dylib", "-o", "libfoo.dylib", "a.o", "b.o"]));
        match result {
            ParsedLinkerInvocation::Cacheable(c) => {
                assert_eq!(c.output_file, NormalizedPath::new("libfoo.dylib"));
                assert_eq!(c.input_files.len(), 2);
                assert!(!c.non_deterministic);
            }
            other => panic!("expected cacheable, got: {other:?}"),
        }
    }

    #[test]
    fn macos_dylib_with_install_name() {
        let result = parse_linker_invocation(
            "ld",
            args(&[
                "-dylib",
                "-install_name",
                "@rpath/libfoo.dylib",
                "-o",
                "libfoo.dylib",
                "a.o",
            ]),
        );
        match result {
            ParsedLinkerInvocation::Cacheable(c) => {
                assert!(c
                    .cache_relevant_flags
                    .contains(&"-install_name".to_string()));
                assert!(c
                    .cache_relevant_flags
                    .contains(&"@rpath/libfoo.dylib".to_string()));
            }
            other => panic!("expected cacheable, got: {other:?}"),
        }
    }

    // â”€â”€â”€ LLD â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€

    #[test]
    fn lld_shared_lib() {
        let result = parse_linker_invocation(
            "ld.lld",
            args(&["-shared", "-o", "libfoo.so", "a.o", "b.o"]),
        );
        match result {
            ParsedLinkerInvocation::Cacheable(c) => {
                assert_eq!(c.family, LinkerFamily::Lld);
                assert_eq!(c.output_file, NormalizedPath::new("libfoo.so"));
            }
            other => panic!("expected cacheable, got: {other:?}"),
        }
    }

    // â”€â”€â”€ Linker script and version script â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€

    #[test]
    fn with_linker_script() {
        let result = parse_linker_invocation(
            "ld",
            args(&["-shared", "-T", "link.ld", "-o", "libfoo.so", "a.o"]),
        );
        match result {
            ParsedLinkerInvocation::Cacheable(c) => {
                // Linker script is an input file (affects output)
                assert!(c.input_files.contains(&NormalizedPath::new("link.ld")));
                assert!(c.input_files.contains(&NormalizedPath::new("a.o")));
            }
            other => panic!("expected cacheable, got: {other:?}"),
        }
    }

    #[test]
    fn with_version_script() {
        let result = parse_linker_invocation(
            "ld",
            args(&[
                "-shared",
                "--version-script=libfoo.map",
                "-o",
                "libfoo.so",
                "a.o",
            ]),
        );
        match result {
            ParsedLinkerInvocation::Cacheable(c) => {
                // Version script is an input file
                assert!(c.input_files.contains(&NormalizedPath::new("libfoo.map")));
            }
            other => panic!("expected cacheable, got: {other:?}"),
        }
    }

    // â”€â”€â”€ MSVC link.exe DLL parsing â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€

    #[test]
    fn basic_msvc_dll() {
        let result = parse_linker_invocation(
            "link.exe",
            args(&["/DLL", "/OUT:foo.dll", "a.obj", "b.obj"]),
        );
        match result {
            ParsedLinkerInvocation::Cacheable(c) => {
                assert_eq!(c.family, LinkerFamily::MsvcLink);
                assert_eq!(c.output_file, NormalizedPath::new("foo.dll"));
                assert_eq!(c.input_files.len(), 2);
                assert_eq!(c.input_files[0], NormalizedPath::new("a.obj"));
                assert_eq!(c.input_files[1], NormalizedPath::new("b.obj"));
                assert!(c.non_deterministic); // no /DETERMINISTIC
            }
            other => panic!("expected cacheable, got: {other:?}"),
        }
    }

    #[test]
    fn msvc_dll_with_deterministic() {
        let result = parse_linker_invocation(
            "link.exe",
            args(&["/DLL", "/DETERMINISTIC", "/OUT:foo.dll", "a.obj"]),
        );
        match result {
            ParsedLinkerInvocation::Cacheable(c) => {
                assert!(!c.non_deterministic); // /DETERMINISTIC present
            }
            other => panic!("expected cacheable, got: {other:?}"),
        }
    }

    #[test]
    fn msvc_exe_cacheable() {
        // Executable linking (no /DLL) should be cacheable
        let result = parse_linker_invocation("link.exe", args(&["/OUT:foo.exe", "main.obj"]));
        match result {
            ParsedLinkerInvocation::Cacheable(c) => {
                assert_eq!(c.family, LinkerFamily::MsvcLink);
                assert_eq!(c.output_file, NormalizedPath::new("foo.exe"));
                assert_eq!(c.input_files, vec![NormalizedPath::new("main.obj")]);
            }
            other => panic!("expected cacheable, got: {other:?}"),
        }
    }

    #[test]
    fn msvc_exe_default_output_name() {
        // Without /OUT: and without /DLL, defaults to first input with .exe extension
        let result = parse_linker_invocation("link.exe", args(&["main.obj", "util.obj"]));
        match result {
            ParsedLinkerInvocation::Cacheable(c) => {
                assert_eq!(c.output_file, NormalizedPath::new("main.exe"));
            }
            other => panic!("expected cacheable, got: {other:?}"),
        }
    }

    #[test]
    fn msvc_dll_no_inputs() {
        let result = parse_linker_invocation("link.exe", args(&["/DLL", "/OUT:foo.dll"]));
        assert!(matches!(
            result,
            ParsedLinkerInvocation::NonCacheable { .. }
        ));
    }

    #[test]
    fn msvc_dll_default_output_name() {
        // Without /OUT:, defaults to first input with .dll extension
        let result = parse_linker_invocation("link.exe", args(&["/DLL", "a.obj", "b.obj"]));
        match result {
            ParsedLinkerInvocation::Cacheable(c) => {
                assert_eq!(c.output_file, NormalizedPath::new("a.dll"));
            }
            other => panic!("expected cacheable, got: {other:?}"),
        }
    }

    #[test]
    fn msvc_dll_preserves_input_order() {
        let result = parse_linker_invocation(
            "link.exe",
            args(&["/DLL", "/OUT:foo.dll", "z.obj", "a.obj", "m.obj"]),
        );
        match result {
            ParsedLinkerInvocation::Cacheable(c) => {
                assert_eq!(c.input_files[0], NormalizedPath::new("z.obj"));
                assert_eq!(c.input_files[1], NormalizedPath::new("a.obj"));
                assert_eq!(c.input_files[2], NormalizedPath::new("m.obj"));
            }
            other => panic!("expected cacheable, got: {other:?}"),
        }
    }

    #[test]
    fn msvc_dll_with_implib() {
        let result = parse_linker_invocation(
            "link.exe",
            args(&["/DLL", "/OUT:foo.dll", "/IMPLIB:foo.lib", "a.obj"]),
        );
        match result {
            ParsedLinkerInvocation::Cacheable(c) => {
                assert!(c
                    .cache_relevant_flags
                    .contains(&"/IMPLIB:foo.lib".to_string()));
                // /IMPLIB: extracts secondary outputs: .lib + inferred .exp
                assert_eq!(c.secondary_outputs.len(), 2);
                assert_eq!(c.secondary_outputs[0], NormalizedPath::new("foo.lib"));
                assert_eq!(c.secondary_outputs[1], NormalizedPath::new("foo.exp"));
            }
            other => panic!("expected cacheable, got: {other:?}"),
        }
    }

    #[test]
    fn msvc_dll_without_implib_no_secondary() {
        let result = parse_linker_invocation("link.exe", args(&["/DLL", "/OUT:foo.dll", "a.obj"]));
        match result {
            ParsedLinkerInvocation::Cacheable(c) => {
                assert!(c.secondary_outputs.is_empty());
            }
            other => panic!("expected cacheable, got: {other:?}"),
        }
    }

    #[test]
    fn msvc_implib_dash_syntax() {
        let result = parse_linker_invocation(
            "link.exe",
            args(&["/DLL", "-IMPLIB:mylib.lib", "/OUT:mylib.dll", "a.obj"]),
        );
        match result {
            ParsedLinkerInvocation::Cacheable(c) => {
                assert_eq!(c.secondary_outputs.len(), 2);
                assert_eq!(c.secondary_outputs[0], NormalizedPath::new("mylib.lib"));
                assert_eq!(c.secondary_outputs[1], NormalizedPath::new("mylib.exp"));
            }
            other => panic!("expected cacheable, got: {other:?}"),
        }
    }

    #[test]
    fn gnu_ld_no_secondary_outputs() {
        let result = parse_linker_invocation("ld", args(&["-shared", "-o", "libfoo.so", "a.o"]));
        match result {
            ParsedLinkerInvocation::Cacheable(c) => {
                assert!(c.secondary_outputs.is_empty());
            }
            other => panic!("expected cacheable, got: {other:?}"),
        }
    }

    #[test]
    fn gcc_no_secondary_outputs() {
        let result = parse_linker_invocation("gcc", args(&["-shared", "-o", "libfoo.so", "a.o"]));
        match result {
            ParsedLinkerInvocation::Cacheable(c) => {
                assert!(c.secondary_outputs.is_empty());
            }
            other => panic!("expected cacheable, got: {other:?}"),
        }
    }

    #[test]
    fn msvc_dll_with_flags() {
        let result = parse_linker_invocation(
            "link.exe",
            args(&["/DLL", "/NOLOGO", "/MACHINE:X64", "/OUT:foo.dll", "a.obj"]),
        );
        match result {
            ParsedLinkerInvocation::Cacheable(c) => {
                assert!(c.cache_relevant_flags.contains(&"/NOLOGO".to_string()));
                assert!(c.cache_relevant_flags.contains(&"/MACHINE:X64".to_string()));
            }
            other => panic!("expected cacheable, got: {other:?}"),
        }
    }

    #[test]
    fn msvc_dll_dash_syntax() {
        // link.exe also accepts - prefix for flags
        let result = parse_linker_invocation(
            "link.exe",
            args(&["-DLL", "-OUT:foo.dll", "-DETERMINISTIC", "a.obj"]),
        );
        match result {
            ParsedLinkerInvocation::Cacheable(c) => {
                assert_eq!(c.output_file, NormalizedPath::new("foo.dll"));
                assert!(!c.non_deterministic);
            }
            other => panic!("expected cacheable, got: {other:?}"),
        }
    }

    #[test]
    fn msvc_no_args() {
        let result = parse_linker_invocation("link.exe", args(&[]));
        assert!(matches!(
            result,
            ParsedLinkerInvocation::NonCacheable { .. }
        ));
    }

    // â”€â”€â”€ Unknown tool â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€

    #[test]
    fn unknown_tool_non_cacheable() {
        let result = parse_linker_invocation("rustc", args(&["-shared", "-o", "libfoo.so", "a.o"]));
        assert!(matches!(
            result,
            ParsedLinkerInvocation::NonCacheable { .. }
        ));
    }

    // â”€â”€â”€ Cross-compile linker â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€

    #[test]
    fn cross_compile_ld() {
        let result = parse_linker_invocation(
            "x86_64-linux-gnu-ld",
            args(&["-shared", "-o", "libfoo.so", "a.o"]),
        );
        match result {
            ParsedLinkerInvocation::Cacheable(c) => {
                assert_eq!(c.family, LinkerFamily::Ld);
                assert_eq!(c.tool, NormalizedPath::new("x86_64-linux-gnu-ld"));
            }
            other => panic!("expected cacheable, got: {other:?}"),
        }
    }

    // â”€â”€â”€ --output= syntax â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€

    #[test]
    fn output_equals_syntax() {
        let result = parse_linker_invocation("ld", args(&["-shared", "--output=libfoo.so", "a.o"]));
        match result {
            ParsedLinkerInvocation::Cacheable(c) => {
                assert_eq!(c.output_file, NormalizedPath::new("libfoo.so"));
            }
            other => panic!("expected cacheable, got: {other:?}"),
        }
    }

    // â”€â”€â”€ Edge cases: -z flags, -rpath, mixed inputs â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€

    #[test]
    fn z_relro_and_now_flags() {
        let result = parse_linker_invocation(
            "ld",
            args(&[
                "-shared",
                "-z",
                "relro",
                "-z",
                "now",
                "-o",
                "libfoo.so",
                "a.o",
            ]),
        );
        match result {
            ParsedLinkerInvocation::Cacheable(c) => {
                assert!(c.cache_relevant_flags.contains(&"-z".to_string()));
                assert!(c.cache_relevant_flags.contains(&"relro".to_string()));
                assert!(c.cache_relevant_flags.contains(&"now".to_string()));
            }
            other => panic!("expected cacheable, got: {other:?}"),
        }
    }

    #[test]
    fn rpath_flag() {
        let result = parse_linker_invocation(
            "ld",
            args(&["-shared", "-rpath", "/usr/lib", "-o", "libfoo.so", "a.o"]),
        );
        match result {
            ParsedLinkerInvocation::Cacheable(c) => {
                // -rpath is consumed as a generic flag
                assert!(c.cache_relevant_flags.contains(&"-rpath".to_string()));
            }
            other => panic!("expected cacheable, got: {other:?}"),
        }
    }

    #[test]
    fn mixed_object_and_archive_inputs() {
        let result = parse_linker_invocation(
            "ld",
            args(&["-shared", "-o", "libfoo.so", "a.o", "libbar.a", "c.o"]),
        );
        match result {
            ParsedLinkerInvocation::Cacheable(c) => {
                assert_eq!(c.input_files.len(), 3);
                assert_eq!(c.input_files[0], NormalizedPath::new("a.o"));
                assert_eq!(c.input_files[1], NormalizedPath::new("libbar.a"));
                assert_eq!(c.input_files[2], NormalizedPath::new("c.o"));
            }
            other => panic!("expected cacheable, got: {other:?}"),
        }
    }

    #[test]
    fn soname_equals_syntax() {
        let result = parse_linker_invocation(
            "ld",
            args(&[
                "-shared",
                "--soname=libfoo.so.1",
                "-o",
                "libfoo.so.1.0",
                "a.o",
            ]),
        );
        match result {
            ParsedLinkerInvocation::Cacheable(c) => {
                assert!(c
                    .cache_relevant_flags
                    .contains(&"--soname=libfoo.so.1".to_string()));
            }
            other => panic!("expected cacheable, got: {other:?}"),
        }
    }

    #[test]
    fn version_script_separate_args() {
        let result = parse_linker_invocation(
            "ld",
            args(&[
                "-shared",
                "--version-script",
                "libfoo.map",
                "-o",
                "libfoo.so",
                "a.o",
            ]),
        );
        match result {
            ParsedLinkerInvocation::Cacheable(c) => {
                assert!(c.input_files.contains(&NormalizedPath::new("libfoo.map")));
                assert!(c.input_files.contains(&NormalizedPath::new("a.o")));
            }
            other => panic!("expected cacheable, got: {other:?}"),
        }
    }

    #[test]
    fn redundant_shared_flags() {
        // Multiple -shared flags are valid and shouldn't cause issues
        let result = parse_linker_invocation(
            "ld",
            args(&["-shared", "--shared", "-o", "libfoo.so", "a.o"]),
        );
        match result {
            ParsedLinkerInvocation::Cacheable(c) => {
                assert_eq!(c.output_file, NormalizedPath::new("libfoo.so"));
            }
            other => panic!("expected cacheable, got: {other:?}"),
        }
    }

    #[test]
    fn wl_shared_inside_pass_through() {
        // -Wl,-shared inside a -Wl, pass-through should detect shared mode
        let result =
            parse_linker_invocation("ld", args(&["-Wl,-shared", "-o", "libfoo.so", "a.o"]));
        match result {
            ParsedLinkerInvocation::Cacheable(c) => {
                assert_eq!(c.output_file, NormalizedPath::new("libfoo.so"));
            }
            other => panic!("expected cacheable, got: {other:?}"),
        }
    }

    #[test]
    fn msvc_def_file_as_flag() {
        let result = parse_linker_invocation(
            "link.exe",
            args(&["/DLL", "/DEF:foo.def", "/OUT:foo.dll", "a.obj"]),
        );
        match result {
            ParsedLinkerInvocation::Cacheable(c) => {
                assert!(c.cache_relevant_flags.contains(&"/DEF:foo.def".to_string()));
            }
            other => panic!("expected cacheable, got: {other:?}"),
        }
    }

    #[test]
    fn msvc_case_insensitive_dll_flag() {
        let result = parse_linker_invocation("link.exe", args(&["/dll", "/out:foo.dll", "a.obj"]));
        match result {
            ParsedLinkerInvocation::Cacheable(c) => {
                assert_eq!(c.output_file, NormalizedPath::new("foo.dll"));
            }
            other => panic!("expected cacheable, got: {other:?}"),
        }
    }

    // â”€â”€â”€ Compiler driver as linker (gcc -shared, clang -shared) â”€â”€â”€â”€â”€â”€â”€

    #[test]
    fn compiler_driver_detection() {
        assert!(is_compiler_driver("gcc"));
        assert!(is_compiler_driver("g++"));
        assert!(is_compiler_driver("clang"));
        assert!(is_compiler_driver("clang++"));
        assert!(is_compiler_driver("clang-17"));
        assert!(is_compiler_driver("cc"));
        assert!(is_compiler_driver("c++"));
        assert!(is_compiler_driver("/usr/bin/gcc"));
        assert!(is_compiler_driver("x86_64-w64-mingw32-gcc"));
        assert!(is_compiler_driver("x86_64-w64-mingw32-g++"));
        assert!(is_compiler_driver("emcc"));
        assert!(is_compiler_driver("em++"));
        assert!(is_compiler_driver("/usr/bin/emcc"));
        assert!(!is_compiler_driver("ld"));
        assert!(!is_compiler_driver("ar"));
        assert!(!is_compiler_driver("rustc"));
    }

    #[test]
    fn is_link_invocation_emcc() {
        // emcc without -c is a link invocation (compiler driver linking)
        assert!(is_link_invocation(
            "emcc",
            &args(&["-o", "output.js", "a.o", "b.o"])
        ));
        assert!(is_link_invocation(
            "em++",
            &args(&["-o", "output.html", "main.o"])
        ));
        // emcc with -c is NOT a link invocation (compile only)
        assert!(!is_link_invocation(
            "emcc",
            &args(&["-c", "foo.c", "-o", "foo.o"])
        ));
    }

    #[test]
    fn wasm_ld_cacheable() {
        let result = parse_linker_invocation("wasm-ld", args(&["-o", "output.wasm", "a.o", "b.o"]));
        match result {
            ParsedLinkerInvocation::Cacheable(c) => {
                assert_eq!(c.family, LinkerFamily::Lld);
                assert_eq!(c.output_file, NormalizedPath::new("output.wasm"));
                assert_eq!(c.input_files.len(), 2);
            }
            other => panic!("expected cacheable, got: {other:?}"),
        }
    }

    #[test]
    fn emcc_link_cacheable() {
        let result = parse_linker_invocation("emcc", args(&["-o", "output.js", "a.o", "b.o"]));
        match result {
            ParsedLinkerInvocation::Cacheable(c) => {
                assert_eq!(c.family, LinkerFamily::CompilerDriver);
                assert_eq!(c.output_file, NormalizedPath::new("output.js"));
                assert_eq!(c.input_files.len(), 2);
            }
            other => panic!("expected cacheable, got: {other:?}"),
        }
    }

    #[test]
    fn gcc_shared_basic() {
        let result =
            parse_linker_invocation("gcc", args(&["-shared", "-o", "libfoo.so", "a.o", "b.o"]));
        match result {
            ParsedLinkerInvocation::Cacheable(c) => {
                assert_eq!(c.family, LinkerFamily::CompilerDriver);
                assert_eq!(c.output_file, NormalizedPath::new("libfoo.so"));
                assert_eq!(c.input_files.len(), 2);
                assert!(!c.non_deterministic);
            }
            other => panic!("expected cacheable, got: {other:?}"),
        }
    }

    #[test]
    fn clang_shared_dll() {
        let result =
            parse_linker_invocation("clang", args(&["-shared", "-o", "foo.dll", "a.o", "b.o"]));
        match result {
            ParsedLinkerInvocation::Cacheable(c) => {
                assert_eq!(c.family, LinkerFamily::CompilerDriver);
                assert_eq!(c.output_file, NormalizedPath::new("foo.dll"));
            }
            other => panic!("expected cacheable, got: {other:?}"),
        }
    }

    #[test]
    fn gpp_shared_with_flags() {
        let result = parse_linker_invocation(
            "g++",
            args(&["-shared", "-fPIC", "-O2", "-o", "libfoo.so", "a.o", "-lm"]),
        );
        match result {
            ParsedLinkerInvocation::Cacheable(c) => {
                assert_eq!(c.input_files, vec![NormalizedPath::new("a.o")]);
                assert!(c.cache_relevant_flags.contains(&"-fPIC".to_string()));
                assert!(c.cache_relevant_flags.contains(&"-O2".to_string()));
                assert!(c.cache_relevant_flags.contains(&"-lm".to_string()));
            }
            other => panic!("expected cacheable, got: {other:?}"),
        }
    }

    #[test]
    fn gcc_shared_wl_build_id_uuid_non_deterministic() {
        let result = parse_linker_invocation(
            "gcc",
            args(&["-shared", "-Wl,--build-id=uuid", "-o", "libfoo.so", "a.o"]),
        );
        match result {
            ParsedLinkerInvocation::Cacheable(c) => {
                assert!(c.non_deterministic, "-Wl,--build-id=uuid must be flagged");
            }
            other => panic!("expected cacheable, got: {other:?}"),
        }
    }

    #[test]
    fn gcc_with_compile_flag_non_cacheable() {
        // gcc -c -shared means compile only (not link), should not be cached as link
        let result =
            parse_linker_invocation("gcc", args(&["-c", "-shared", "-o", "foo.o", "foo.c"]));
        assert!(matches!(
            result,
            ParsedLinkerInvocation::NonCacheable { .. }
        ));
    }

    #[test]
    fn gcc_exe_cacheable() {
        // gcc without -shared is executable linking â€” cacheable
        let result = parse_linker_invocation("gcc", args(&["-o", "a.out", "main.o"]));
        match result {
            ParsedLinkerInvocation::Cacheable(c) => {
                assert_eq!(c.family, LinkerFamily::CompilerDriver);
                assert_eq!(c.output_file, NormalizedPath::new("a.out"));
                assert_eq!(c.input_files, vec![NormalizedPath::new("main.o")]);
                assert!(!c.non_deterministic);
            }
            other => panic!("expected cacheable, got: {other:?}"),
        }
    }

    #[test]
    fn gcc_shared_no_output_non_cacheable() {
        let result = parse_linker_invocation("gcc", args(&["-shared", "a.o"]));
        assert!(matches!(
            result,
            ParsedLinkerInvocation::NonCacheable { .. }
        ));
    }

    #[test]
    fn gcc_shared_no_object_inputs_non_cacheable() {
        // Source files (.c) are not valid linker inputs â€” need pre-compiled .o
        let result = parse_linker_invocation("gcc", args(&["-shared", "-o", "libfoo.so", "foo.c"]));
        assert!(matches!(
            result,
            ParsedLinkerInvocation::NonCacheable { .. }
        ));
    }

    #[test]
    fn cross_compile_gcc() {
        let result = parse_linker_invocation(
            "x86_64-w64-mingw32-gcc",
            args(&["-shared", "-o", "foo.dll", "a.o"]),
        );
        match result {
            ParsedLinkerInvocation::Cacheable(c) => {
                assert_eq!(c.family, LinkerFamily::CompilerDriver);
                assert_eq!(c.tool, NormalizedPath::new("x86_64-w64-mingw32-gcc"));
            }
            other => panic!("expected cacheable, got: {other:?}"),
        }
    }

    #[test]
    fn gcc_shared_with_wl_soname() {
        let result = parse_linker_invocation(
            "gcc",
            args(&[
                "-shared",
                "-Wl,-soname,libfoo.so.1",
                "-o",
                "libfoo.so.1.0",
                "a.o",
            ]),
        );
        match result {
            ParsedLinkerInvocation::Cacheable(c) => {
                assert!(c
                    .cache_relevant_flags
                    .contains(&"-Wl,-soname,libfoo.so.1".to_string()));
            }
            other => panic!("expected cacheable, got: {other:?}"),
        }
    }

    // â”€â”€â”€ is_link_invocation (combined tool + args check) â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€

    #[test]
    fn is_link_invocation_direct_linker() {
        // Direct linkers are always link invocations (args don't matter for detection)
        assert!(is_link_invocation(
            "ld",
            &args(&["-shared", "-o", "foo.so", "a.o"])
        ));
        assert!(is_link_invocation("ld", &args(&["-o", "a.out", "main.o"])));
        assert!(is_link_invocation(
            "link.exe",
            &args(&["/DLL", "/OUT:foo.dll", "a.obj"])
        ));
    }

    #[test]
    fn is_link_invocation_compiler_driver_shared() {
        assert!(is_link_invocation(
            "gcc",
            &args(&["-shared", "-o", "foo.so", "a.o"])
        ));
        assert!(is_link_invocation(
            "clang++",
            &args(&["-shared", "-o", "foo.so", "a.o"])
        ));
    }

    #[test]
    fn is_link_invocation_compiler_not_shared() {
        // gcc -c is compilation, NOT a link invocation
        assert!(!is_link_invocation(
            "gcc",
            &args(&["-c", "foo.c", "-o", "foo.o"])
        ));
        // gcc -E is preprocessing, NOT a link invocation
        assert!(!is_link_invocation("gcc", &args(&["-E", "foo.c"])));
        // gcc -S is assembly generation, NOT a link invocation
        assert!(!is_link_invocation("gcc", &args(&["-S", "foo.c"])));
        // gcc -o a.out main.o IS a link invocation (exe link)
        assert!(is_link_invocation("gcc", &args(&["-o", "a.out", "main.o"])));
    }

    #[test]
    fn is_link_invocation_pch_generation_not_link() {
        // PCH generation with -x c++-header is compilation, NOT linking
        assert!(!is_link_invocation(
            "clang++",
            &args(&["-x", "c++-header", "header.h", "-o", "header.pch"])
        ));
        assert!(!is_link_invocation(
            "gcc",
            &args(&["-x", "c-header", "stdafx.h", "-o", "stdafx.h.gch"])
        ));
        // Cross-compiler with "clang" in the name
        assert!(!is_link_invocation(
            "ctc-clang++",
            &args(&[
                "-x",
                "c++-header",
                "FastLED.h",
                "-o",
                "FastLED.h.pch",
                "-fPIC",
                "-Iinclude",
            ])
        ));
        // With -c AND -x c++-header â€” still not a link
        assert!(!is_link_invocation(
            "clang++",
            &args(&["-x", "c++-header", "-c", "header.h", "-o", "header.pch"])
        ));
    }

    #[test]
    fn is_link_header_and_module_modes_not_link() {
        // All `-x` language modes that imply compilation should NOT be link invocations.
        // Header (PCH):
        assert!(!is_link_invocation(
            "clang++",
            &args(&["-x", "c-header", "foo.h", "-o", "foo.gch"])
        ));
        assert!(!is_link_invocation(
            "clang++",
            &args(&["-x", "c++-header", "foo.h", "-o", "foo.pch"])
        ));
        // Header unit (C++20):
        assert!(!is_link_invocation(
            "clang++",
            &args(&["-x", "c-header-unit", "foo.h", "-o", "foo.pcm"])
        ));
        assert!(!is_link_invocation(
            "clang++",
            &args(&["-x", "c++-header-unit", "foo.h", "-o", "foo.pcm"])
        ));
        // Module mode does NOT imply compilation â€” without -c/--precompile, it's still a link.
        assert!(is_link_invocation(
            "clang++",
            &args(&["-x", "c++-module", "interface.cpp", "-o", "interface"])
        ));
        // --precompile is also not a link invocation:
        assert!(!is_link_invocation(
            "clang++",
            &args(&["--precompile", "module.cppm", "-o", "module.pcm"])
        ));
    }

    #[test]
    fn is_link_invocation_unknown_tool() {
        assert!(!is_link_invocation(
            "rustc",
            &args(&["-shared", "-o", "foo.so"])
        ));
    }

    #[test]
    fn is_link_invocation_c_flag_in_response_file() {
        // When -c is inside a response file (e.g. fbuild on Windows puts all
        // flags in @response.rsp), is_link_invocation must expand the response
        // file to find the -c flag and correctly classify it as compilation.
        use std::io::Write;
        let mut rsp = tempfile::NamedTempFile::new().unwrap();
        writeln!(rsp, "-O2 -Wall -c foo.cpp -o foo.o").unwrap();

        let rsp_arg = format!("@{}", rsp.path().display());
        // Without response file expansion, this would incorrectly return true
        assert!(
            !is_link_invocation("gcc", &args(&[&rsp_arg])),
            "-c inside response file must be detected as compilation, not link"
        );
        assert!(
            !is_link_invocation("xtensa-esp32s3-elf-g++", &args(&[&rsp_arg])),
            "xtensa cross-compiler with -c in response file must not be classified as link"
        );
    }

    #[test]
    fn is_link_invocation_response_file_without_c_flag() {
        // A response file that contains link flags (no -c) should still be link
        use std::io::Write;
        let mut rsp = tempfile::NamedTempFile::new().unwrap();
        writeln!(rsp, "-O2 -o a.out main.o").unwrap();

        let rsp_arg = format!("@{}", rsp.path().display());
        assert!(
            is_link_invocation("gcc", &args(&[&rsp_arg])),
            "response file without -c should be classified as link"
        );
    }

    // â”€â”€â”€ GNU/LLD --out-implib secondary output â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€

    #[test]
    fn gnu_ld_out_implib_equals() {
        let result = parse_linker_invocation(
            "ld",
            args(&[
                "-shared",
                "--out-implib=libfoo.dll.a",
                "-o",
                "libfoo.dll",
                "a.o",
            ]),
        );
        match result {
            ParsedLinkerInvocation::Cacheable(c) => {
                assert_eq!(c.secondary_outputs.len(), 1);
                assert_eq!(c.secondary_outputs[0], NormalizedPath::new("libfoo.dll.a"));
            }
            other => panic!("expected cacheable, got: {other:?}"),
        }
    }

    #[test]
    fn gnu_ld_out_implib_separate() {
        let result = parse_linker_invocation(
            "ld",
            args(&[
                "-shared",
                "--out-implib",
                "libfoo.dll.a",
                "-o",
                "libfoo.dll",
                "a.o",
            ]),
        );
        match result {
            ParsedLinkerInvocation::Cacheable(c) => {
                assert_eq!(c.secondary_outputs.len(), 1);
                assert_eq!(c.secondary_outputs[0], NormalizedPath::new("libfoo.dll.a"));
            }
            other => panic!("expected cacheable, got: {other:?}"),
        }
    }

    #[test]
    fn gnu_ld_out_implib_with_path() {
        // Meson passes relative paths like ci/meson/native\fastled.dll.a
        let result = parse_linker_invocation(
            "ld",
            args(&[
                "-shared",
                "--out-implib=ci/meson/native/fastled.dll.a",
                "-o",
                "ci/meson/native/fastled.dll",
                "a.o",
            ]),
        );
        match result {
            ParsedLinkerInvocation::Cacheable(c) => {
                assert_eq!(c.secondary_outputs.len(), 1);
                assert_eq!(
                    c.secondary_outputs[0],
                    NormalizedPath::new("ci/meson/native/fastled.dll.a")
                );
            }
            other => panic!("expected cacheable, got: {other:?}"),
        }
    }

    #[test]
    fn compiler_driver_wl_out_implib() {
        // clang++ -shared -Wl,--out-implib=foo.dll.a -o foo.dll a.o
        let result = parse_linker_invocation(
            "clang++",
            args(&[
                "-shared",
                "-Wl,--out-implib=foo.dll.a",
                "-o",
                "foo.dll",
                "a.o",
            ]),
        );
        match result {
            ParsedLinkerInvocation::Cacheable(c) => {
                assert_eq!(c.family, LinkerFamily::CompilerDriver);
                assert_eq!(c.secondary_outputs.len(), 1);
                assert_eq!(c.secondary_outputs[0], NormalizedPath::new("foo.dll.a"));
            }
            other => panic!("expected cacheable, got: {other:?}"),
        }
    }

    #[test]
    fn compiler_driver_wl_out_implib_with_path() {
        // Real-world meson invocation:
        // clang++ -shared -Wl,--out-implib=ci/meson/native\fastled.dll.a -o fastled.dll a.o
        let result = parse_linker_invocation(
            "clang++",
            args(&[
                "-shared",
                "-Wl,--start-group",
                "-Wl,--out-implib=ci/meson/native\\fastled.dll.a",
                "-fuse-ld=lld",
                "-o",
                "ci/meson/native/fastled.dll",
                "a.o",
            ]),
        );
        match result {
            ParsedLinkerInvocation::Cacheable(c) => {
                assert_eq!(c.secondary_outputs.len(), 1);
                assert_eq!(
                    c.secondary_outputs[0],
                    NormalizedPath::new("ci/meson/native\\fastled.dll.a")
                );
            }
            other => panic!("expected cacheable, got: {other:?}"),
        }
    }

    #[test]
    fn compiler_driver_no_implib_no_secondary() {
        // Without --out-implib, no secondary outputs
        let result = parse_linker_invocation("clang++", args(&["-shared", "-o", "foo.dll", "a.o"]));
        match result {
            ParsedLinkerInvocation::Cacheable(c) => {
                assert!(c.secondary_outputs.is_empty());
            }
            other => panic!("expected cacheable, got: {other:?}"),
        }
    }
}
