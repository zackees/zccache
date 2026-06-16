//! GNU `ld` / LLVM `lld` argument parser.

use super::types::{CacheableLink, LinkerFamily, ParsedLinkerInvocation};
use crate::core::NormalizedPath;

/// Parse GNU ld / lld arguments for linking.
///
/// Both shared library (`-shared` / `-dylib`) and executable linking are cacheable.
/// `-shared` / `-dylib` are kept as cache-relevant flags since they affect output.
pub(super) fn parse_gnu_ld(
    tool: &str,
    family: LinkerFamily,
    args: Vec<String>,
) -> ParsedLinkerInvocation {
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

        // --out-implib=<path> — GNU/LLD import library (secondary output on Windows)
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

        // -shared or --shared — shared library mode (cache-relevant: affects output type)
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

        // --build-id=uuid → non-deterministic
        if arg == "--build-id=uuid" {
            has_build_id_uuid = true;
            cache_relevant_flags.push(arg.clone());
            i += 1;
            continue;
        }

        // --build-id=<style> (sha1, md5, none, etc.) → deterministic
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

        // -L<path> or -L <path> — library search path (cache-relevant)
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

        // -l<lib> — library dependency (cache-relevant, order matters)
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

        // Positional argument — input file (object file or library)
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
