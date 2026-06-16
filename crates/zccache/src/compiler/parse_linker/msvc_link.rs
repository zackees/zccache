//! MSVC `link.exe` argument parser.

use super::types::{CacheableLink, LinkerFamily, ParsedLinkerInvocation};
use crate::core::NormalizedPath;

/// Parse MSVC link.exe arguments for linking (DLL or executable).
///
/// Both `/DLL` (DLL) and non-`/DLL` (executable) invocations are cacheable.
/// `/DLL` is kept as a cache-relevant flag since it affects output type.
pub(super) fn parse_msvc_link(tool: &str, args: Vec<String>) -> ParsedLinkerInvocation {
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

        // /DLL — DLL mode (cache-relevant: affects output type)
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

        // /IMPLIB:filename — import library (secondary output)
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

        // Positional — input file
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
