//! Target artifact selection and classification for Rust plan save operations.

use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::path::{Component, Path};

use zccache_core::NormalizedPath;

use super::schema::{
    RustArtifactClass, RustArtifactPlanV1, RustPlanError, RustPlanMode, RustPlanPackages,
};
use super::summary::RustPlanSummary;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct SelectedArtifact {
    pub(super) source_path: NormalizedPath,
    pub(super) relative_path: String,
    pub(super) class: RustArtifactClass,
}

pub(super) fn select_artifacts(
    plan: &RustArtifactPlanV1,
    candidates: Vec<NormalizedPath>,
    summary: &mut RustPlanSummary,
) -> Vec<SelectedArtifact> {
    let allowed = plan.effective_allowed_classes();
    let dropped: BTreeSet<RustArtifactClass> =
        plan.dropped_artifact_classes.iter().copied().collect();
    let excluded_names = excluded_package_names(&plan.packages);
    let thin_v2 = plan.cache_profile.as_deref() == Some("thin-v2");
    let mut selected = Vec::new();

    for path in candidates {
        let rel_path = match path.strip_prefix(plan.target_dir.as_path()) {
            Ok(rel) => rel,
            Err(_) => {
                summary.skip(path.display().to_string(), "outside_target_dir");
                continue;
            }
        };
        let rel = relative_path_string(rel_path);

        if has_component(rel_path, "incremental") {
            // Always-transient; reported as `transient_state` for back-compat
            // with existing summary consumers regardless of whether thin-v2
            // also listed `Incremental` in `dropped_artifact_classes`.
            summary.skip(rel, "transient_state");
            continue;
        }

        let class = classify_artifact(rel_path, plan.mode, thin_v2);

        if plan.mode == RustPlanMode::Thin {
            let Some(class) = class else {
                summary.skip(rel, "artifact_class_disallowed_by_plan");
                continue;
            };
            // soldr#461: honor the drop list before consulting the allow list.
            // A file matching any dropped class is skipped even if its class is
            // also listed under `allowed_artifact_classes`. This is the
            // load-bearing change that lets thin-v2 actually prune the bundle.
            if dropped.contains(&class) {
                summary.skip(rel, "artifact_class_disallowed_by_plan");
                continue;
            }
            if !allowed.contains(&class) {
                summary.skip(rel, "artifact_class_disallowed_by_plan");
                continue;
            }
            if artifact_matches_excluded_package(rel_path, &excluded_names) {
                summary.skip(rel, "workspace_or_path_dependency_excluded_by_plan");
                continue;
            }
            selected.push(SelectedArtifact {
                source_path: path,
                relative_path: rel,
                class,
            });
            continue;
        }

        selected.push(SelectedArtifact {
            source_path: path,
            relative_path: rel,
            class: class.unwrap_or(RustArtifactClass::FullTarget),
        });
    }

    selected.sort_by(|a, b| a.relative_path.cmp(&b.relative_path));
    selected
}

pub(super) fn classify_artifact(
    rel: &Path,
    mode: RustPlanMode,
    thin_v2: bool,
) -> Option<RustArtifactClass> {
    // .dSYM/ is a directory bundle on macOS; every file *inside* an enclosing
    // `*.dSYM` ancestor component is dsym. Check first so we don't try to
    // classify `Contents/Info.plist` etc. by extension.
    if path_has_dsym_ancestor(rel) {
        return Some(RustArtifactClass::Dsym);
    }

    if has_component(rel, ".fingerprint") {
        // thin-v2 splits the legacy umbrella into Meta (kept) vs Outputs
        // (dropped). Older plans keep the legacy single-class behavior so
        // existing thin-v1 callers see no semantic change.
        if thin_v2 {
            if is_fingerprint_meta_file(rel) {
                return Some(RustArtifactClass::CargoFingerprintMeta);
            }
            return Some(RustArtifactClass::CargoFingerprintOutputs);
        }
        return Some(RustArtifactClass::CargoFingerprint);
    }
    if has_component(rel, "build") {
        if has_component(rel, "out") {
            return Some(RustArtifactClass::BuildScriptOutput);
        }
        if let Some(name) = rel.file_name().and_then(OsStr::to_str) {
            if matches!(name, "output" | "invoked.timestamp" | "root-output") {
                return Some(RustArtifactClass::BuildScriptMetadata);
            }
            // soldr#461: name the compiled build-script binaries so the
            // drop list can reach them. Cargo emits them as
            // `target/<profile>/build/<crate>-<hash>/build-script-build`
            // (possibly with a `.exe` suffix on Windows).
            if is_build_script_build_file(name) {
                return Some(RustArtifactClass::BuildScriptBuild);
            }
        }
    }

    match rel.extension().and_then(OsStr::to_str) {
        Some("rlib") => Some(RustArtifactClass::Rlib),
        Some("rmeta") => Some(RustArtifactClass::Rmeta),
        Some("d") => Some(RustArtifactClass::DepInfo),
        Some("dwo") if has_component(rel, "deps") => Some(RustArtifactClass::Dwo),
        Some("pdb") if has_component(rel, "deps") => Some(RustArtifactClass::Pdb),
        Some("so" | "dylib" | "dll") if is_likely_proc_macro_dylib(rel) => {
            Some(RustArtifactClass::ProcMacro)
        }
        Some("so" | "dylib" | "dll") => Some(RustArtifactClass::SharedLib),
        _ if mode == RustPlanMode::Full => Some(RustArtifactClass::FullTarget),
        _ => None,
    }
}

/// True when `rel` has any ancestor path component ending in `.dSYM`. The
/// match is case-insensitive on the suffix to tolerate filesystems that
/// preserve the historical mixed case but mount case-folded.
fn path_has_dsym_ancestor(rel: &Path) -> bool {
    rel.components().any(|component| {
        component
            .as_os_str()
            .to_str()
            .map(|name| {
                let lower = name.to_ascii_lowercase();
                lower.ends_with(".dsym")
            })
            .unwrap_or(false)
    })
}

/// True for files cargo writes inside `.fingerprint/<crate>-<hash>/` that
/// feed its freshness decision. soldr's `docs/THIN_TARGET_CACHE_PRUNING.md`
/// Section 4.3 enumerates these prefixes. Everything else in the directory
/// (notably the `*.json` debug files) is treated as output.
fn is_fingerprint_meta_file(rel: &Path) -> bool {
    let Some(name) = rel.file_name().and_then(OsStr::to_str) else {
        return false;
    };
    if name == "invoked.timestamp" {
        return true;
    }
    matches!(
        name.split('-').next(),
        Some("dep" | "output" | "lib" | "bin")
    ) && name.contains('-')
}

/// True for cargo's compiled build-script binaries. The base name is
/// `build-script-build`; the `.exe` suffix appears on Windows targets.
fn is_build_script_build_file(name: &str) -> bool {
    let stem = name.strip_suffix(".exe").unwrap_or(name);
    stem == "build-script-build" || stem.starts_with("build-script-build-")
}

fn is_likely_proc_macro_dylib(rel: &Path) -> bool {
    if !has_component(rel, "deps") {
        return false;
    }

    rel.file_stem()
        .and_then(OsStr::to_str)
        .map(|stem| {
            let stem = stem.to_ascii_lowercase();
            stem.contains("proc_macro") || stem.contains("proc-macro")
        })
        .unwrap_or(false)
}

pub(super) fn collect_files(
    root: &Path,
    files: &mut Vec<NormalizedPath>,
) -> Result<(), RustPlanError> {
    if !root.exists() {
        return Ok(());
    }

    let mut entries = Vec::new();
    for entry in std::fs::read_dir(root)? {
        entries.push(entry?);
    }
    entries.sort_by_key(|entry| entry.file_name());

    for entry in entries {
        let path = NormalizedPath::new(entry.path());
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            collect_files(path.as_path(), files)?;
        } else if file_type.is_file() {
            files.push(path);
        }
    }
    Ok(())
}

fn relative_path_string(path: &Path) -> String {
    path.components()
        .filter_map(|component| match component {
            Component::Normal(part) => Some(part.to_string_lossy().into_owned()),
            Component::CurDir => None,
            _ => Some(component.as_os_str().to_string_lossy().into_owned()),
        })
        .collect::<Vec<_>>()
        .join("/")
}

fn has_component(path: &Path, needle: &str) -> bool {
    path.components()
        .any(|component| component.as_os_str() == OsStr::new(needle))
}

fn excluded_package_names(packages: &RustPlanPackages) -> BTreeSet<String> {
    packages
        .workspace_package_ids
        .iter()
        .chain(packages.excluded_path_package_ids.iter())
        .filter_map(|id| package_name_from_id(id))
        .collect()
}

pub(super) fn package_name_from_id(id: &str) -> Option<String> {
    let candidate = if let Some(after_hash) = id.rsplit_once('#').map(|(_, right)| right) {
        after_hash.split('@').next().unwrap_or(after_hash)
    } else if let Some((left, _)) = id.split_once(' ') {
        left
    } else {
        id
    };
    let candidate = candidate
        .trim()
        .trim_matches('"')
        .trim_matches('\'')
        .replace('-', "_");
    if candidate.is_empty()
        || candidate.contains('/')
        || candidate.contains('\\')
        || candidate.contains(':')
    {
        None
    } else {
        Some(candidate)
    }
}

pub(super) fn artifact_matches_excluded_package(
    rel: &Path,
    excluded_names: &BTreeSet<String>,
) -> bool {
    if excluded_names.is_empty() {
        return false;
    }
    rel.components().any(|component| {
        let name = component.as_os_str().to_string_lossy();
        excluded_names.iter().any(|package| {
            let without_lib = name.strip_prefix("lib").unwrap_or(&name);
            without_lib == package
                || without_lib.starts_with(&format!("{package}-"))
                || without_lib.starts_with(&format!("{package}."))
        })
    })
}
