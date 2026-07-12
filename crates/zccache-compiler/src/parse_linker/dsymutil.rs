//! LLVM/Apple `dsymutil` directory-bundle parser.

use super::types::{CacheableLink, LinkOutputKind, LinkerFamily, ParsedLinkerInvocation};
use zccache_core::NormalizedPath;

pub(super) fn parse_dsymutil(tool: &str, args: Vec<String>) -> ParsedLinkerInvocation {
    if args.iter().any(|arg| {
        matches!(
            arg.as_str(),
            "--codesign"
                | "--flat"
                | "-f"
                | "--no-output"
                | "--dump-debug-map"
                | "--symtab"
                | "-s"
                | "-S"
                | "--update"
                | "-u"
                | "--gen-reproducer"
        ) || arg.starts_with("--codesign=")
            || arg.starts_with("--reproducer")
            || arg.starts_with("--use-reproducer")
            || arg.starts_with("--embed-resource")
    }) {
        return ParsedLinkerInvocation::NonCacheable {
            reason: "dsymutil mode requires signed, mutable, flat, or opaque outputs".to_string(),
        };
    }

    let mut input_files = Vec::new();
    let mut output_file = None;
    let mut cache_relevant_flags = Vec::new();
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        if matches!(arg.as_str(), "-o" | "--out") {
            index += 1;
            let Some(path) = args.get(index) else {
                return ParsedLinkerInvocation::NonCacheable {
                    reason: "dsymutil output option is missing its path".to_string(),
                };
            };
            output_file = Some(NormalizedPath::new(path));
            index += 1;
            continue;
        }
        if let Some(path) = arg.strip_prefix("--out=") {
            output_file = Some(NormalizedPath::new(path));
            index += 1;
            continue;
        }
        if option_takes_value(arg) {
            cache_relevant_flags.push(arg.clone());
            index += 1;
            let Some(value) = args.get(index) else {
                return ParsedLinkerInvocation::NonCacheable {
                    reason: format!("dsymutil option {arg} is missing its value"),
                };
            };
            cache_relevant_flags.push(value.clone());
            index += 1;
            continue;
        }
        if arg.starts_with('-') {
            cache_relevant_flags.push(arg.clone());
        } else {
            input_files.push(NormalizedPath::new(arg));
        }
        index += 1;
    }

    if input_files.len() != 1 {
        return ParsedLinkerInvocation::NonCacheable {
            reason: "dsymutil caching requires exactly one input image".to_string(),
        };
    }
    let output_file = output_file.unwrap_or_else(|| {
        NormalizedPath::new(format!("{}.dSYM", input_files[0].to_string_lossy()))
    });
    ParsedLinkerInvocation::Cacheable(CacheableLink {
        tool: NormalizedPath::new(tool),
        family: LinkerFamily::Dsymutil,
        input_files,
        output_file,
        output_kind: LinkOutputKind::DirectoryBundle,
        secondary_outputs: Vec::new(),
        cache_relevant_flags,
        original_args: args,
        non_deterministic: false,
    })
}

fn option_takes_value(option: &str) -> bool {
    matches!(
        option,
        "--accelerator"
            | "--allow"
            | "--arch"
            | "-arch"
            | "--build-variant-suffix"
            | "-D"
            | "--disallow"
            | "-j"
            | "--num-threads"
            | "--object-prefix-map"
            | "--oso-prepend-path"
            | "--remarks-output-format"
            | "--remarks-prepend-path"
            | "--toolchain"
    )
}
