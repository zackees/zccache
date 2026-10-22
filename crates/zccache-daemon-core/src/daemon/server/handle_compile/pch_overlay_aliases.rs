//! Discover include aliases before a private PCH view changes file identity.

use std::path::Path;

use super::super::*;
use crate::fingerprint::pch_inputs::{PchRefusal, PchRefusalReason};

fn refusal(path: &Path, error: impl std::fmt::Display) -> PchRefusal {
    PchRefusal {
        path: path.to_string_lossy().into_owned(),
        reason: PchRefusalReason::Unreadable(format!("PCH alias scan: {error}")),
    }
}

/// Retain each original spelling, including hardlinks and symlinked directories.
#[allow(clippy::too_many_arguments)]
pub(in crate::daemon::server) async fn extend_overlay_aliases(
    overlay_path: &Path,
    compiler: &NormalizedPath,
    args: &[String],
    cwd: &Path,
    client_env: &Option<Vec<(String, String)>>,
    lineage: &crate::daemon::lineage::Lineage,
    stdin: Option<&[u8]>,
) -> Result<(), PchRefusal> {
    let fail = |error| refusal(overlay_path, error);
    let bytes = std::fs::read(overlay_path).map_err(|error| fail(error.to_string()))?;
    let mut document: serde_json::Value =
        serde_json::from_slice(&bytes).map_err(|error| fail(error.to_string()))?;
    let roots = document
        .get_mut("roots")
        .and_then(serde_json::Value::as_array_mut)
        .ok_or_else(|| fail("overlay has no roots array".to_owned()))?;
    let mappings = coalesce_roots(roots, overlay_path)?;
    let directory = overlay_path
        .parent()
        .ok_or_else(|| fail("overlay has no parent directory".to_owned()))?;
    let depfile = directory.join("aliases.d");
    // Direct requests can still contain response files. Expand them before
    // stripping destinations so hidden -MJ/-MF options cannot write out.
    let expanded = crate::compiler::response_file::expand_response_files_in(args, cwd)
        .map_err(|error| fail(error.to_string()))?;
    let mut scan_args =
        strip_scan_outputs(&expanded, false).map_err(|error| fail(error.to_owned()))?;
    let insertion = scan_args
        .iter()
        .position(|arg| arg == "--")
        .unwrap_or(scan_args.len());
    let mut private_flags = Vec::new();
    if !scan_args
        .windows(2)
        .any(|pair| pair[0] == "-ivfsoverlay" && Path::new(&pair[1]) == overlay_path)
    {
        private_flags.extend([
            "-ivfsoverlay".to_owned(),
            overlay_path.to_string_lossy().into_owned(),
        ]);
    }
    private_flags.extend([
        "-M".to_owned(),
        "-MF".to_owned(),
        depfile.to_string_lossy().into_owned(),
        "-MT".to_owned(),
        "zccache_pch_alias_scan".to_owned(),
    ]);
    scan_args.splice(insertion..insertion, private_flags);
    let response = crate::compiler::response_file::write_response_file_if_needed(
        &scan_args,
        directory,
        crate::compiler::CompilerFamily::Clang,
    )
    .map_err(|error| fail(error.to_string()))?;
    let mut builder = kernal_api::SpawnSpec::new(compiler.as_path()).current_dir(cwd);
    builder = builder.args(scan_driver_args(
        &scan_args,
        response.as_ref().map(|file| file.at_arg()),
    ));
    let builder = super::super::client_env::apply_client_env_builder(
        builder,
        client_env,
        lineage,
        crate::compiler::is_dylint_driver(&compiler.to_string_lossy()),
    );
    // The scanner must see coalesced identities as well as the actual compiler.
    std::fs::write(
        overlay_path,
        serde_json::to_vec(&document).map_err(|error| fail(error.to_string()))?,
    )
    .map_err(|error| fail(error.to_string()))?;
    let output = crate::daemon::process::async_builder_output_with_priority_stdin(
        builder,
        CompilePriority::from_client_env(client_env.as_deref()),
        stdin,
        None,
        format!("{} PCH include alias scan", compiler.display()),
        crate::daemon::process::ProcessLiveness::configured(),
    )
    .await
    .map_err(|error| fail(error.to_string()))?;
    if !output.status.success() {
        return Err(fail(format!(
            "compiler failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    let dependencies =
        std::fs::read_to_string(&depfile).map_err(|error| fail(error.to_string()))?;
    // parse_depfile canonicalizes symlinks. Raw tokens preserve precisely the
    // include spellings that need the same virtual file identity.
    let roots = document["roots"]
        .as_array_mut()
        .ok_or_else(|| fail("overlay has no roots array".to_owned()))?;
    let mut seen: std::collections::HashSet<_> = mappings
        .iter()
        .map(|(spelling, _)| spelling.clone())
        .collect();
    for token in crate::depgraph::depfile::depfile_path_tokens(&dependencies) {
        let spelling = if Path::new(&token).is_absolute() {
            token
        } else {
            cwd.join(&token).to_string_lossy().into_owned()
        };
        // A symlink followed by `..` has filesystem semantics that lexical
        // normalization changes. Keep compiler spellings intact for identity
        // checks and VFS names; only private owned outputs are normalized.
        let path = Path::new(&spelling);
        // Tokens include the synthetic make target, which is not an input.
        if seen.contains(&spelling) || !path.is_file() {
            continue;
        }
        for (original, snapshot) in &mappings {
            if !crate::platform::fs::identity::same_file(path, Path::new(original))
                .map_err(|error| refusal(path, error))?
            {
                continue;
            }
            let alias_bytes = std::fs::read(path).map_err(|error| refusal(path, error))?;
            let snapshot_bytes = std::fs::read(snapshot).map_err(|error| refusal(path, error))?;
            if alias_bytes != snapshot_bytes {
                return Err(refusal(path, "input changed while discovering aliases"));
            }
            roots.push(serde_json::json!({
                "type": "file", "name": spelling, "external-contents": snapshot,
            }));
            seen.insert(spelling);
            break;
        }
    }
    let bytes = serde_json::to_vec(&document).map_err(|error| fail(error.to_string()))?;
    std::fs::write(overlay_path, bytes).map_err(|error| fail(error.to_string()))
}

/// Different PCHs can record different spellings for the same underlying file.
/// Every spelling must name one snapshot inode, or pragma-once identity splits.
fn coalesce_roots(
    roots: &mut [serde_json::Value],
    overlay: &Path,
) -> Result<Vec<(String, NormalizedPath)>, PchRefusal> {
    let mut mappings: Vec<(String, NormalizedPath)> = Vec::new();
    for root in roots {
        let original = root["name"]
            .as_str()
            .ok_or_else(|| refusal(overlay, "overlay root has no name"))?;
        let original = original.to_owned();
        let mut snapshot = NormalizedPath::new(
            root["external-contents"]
                .as_str()
                .ok_or_else(|| refusal(overlay, "overlay root has no snapshot"))?,
        );
        for (previous, shared) in &mappings {
            if !crate::platform::fs::identity::same_file(Path::new(&original), Path::new(previous))
                .map_err(|error| refusal(Path::new(&original), error))?
            {
                continue;
            }
            let verify = || -> std::io::Result<bool> {
                Ok(std::fs::metadata(&snapshot)?.modified()?
                    == std::fs::metadata(shared)?.modified()?
                    && std::fs::read(&snapshot)? == std::fs::read(shared)?)
            };
            if !verify().map_err(|error| refusal(Path::new(&original), error))? {
                return Err(refusal(
                    Path::new(&original),
                    "loaded PCHs require conflicting file identities",
                ));
            }
            snapshot.clone_from(shared);
            root["external-contents"] =
                serde_json::Value::String(snapshot.to_string_lossy().into_owned());
            break;
        }
        mappings.push((original, snapshot));
    }
    Ok(mappings)
}

/// Keep the preprocessing action visible to compiler launchers, including when
/// the remaining flags are transported through a response file.
fn scan_driver_args(args: &[String], response: Option<String>) -> Vec<String> {
    // -M implies preprocessing to Clang itself, but not to launchers that
    // select link behavior from literal -c/-S/-E actions. Keep -E outside any
    // response file: launchers need not expand Clang's response-file syntax.
    let mut driver = vec!["-E".to_owned()];
    driver.extend(response.map_or_else(|| args.to_vec(), |file| vec![file]));
    driver
}

/// Remove compile actions and every user-selected dependency/output destination.
/// Forwarded preprocessor streams use cc1's -MD <file> arity.
fn strip_scan_outputs(args: &[String], forwarded: bool) -> Result<Vec<String>, &'static str> {
    let mut result = Vec::new();
    let mut index = 0;
    while index < args.len() {
        let arg = args[index].as_str();
        if arg == "--" {
            result.extend_from_slice(&args[index..]);
            break;
        }
        if matches!(arg, "-Xclang" | "-Xpreprocessor") {
            let wrapper = arg.to_owned();
            let mut stream = Vec::new();
            while args.get(index).is_some_and(|arg| arg == &wrapper) {
                stream.push(
                    args.get(index + 1)
                        .ok_or("missing forwarded argument")?
                        .clone(),
                );
                index += 2;
            }
            for option in strip_scan_outputs(&stream, true)? {
                result.extend([wrapper.clone(), option]);
            }
            continue;
        }
        if let Some(stream) = arg.strip_prefix("-Wp,") {
            let options: Vec<_> = stream.split(',').map(str::to_owned).collect();
            let retained = strip_scan_outputs(&options, true)?;
            if !retained.is_empty() {
                result.push(format!("-Wp,{}", retained.join(",")));
            }
            index += 1;
            continue;
        }
        if arg.starts_with("-Xclang=") || arg.starts_with("-Xpreprocessor=") {
            return Err("unsupported joined forwarded options");
        }
        let output_value = matches!(
            arg,
            "-o" | "--output"
                | "-MF"
                | "-MT"
                | "-MQ"
                | "-MJ"
                | "-dependency-file"
                | "-serialize-diagnostics"
                | "--serialize-diagnostics"
                | "-opt-record-file"
                | "-dumpdir"
                | "-dumpbase"
                | "-dumpbase-ext"
                | "-auxbase-strip"
                | "-dependency-dot"
                | "-header-include-file"
                | "-coverage-notes-file"
                | "-coverage-data-file"
                | "-fprofile-instrument-path"
        ) || (forwarded && matches!(arg, "-MD" | "-MMD"));
        if output_value {
            if args.get(index + 1).is_none() {
                return Err("missing output or dependency option value");
            }
            index += 2;
            continue;
        }
        let joined_output = ["-MF", "-MT", "-MQ", "-MJ"]
            .iter()
            .any(|prefix| arg.starts_with(prefix) && arg.len() > prefix.len())
            || (arg.starts_with("-o")
                && !["-objc", "-object", "-openmp", "-opt-"]
                    .iter()
                    .any(|p| arg.starts_with(p)))
            || [
                "--output=",
                "-dependency-file=",
                "-serialize-diagnostics=",
                "--serialize-diagnostics=",
                "-fmodule-output=",
                "-dumpdir=",
                "-dumpbase=",
                "-dumpbase-ext=",
                "-auxbase-strip=",
                "-dependency-dot=",
                "-header-include-file=",
                "-coverage-notes-file=",
                "-coverage-data-file=",
                "-fprofile-instrument-path=",
                "-opt-record-file=",
            ]
            .iter()
            .any(|prefix| arg.starts_with(prefix));
        if joined_output
            || matches!(
                arg,
                "-c" | "-S"
                    | "-E"
                    | "-M"
                    | "-MM"
                    | "-MD"
                    | "-MMD"
                    | "-MP"
                    | "-MG"
                    | "-verify-pch"
                    | "-emit-pch"
                    | "-emit-obj"
                    | "-emit-llvm"
                    | "-fsyntax-only"
                    | "-save-temps"
                    | "--save-temps"
                    | "-ftime-trace"
                    | "-fmodule-output"
            )
            || arg.starts_with("-save-temps=")
            || arg.starts_with("--save-temps=")
            || arg.starts_with("-ftime-trace=")
        {
            index += 1;
            continue;
        }
        result.push(args[index].clone());
        index += 1;
        if crate::compiler::gnu_flag_takes_value(arg)
            || matches!(
                arg,
                "-D" | "-U" | "-I" | "-F" | "-x" | "-target" | "--target"
            )
        {
            result.push(
                args.get(index)
                    .ok_or("missing compiler option value")?
                    .clone(),
            );
            index += 1;
        }
    }
    Ok(result)
}

#[cfg(test)]
#[path = "pch_overlay_aliases_tests.rs"]
mod tests;
