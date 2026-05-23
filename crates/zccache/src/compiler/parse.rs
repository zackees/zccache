//! Clang/GCC/MSVC compiler-invocation parsing.
//!
//! This module owns the parsing entry point for non-rustc compilers.
//! For rustc, see [`crate::parse_rustc`].

use std::sync::Arc;
use zccache::core::NormalizedPath;

use super::detect::{detect_family, is_source_file, MODULE_EXTENSIONS};
use super::{parse_msvc, CacheableCompilation, CompilerFamily, ParsedInvocation, SourceMode};

/// Map a `-x <lang>` value to the corresponding source mode.
/// Returns `None` for unrecognized language values (no special mode).
pub(crate) fn source_mode_from_language(lang: &str) -> Option<SourceMode> {
    match lang {
        "c-header" | "c++-header" => Some(SourceMode::Header),
        "c-header-unit" | "c++-header-unit" => Some(SourceMode::HeaderUnit),
        "c++-module" => Some(SourceMode::Module),
        _ => None,
    }
}

/// Determine the source mode implied by a file extension.
pub(crate) fn source_mode_from_extension(path: &str) -> SourceMode {
    if let Some(ext) = std::path::Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
    {
        if MODULE_EXTENSIONS.contains(&ext) {
            return SourceMode::Module;
        }
    }
    SourceMode::Normal
}

/// Compute the default output path when `-o` is absent.
///
/// For both normal compilation and PCH generation, the output is placed in
/// the current working directory using just the filename — never preserving
/// directory components from the source path.
///
/// Normal compilation: `src/foo.cpp` → `foo.o`
/// PCH generation:     `src/pch.h`   → `pch.h.pch`  (clang)
///                     `src/pch.h`   → `pch.h.gch`  (gcc)
///
/// Note: real compilers place PCH output next to the source file
/// (`src/pch.h` → `src/pch.h.pch`), but zccache intentionally uses only
/// the filename. This prevents spurious `.pch` files from being written
/// into the source tree when a compilation falls back to `default_output`
/// (e.g., during cache restoration without an explicit `-o` flag).
pub(crate) fn default_output(
    source: &str,
    family: CompilerFamily,
    mode: SourceMode,
    has_precompile: bool,
) -> String {
    match mode {
        SourceMode::Header => {
            // PCH: filename.pch (Clang) or filename.gch (GCC)
            if let Some(ext) = family.pch_extension() {
                let filename = std::path::Path::new(source)
                    .file_name()
                    .and_then(|f| f.to_str())
                    .unwrap_or(source);
                return format!("{filename}.{ext}");
            }
        }
        SourceMode::HeaderUnit => {
            // Header unit: filename.pcm
            let filename = std::path::Path::new(source)
                .file_name()
                .and_then(|f| f.to_str())
                .unwrap_or(source);
            return format!("{filename}.pcm");
        }
        SourceMode::Module | SourceMode::Normal => {
            if has_precompile {
                // --precompile: stem.pcm
                let stem = std::path::Path::new(source)
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("a");
                return format!("{stem}.pcm");
            }
        }
    }
    // Default: stem.o
    let stem = std::path::Path::new(source)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("a");
    format!("{stem}.o")
}

/// Flags that take a following argument (value in next argv element).
const FLAGS_WITH_VALUE: &[&str] = &[
    "-o",
    "-D",
    "-U",
    "-I",
    "-isystem",
    "-iquote",
    "-idirafter",
    "-include",
    "-include-pch",
    "-isysroot",
    "-target",
    "--target",
    "-MF",
    "-MQ",
    "-MT",
    "-std",
    "-x",
    "-arch",
    "-Xclang",
    "-mllvm",
    "--serialize-diagnostics",
];

/// Parse a compiler invocation's arguments to determine cacheability.
///
/// Returns a `ParsedInvocation` indicating whether the invocation is
/// cacheable, and if so, extracts the relevant information.
///
/// Arg parsing is read-only analysis — it never modifies what goes to
/// the compiler. The compiler always receives the exact original args.
#[must_use]
pub fn parse_invocation(compiler: &str, args: &[String]) -> ParsedInvocation {
    let family = detect_family(compiler);
    // Rustfmt is not a compiler — reject here, CLI handles it separately.
    if family == CompilerFamily::Rustfmt {
        return ParsedInvocation::NonCacheable {
            reason: "rustfmt is handled via the format cache path, not compile cache".to_string(),
        };
    }
    // Rustc has a completely different invocation model — dispatch early.
    if family == CompilerFamily::Rustc {
        return super::parse_rustc::parse_rustc_invocation(compiler, args);
    }

    // MSVC / clang-cl use Windows-style slash flags (`/c`, `/Fo:foo.obj`).
    // Dispatch to the MSVC classifier on:
    //   1. Detected MSVC family (cl.exe, clang-cl.exe), OR
    //   2. Any compiler whose argv contains MSVC-style flags. This catches
    //      the case where a build system invokes `clang` (not `clang-cl`)
    //      but passes `/c` and `/Fo`, which still triggers MSVC mode in
    //      clang. Without this dispatch the GCC parser would silently drop
    //      `/c` and the invocation would be misclassified as a link step.
    //      Issue #261.
    if family == CompilerFamily::Msvc || parse_msvc::looks_like_msvc_args(args) {
        return parse_msvc::parse_msvc_invocation(compiler, args, family);
    }

    let mut has_c_flag = false;
    let mut has_precompile_flag = false;
    let mut source_files: Vec<(String, usize, SourceMode)> = Vec::new();
    let mut output_file: Option<String> = None;
    let mut current_mode = SourceMode::Normal;
    let mut unknown_flags: Vec<String> = Vec::new();

    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];

        // Check for non-cacheable flags
        if arg == "-E" || arg == "-M" || arg == "-MM" {
            return ParsedInvocation::NonCacheable {
                reason: format!("preprocessing-only flag: {arg}"),
            };
        }

        if arg == "-" {
            return ParsedInvocation::NonCacheable {
                reason: "stdin source not cacheable".to_string(),
            };
        }

        if arg == "-c" {
            has_c_flag = true;
            i += 1;
            continue;
        }

        // --precompile (Clang): compile module interface to BMI (.pcm).
        // Acts like -c but for module output.
        if arg == "--precompile" {
            has_precompile_flag = true;
            i += 1;
            continue;
        }

        // -o takes the next arg as output file, or -o<path> (concatenated)
        if arg == "-o" {
            if let Some(next) = args.get(i + 1) {
                output_file = Some(next.clone());
                i += 2;
            } else {
                i += 1;
            }
            continue;
        } else if let Some(path) = arg.strip_prefix("-o") {
            output_file = Some(path.to_string());
            i += 1;
            continue;
        }

        // Flags that take a value in the next arg — skip both flag and value
        if let Some(&flag) = FLAGS_WITH_VALUE.iter().find(|&&f| f == arg.as_str()) {
            if flag == "-x" && i + 1 < args.len() {
                current_mode =
                    source_mode_from_language(&args[i + 1]).unwrap_or(SourceMode::Normal);
            }
            i += 2;
            continue;
        }

        // Any flag starting with - (including unknown flags) — preserve
        if arg.starts_with('-') {
            unknown_flags.push(arg.clone());
            i += 1;
            continue;
        }

        // Positional arg — source file candidate.
        // In a special mode (Header/HeaderUnit/Module), any positional arg is a source.
        // Otherwise, check by file extension. Module extensions (.cppm/.ixx) also set
        // the effective mode to Module for correct default output.
        let effective_mode = if current_mode != SourceMode::Normal {
            current_mode
        } else {
            source_mode_from_extension(arg)
        };
        if is_source_file(arg) || current_mode != SourceMode::Normal {
            source_files.push((arg.clone(), i, effective_mode));
        }

        i += 1;
    }

    // Header and header-unit modes imply compilation (no -c needed).
    // Module mode does NOT imply compilation alone — requires -c or --precompile.
    // --precompile also implies compilation (like -c but for BMI output).
    if !has_c_flag && !has_precompile_flag && !current_mode.implies_compilation() {
        return ParsedInvocation::NonCacheable {
            reason: "no -c flag (likely a link invocation)".to_string(),
        };
    }

    if source_files.is_empty() {
        return ParsedInvocation::NonCacheable {
            reason: "no source file found".to_string(),
        };
    }

    let family = detect_family(compiler);

    // Multi-file: `-o` is invalid with `-c` and multiple sources (compiler rejects it),
    // so each source gets its default output name (stem.o).
    if source_files.len() > 1 {
        let source_indices: Vec<usize> = source_files.iter().map(|(_, idx, _)| *idx).collect();
        let shared_args: Arc<[String]> = Arc::from(args.to_vec());
        let compilations = source_files
            .iter()
            .map(|(src, _, mode)| CacheableCompilation {
                compiler: NormalizedPath::new(compiler),
                family,
                source_file: NormalizedPath::new(src),
                output_file: NormalizedPath::new(default_output(
                    src,
                    family,
                    *mode,
                    has_precompile_flag,
                )),
                original_args: Arc::clone(&shared_args),
                unknown_flags: unknown_flags.clone(),
            })
            .collect();
        return ParsedInvocation::MultiFile {
            compilations,
            original_args: shared_args,
            source_indices,
        };
    }

    // Single source file
    let (source, _, mode) = source_files.into_iter().next().unwrap();
    let output =
        output_file.unwrap_or_else(|| default_output(&source, family, mode, has_precompile_flag));

    ParsedInvocation::Cacheable(CacheableCompilation {
        compiler: NormalizedPath::new(compiler),
        family,
        source_file: NormalizedPath::new(source),
        output_file: NormalizedPath::new(output),
        original_args: Arc::from(args.to_vec()),
        unknown_flags,
    })
}
