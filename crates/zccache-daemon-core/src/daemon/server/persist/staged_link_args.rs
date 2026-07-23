//! Argument analysis and rewriting helpers for private linker staging plans.

use super::staged_plan::StagedOutputPlan;
use crate::core::path::NormalizedPath;
use std::path::Path;

/// Rewrite one exact or delimiter-bounded linker output path. Returning
/// `None` means the token was ambiguous and the caller must use the legacy
/// path before spawning the linker.
pub(super) fn rewrite_link_output_arg<'a>(
    arg: &mut String,
    candidates: impl Iterator<Item = &'a str>,
    staged: &str,
) -> Option<bool> {
    let mut ranges = Vec::new();
    for candidate in candidates.filter(|candidate| !candidate.is_empty()) {
        if arg == candidate {
            ranges.push((0, arg.len()));
            continue;
        }
        for (start, _) in arg.match_indices(candidate) {
            let end = start + candidate.len();
            let before = arg[..start].chars().next_back();
            let after = arg[end..].chars().next();
            if before.is_some_and(|ch| matches!(ch, '=' | ':' | ','))
                && after.is_none_or(|ch| ch == ',')
            {
                ranges.push((start, end));
            }
        }
    }
    ranges.sort_unstable();
    ranges.dedup();
    match ranges.as_slice() {
        [] => Some(false),
        &[(start, end)] => {
            arg.replace_range(start..end, staged);
            Some(true)
        }
        _ => None,
    }
}

pub(super) fn has_unmodeled_link_output_option(args: &[String]) -> bool {
    args.iter().any(|arg| {
        let lower = arg.to_ascii_lowercase();
        [
            "/idlout:", "-idlout:", "/tlbout:", "-tlbout:", "/midl:", "-midl:", "--stats=",
        ]
        .iter()
        .any(|prefix| lower.starts_with(prefix))
            || matches!(arg.as_str(), "-object_path_lto" | "-save-temps")
            || matches!(
                lower.as_str(),
                "/ltcg:pginstrument"
                    | "-ltcg:pginstrument"
                    | "/ltcg:pgoptimize"
                    | "-ltcg:pgoptimize"
                    | "/ltcg:pgupdate"
                    | "-ltcg:pgupdate"
            )
    })
}

pub(super) fn implicit_msvc_output_option(
    args: &[String],
    requested: &Path,
    staged: &Path,
) -> Option<String> {
    let extension = requested
        .extension()
        .and_then(|extension| extension.to_str())?
        .to_ascii_lowercase();
    let upper_args = args
        .iter()
        .map(|arg| arg.to_ascii_uppercase())
        .collect::<Vec<_>>();
    let has = |option: &str| upper_args.iter().any(|arg| arg == option);
    let has_prefix = |prefix: &str| upper_args.iter().any(|arg| arg.starts_with(prefix));
    let prefix = match extension.as_str() {
        "pdb" if has_prefix("/DEBUG") || has_prefix("-DEBUG") => "/PDB:",
        "ilk"
            if has("/INCREMENTAL")
                || has("-INCREMENTAL")
                || has_prefix("/DEBUG")
                || has_prefix("-DEBUG") =>
        {
            "/ILK:"
        }
        "map" if has("/MAP") || has("-MAP") => "/MAP:",
        "iobj" if has("/LTCG:INCREMENTAL") || has("-LTCG:INCREMENTAL") => "/LTCGOUT:",
        "pgd"
            if has("/GENPROFILE")
                || has("-GENPROFILE")
                || has("/FASTGENPROFILE")
                || has("-FASTGENPROFILE") =>
        {
            "/PGD:"
        }
        "winmd" if has("/WINMD") || has("-WINMD") => "/WINMDFILE:",
        _ => return None,
    };
    Some(format!("{prefix}{}", staged.display()))
}

pub(super) fn absolute(path: &Path, cwd: &Path) -> NormalizedPath {
    if path.is_absolute() {
        path.into()
    } else {
        cwd.join(path).into()
    }
}

pub(super) fn rewrite_emit_value(value: &mut String, outputs: &[StagedOutputPlan], cwd: &Path) {
    let rewritten = value
        .split(',')
        .map(|part| {
            let Some((kind, path)) = part.split_once('=') else {
                return part.to_string();
            };
            let requested = absolute(Path::new(path), cwd);
            outputs
                .iter()
                .find(|output| output.requested == requested)
                .map_or_else(
                    || part.to_string(),
                    |output| format!("{kind}={}", output.staged.display()),
                )
        })
        .collect::<Vec<_>>()
        .join(",");
    *value = rewritten;
}
