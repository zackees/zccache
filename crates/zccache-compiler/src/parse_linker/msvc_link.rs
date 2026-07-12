//! MSVC `link.exe` argument parser.

use super::types::{CacheableLink, LinkOutputKind, LinkerFamily, ParsedLinkerInvocation};
use zccache_core::NormalizedPath;

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
    let mut has_debug = false;
    let mut incremental = None;
    let mut implicit_map = false;
    let mut explicit_pdb = false;
    let mut explicit_ilk = false;
    let mut debug_outputs = Vec::new();
    let mut incremental_outputs = Vec::new();
    let mut ltcg_incremental = false;
    let mut ltcg_output = None;
    let mut profile_generation = false;
    let mut profile_use = false;
    let mut profile_output = None;
    let mut winmd = false;
    let mut winmd_output = None;

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

        // Debug/incremental linker products. Explicit destinations are part
        // of the declared output set. Implicit names are added after /OUT is
        // known so the staging planner can reject the invocation before
        // spawn unless it can redirect every product.
        if upper == "/DEBUG:NONE" || upper == "-DEBUG:NONE" {
            has_debug = false;
            cache_relevant_flags.push(arg.clone());
            continue;
        }
        if upper == "/DEBUG"
            || upper == "-DEBUG"
            || upper.starts_with("/DEBUG:")
            || upper.starts_with("-DEBUG:")
        {
            has_debug = true;
            cache_relevant_flags.push(arg.clone());
            continue;
        }
        if upper == "/INCREMENTAL" || upper == "-INCREMENTAL" {
            incremental = Some(true);
            cache_relevant_flags.push(arg.clone());
            continue;
        }
        if upper == "/INCREMENTAL:NO" || upper == "-INCREMENTAL:NO" {
            incremental = Some(false);
            cache_relevant_flags.push(arg.clone());
            continue;
        }
        if upper == "/LTCG:INCREMENTAL" || upper == "-LTCG:INCREMENTAL" {
            ltcg_incremental = true;
            cache_relevant_flags.push(arg.clone());
            continue;
        }
        if upper.starts_with("/LTCGOUT:") || upper.starts_with("-LTCGOUT:") {
            ltcg_output = Some(NormalizedPath::new(&arg[9..]));
            cache_relevant_flags.push(arg.clone());
            continue;
        }
        if matches!(
            upper.as_str(),
            "/GENPROFILE" | "-GENPROFILE" | "/FASTGENPROFILE" | "-FASTGENPROFILE"
        ) {
            profile_generation = true;
            cache_relevant_flags.push(arg.clone());
            continue;
        }
        if upper == "/USEPROFILE"
            || upper == "-USEPROFILE"
            || upper.starts_with("/USEPROFILE:")
            || upper.starts_with("-USEPROFILE:")
        {
            profile_use = true;
            if let Some(pgd_offset) = upper.find("PGD=") {
                profile_output = Some(NormalizedPath::new(&arg[pgd_offset + 4..]));
            }
            cache_relevant_flags.push(arg.clone());
            continue;
        }
        if upper.starts_with("/PGD:") || upper.starts_with("-PGD:") {
            profile_output = Some(NormalizedPath::new(&arg[5..]));
            cache_relevant_flags.push(arg.clone());
            continue;
        }
        if upper == "/WINMD" || upper == "-WINMD" {
            winmd = true;
            cache_relevant_flags.push(arg.clone());
            continue;
        }
        if upper.starts_with("/WINMDFILE:") || upper.starts_with("-WINMDFILE:") {
            winmd_output = Some(NormalizedPath::new(&arg[11..]));
            cache_relevant_flags.push(arg.clone());
            continue;
        }
        if upper == "/MAP" || upper == "-MAP" {
            implicit_map = true;
            cache_relevant_flags.push(arg.clone());
            continue;
        }
        let explicit_output = [
            "/PDB:",
            "-PDB:",
            "/PDBSTRIPPED:",
            "-PDBSTRIPPED:",
            "/ILK:",
            "-ILK:",
            "/MAP:",
            "-MAP:",
        ]
        .into_iter()
        .find(|prefix| upper.starts_with(prefix));
        if let Some(prefix) = explicit_output {
            let path = &arg[prefix.len()..];
            if !path.is_empty() {
                let output = NormalizedPath::new(path);
                if prefix.eq_ignore_ascii_case("/PDB:")
                    || prefix.eq_ignore_ascii_case("-PDB:")
                    || prefix.eq_ignore_ascii_case("/PDBSTRIPPED:")
                    || prefix.eq_ignore_ascii_case("-PDBSTRIPPED:")
                {
                    debug_outputs.push(output);
                } else if prefix.eq_ignore_ascii_case("/ILK:")
                    || prefix.eq_ignore_ascii_case("-ILK:")
                {
                    incremental_outputs.push(output);
                } else {
                    secondary_outputs.push(output);
                }
            }
            explicit_pdb |=
                prefix.eq_ignore_ascii_case("/PDB:") || prefix.eq_ignore_ascii_case("-PDB:");
            explicit_ilk |=
                prefix.eq_ignore_ascii_case("/ILK:") || prefix.eq_ignore_ascii_case("-ILK:");
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
    if profile_generation && profile_use {
        return ParsedLinkerInvocation::NonCacheable {
            reason: "MSVC profile generation and profile use cannot share one staged plan"
                .to_string(),
        };
    }
    if profile_use {
        let Some(profile_input) = profile_output.as_ref() else {
            return ParsedLinkerInvocation::NonCacheable {
                reason: "MSVC /USEPROFILE requires an explicit PGD path for caching".to_string(),
            };
        };
        input_files.push(if profile_input.extension().is_some() {
            profile_input.clone()
        } else {
            NormalizedPath::new(profile_input.with_extension("pgd"))
        });
    }
    if has_debug {
        secondary_outputs.extend(debug_outputs);
    }
    let incremental_enabled =
        incremental == Some(true) || (has_debug && incremental != Some(false));
    if incremental_enabled {
        secondary_outputs.extend(incremental_outputs);
    }
    if has_debug && !explicit_pdb {
        secondary_outputs.push(NormalizedPath::new(output_file.with_extension("pdb")));
    }
    // /DEBUG changes LINK's default to incremental linking unless explicitly
    // disabled. Both /DEBUG's implicit case and explicit /INCREMENTAL create
    // an ILK whose default name follows the image output.
    if incremental_enabled && !explicit_ilk {
        secondary_outputs.push(NormalizedPath::new(output_file.with_extension("ilk")));
    }
    if implicit_map {
        secondary_outputs.push(NormalizedPath::new(output_file.with_extension("map")));
    }
    if ltcg_incremental {
        secondary_outputs.push(
            ltcg_output.unwrap_or_else(|| NormalizedPath::new(output_file.with_extension("iobj"))),
        );
    }
    if profile_generation {
        secondary_outputs.push(
            profile_output
                .unwrap_or_else(|| NormalizedPath::new(output_file.with_extension("pgd"))),
        );
    }
    if winmd {
        let output = winmd_output
            .unwrap_or_else(|| NormalizedPath::new(output_file.with_extension("winmd")));
        secondary_outputs.push(if output.extension().is_some() {
            output
        } else {
            NormalizedPath::new(output.with_extension("winmd"))
        });
    }
    let mut seen_outputs = std::collections::HashSet::new();
    secondary_outputs.retain(|output| seen_outputs.insert(output.clone()));

    ParsedLinkerInvocation::Cacheable(CacheableLink {
        tool: NormalizedPath::new(tool),
        family: LinkerFamily::MsvcLink,
        input_files,
        output_file,
        output_kind: LinkOutputKind::File,
        secondary_outputs,
        cache_relevant_flags,
        original_args: args,
        non_deterministic: !has_deterministic,
    })
}
