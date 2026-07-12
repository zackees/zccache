//! Compiler-driver-as-linker argument parser (e.g. `gcc -shared -o ...`).

use super::types::{CacheableLink, LinkOutputKind, LinkerFamily, ParsedLinkerInvocation};
use zccache_core::NormalizedPath;

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
pub(super) fn parse_compiler_driver_link(tool: &str, args: Vec<String>) -> ParsedLinkerInvocation {
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

        // -shared — shared library mode (cache-relevant: affects output type)
        if arg == "-shared" || arg == "--shared" {
            cache_relevant_flags.push(arg.clone());
            i += 1;
            continue;
        }

        // -c — compile only, NOT linking
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

        // -Wl, pass-through to linker — check for non-determinism and secondary outputs
        if arg.starts_with("-Wl,") {
            let parts = arg.split(',').collect::<Vec<_>>();
            for (index, part) in parts.iter().enumerate() {
                if *part == "--build-id=uuid" {
                    has_build_id_uuid = true;
                }
                // GNU/LLD --out-implib produces an import library (.dll.a) as a side effect.
                // Meson/ninja uses: -Wl,--out-implib=path/to/foo.dll.a
                if let Some(implib) = part.strip_prefix("--out-implib=") {
                    secondary_outputs.push(NormalizedPath::new(implib));
                }
                if let Some(map) = part.strip_prefix("-Map=") {
                    if map != "-" {
                        secondary_outputs.push(NormalizedPath::new(map));
                    }
                }
                if (*part == "-Map" || *part == "--dependency-file")
                    && parts.get(index + 1).is_some_and(|value| *value != "-")
                {
                    secondary_outputs.push(NormalizedPath::new(parts[index + 1]));
                }
                if let Some(depfile) = part.strip_prefix("--dependency-file=") {
                    if depfile != "-" {
                        secondary_outputs.push(NormalizedPath::new(depfile));
                    }
                }
                if (*part == "-map" || *part == "-dependency_info")
                    && parts.get(index + 1).is_some_and(|value| !value.is_empty())
                {
                    secondary_outputs.push(NormalizedPath::new(parts[index + 1]));
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

        // Positional argument — input file (object or source)
        if is_linker_input(arg) {
            input_files.push(NormalizedPath::new(arg));
        }
        // Ignore non-object positional args (e.g., source files passed to gcc
        // during combined compile-and-link — too complex to cache)
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

    let mut seen_outputs = std::collections::HashSet::new();
    secondary_outputs.retain(|output| seen_outputs.insert(output.clone()));

    ParsedLinkerInvocation::Cacheable(CacheableLink {
        tool: NormalizedPath::new(tool),
        family: LinkerFamily::CompilerDriver,
        input_files,
        output_file,
        output_kind: LinkOutputKind::File,
        secondary_outputs,
        cache_relevant_flags,
        original_args: args,
        non_deterministic: has_build_id_uuid,
    })
}
