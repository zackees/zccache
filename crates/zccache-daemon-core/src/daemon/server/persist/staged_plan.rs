//! Private compiler-output staging plans.
//!
//! Planning is deliberately separate from persistence: the compiler must be
//! redirected before it starts, and unsupported output shapes must fall back
//! before spawn rather than publishing a partial set afterwards.

use super::staged_store::{staged_lane_enabled, StagedMaterializationStats};
use crate::core::path::NormalizedPath;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static PLAN_COUNTER: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone)]
pub(in crate::daemon::server) struct StagedOutputPlan {
    pub(in crate::daemon::server) requested: NormalizedPath,
    pub(in crate::daemon::server) staged: NormalizedPath,
}

#[derive(Debug)]
pub(in crate::daemon::server) struct StagedCompilePlan {
    pub(in crate::daemon::server) outputs: Vec<StagedOutputPlan>,
    pub(in crate::daemon::server) rewritten_args: Vec<String>,
    root: PathBuf,
}

impl StagedCompilePlan {
    /// Build a plan for the narrow Phase 3 Rust lane.  A plan is absent for
    /// unsupported invocations, preserving the proven legacy path.
    pub(in crate::daemon::server) fn rustc(
        staging_dir: &Path,
        args: &[String],
        primary_output: &NormalizedPath,
        expected_outputs: &[NormalizedPath],
        cwd: &Path,
    ) -> io::Result<Option<Self>> {
        if !staged_lane_enabled(crate::compiler::CompilerFamily::Rustc) || emit_to_stdout(args) {
            return Ok(None);
        }
        let root = staging_dir.join(format!(
            ".compile-{}-{}",
            std::process::id(),
            PLAN_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&root)?;

        let mut primary = absolute(primary_output, cwd);
        let mut outputs = Vec::with_capacity(expected_outputs.len());
        for requested in expected_outputs {
            let requested = absolute(requested, cwd);
            let file_name = requested.file_name().ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "rustc output has no filename")
            })?;
            let staged = root.join(file_name);
            if outputs
                .iter()
                .any(|output: &StagedOutputPlan| output.staged.as_path() == staged)
            {
                let _ = std::fs::remove_dir_all(&root);
                return Ok(None);
            }
            outputs.push(StagedOutputPlan {
                requested: requested.clone(),
                staged: staged.into(),
            });
        }
        for (kind, path) in emit_specs(args) {
            let requested = absolute(Path::new(&path), cwd);
            if kind == "link" {
                primary = requested.clone();
            }
            let replacement = outputs.iter().position(|output| {
                output.requested == requested || output_kind(&output.requested) == kind
            });
            if let Some(index) = replacement {
                outputs[index].requested = requested;
            } else {
                outputs.push(StagedOutputPlan {
                    requested,
                    staged: root
                        .join(Path::new(&path).file_name().ok_or_else(|| {
                            io::Error::new(io::ErrorKind::InvalidInput, "emit path has no filename")
                        })?)
                        .into(),
                });
            }
        }
        let mut names = std::collections::HashSet::new();
        outputs.retain(|output| names.insert(output.requested.clone()));
        for output in &mut outputs {
            let file_name = output.requested.file_name().ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "rustc output has no filename")
            })?;
            output.staged = root.join(file_name).into();
        }
        let staged_primary = outputs
            .iter()
            .find(|output| output.requested == primary)
            .map(|output| output.staged.clone())
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "primary output missing from Rust plan",
                )
            })?;

        let mut rewritten_args = args.to_vec();
        let mut replaced_output = false;
        let mut replaced_out_dir = false;
        let mut i = 0;
        while i < rewritten_args.len() {
            if rewritten_args[i] == "-o" {
                if i + 1 >= rewritten_args.len() {
                    return Ok(None);
                }
                rewritten_args[i + 1] = staged_primary.to_string_lossy().into_owned();
                replaced_output = true;
                i += 2;
                continue;
            }
            if rewritten_args[i] == "--out-dir" {
                if i + 1 >= rewritten_args.len() {
                    let _ = std::fs::remove_dir_all(&root);
                    return Ok(None);
                }
                rewritten_args[i + 1] = root.to_string_lossy().into_owned();
                replaced_out_dir = true;
                i += 2;
                continue;
            }
            if rewritten_args[i].starts_with("--out-dir=") {
                rewritten_args[i] = format!("--out-dir={}", root.display());
                replaced_out_dir = true;
                i += 1;
                continue;
            }
            if rewritten_args[i] == "--emit" {
                if let Some(value) = rewritten_args.get_mut(i + 1) {
                    rewrite_emit_value(value, &outputs, cwd);
                }
                i += 2;
                continue;
            }
            if rewritten_args[i].starts_with("--emit=") {
                let value = rewritten_args[i]["--emit=".len()..].to_string();
                let mut rewritten = value;
                rewrite_emit_value(&mut rewritten, &outputs, cwd);
                rewritten_args[i] = format!("--emit={rewritten}");
                i += 1;
                continue;
            }
            if let Some(value) = rewritten_args[i].strip_prefix("-o") {
                if !value.is_empty() {
                    rewritten_args[i] = format!("-o{}", staged_primary.display());
                    replaced_output = true;
                }
            }
            i += 1;
        }
        if !replaced_output && !replaced_out_dir && emit_specs(args).is_empty() {
            rewritten_args.push("-o".to_string());
            rewritten_args.push(staged_primary.to_string_lossy().into_owned());
        }
        if outputs.len() > 1 && !replaced_out_dir {
            rewritten_args.push("--out-dir".to_string());
            rewritten_args.push(root.to_string_lossy().into_owned());
        }

        Ok(Some(Self {
            outputs,
            rewritten_args,
            root,
        }))
    }

    pub(in crate::daemon::server) fn cc(
        staging_dir: &Path,
        family: crate::compiler::CompilerFamily,
        args: &[String],
        primary_output: &NormalizedPath,
        cwd: &Path,
        dep_flags: &crate::depgraph::UserDepFlags,
    ) -> io::Result<Option<Self>> {
        if !staged_lane_enabled(family) {
            return Ok(None);
        }
        if cc_has_unsupported_side_outputs(args) {
            return Ok(None);
        }
        let output_role = crate::compiler::OutputClassification::for_compiler(
            family,
            &primary_output.to_string_lossy(),
        )
        .role;
        if !matches!(
            output_role,
            crate::compiler::OutputRole::Object
                | crate::compiler::OutputRole::PrecompiledHeaderOrModule
        ) {
            return Ok(None);
        }
        if family == crate::compiler::CompilerFamily::Msvc
            && !args.iter().any(|arg| {
                arg.get(..3)
                    .is_some_and(|prefix| prefix.eq_ignore_ascii_case("/fo"))
            })
        {
            return Ok(None);
        }
        let root = staging_dir.join(format!(
            ".compile-{}-{}",
            std::process::id(),
            PLAN_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&root)?;
        let requested = absolute(primary_output, cwd);
        let file_name = requested.file_name().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "compiler output has no filename",
            )
        })?;
        let staged: NormalizedPath = root.join(file_name).into();
        let requested_depfile = dep_flags.mf_path.clone().or_else(|| {
            dep_flags
                .has_md
                .then(|| requested.as_path().with_extension("d").into())
        });
        let mut rewritten_args = args.to_vec();
        let mut replaced = false;
        let mut i = 0;
        while i < rewritten_args.len() {
            if family == crate::compiler::CompilerFamily::Msvc
                && rewritten_args[i].eq_ignore_ascii_case("/fo")
            {
                if i + 1 >= rewritten_args.len() {
                    let _ = std::fs::remove_dir_all(&root);
                    return Ok(None);
                }
                rewritten_args[i + 1] = staged.to_string_lossy().into_owned();
                replaced = true;
                i += 2;
                continue;
            }
            if family == crate::compiler::CompilerFamily::Msvc
                && rewritten_args[i]
                    .get(..3)
                    .is_some_and(|prefix| prefix.eq_ignore_ascii_case("/fo"))
            {
                let prefix = rewritten_args[i].get(..3).unwrap_or("/Fo");
                rewritten_args[i] = format!("{prefix}{}", staged.display());
                replaced = true;
                i += 1;
                continue;
            }
            if rewritten_args[i] == "-o" {
                if i + 1 >= rewritten_args.len() {
                    let _ = std::fs::remove_dir_all(&root);
                    return Ok(None);
                }
                rewritten_args[i + 1] = staged.to_string_lossy().into_owned();
                replaced = true;
                i += 2;
                continue;
            }
            if let Some(value) = rewritten_args[i].strip_prefix("-o") {
                if !value.is_empty() {
                    rewritten_args[i] = format!("-o{}", staged.display());
                    replaced = true;
                }
            }
            if let Some(requested_depfile) = requested_depfile.as_ref() {
                if rewritten_args[i] == "-MF" {
                    if i + 1 >= rewritten_args.len() {
                        let _ = std::fs::remove_dir_all(&root);
                        return Ok(None);
                    }
                    let staged_depfile =
                        root.join(requested_depfile.file_name().ok_or_else(|| {
                            io::Error::new(
                                io::ErrorKind::InvalidInput,
                                "depfile output has no filename",
                            )
                        })?);
                    rewritten_args[i + 1] = staged_depfile.to_string_lossy().into_owned();
                    i += 2;
                    continue;
                }
                if let Some(value) = rewritten_args[i].strip_prefix("-MF") {
                    if !value.is_empty() {
                        let staged_depfile =
                            root.join(requested_depfile.file_name().ok_or_else(|| {
                                io::Error::new(
                                    io::ErrorKind::InvalidInput,
                                    "depfile output has no filename",
                                )
                            })?);
                        rewritten_args[i] = format!("-MF{}", staged_depfile.display());
                        continue;
                    }
                }
            }
            i += 1;
        }
        if dep_flags.mf_path.is_some()
            && !args
                .iter()
                .any(|arg| arg == "-MF" || arg.starts_with("-MF"))
        {
            let _ = std::fs::remove_dir_all(&root);
            return Ok(None);
        }
        if !replaced {
            rewritten_args.push("-o".to_string());
            rewritten_args.push(staged.to_string_lossy().into_owned());
        }
        let mut outputs = vec![StagedOutputPlan { requested, staged }];
        if let Some(requested_depfile) = requested_depfile {
            let staged_depfile = root.join(requested_depfile.file_name().ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "depfile output has no filename",
                )
            })?);
            outputs.push(StagedOutputPlan {
                requested: requested_depfile,
                staged: staged_depfile.into(),
            });
        }
        Ok(Some(Self {
            outputs,
            rewritten_args,
            root,
        }))
    }

    /// Stage a pure archive invocation. Linkers with secondary side outputs
    /// use a separate planner because silently omitting a PDB/import library
    /// would violate complete-set publication.
    pub(in crate::daemon::server) fn archive(
        staging_dir: &Path,
        args: &[String],
        primary_output: &NormalizedPath,
        cwd: &Path,
    ) -> io::Result<Option<Self>> {
        if !staged_lane_enabled(crate::compiler::CompilerFamily::Gcc) {
            return Ok(None);
        }
        let root = staging_dir.join(format!(
            ".compile-{}-{}",
            std::process::id(),
            PLAN_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&root)?;
        let requested = absolute(primary_output, cwd);
        let file_name = requested.file_name().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "archive output has no filename",
            )
        })?;
        let staged: NormalizedPath = root.join(file_name).into();
        let requested_text = requested.to_string_lossy();
        let mut rewritten_args = args.to_vec();
        let mut replaced = false;
        for arg in &mut rewritten_args {
            if *arg == requested_text {
                *arg = staged.to_string_lossy().into_owned();
                replaced = true;
            }
        }
        if !replaced {
            let file_name = requested.file_name().unwrap_or_default().to_string_lossy();
            for arg in &mut rewritten_args {
                if arg == file_name.as_ref() {
                    *arg = staged.to_string_lossy().into_owned();
                    replaced = true;
                    break;
                }
            }
        }
        if !replaced {
            let _ = std::fs::remove_dir_all(&root);
            return Ok(None);
        }
        Ok(Some(Self {
            outputs: vec![StagedOutputPlan { requested, staged }],
            rewritten_args,
            root,
        }))
    }

    /// Build a linker plan only when every declared output can be redirected
    /// by exact path replacement. Undeclared linker side effects are checked
    /// by the caller after the process exits; they invalidate publication.
    pub(in crate::daemon::server) fn link(
        staging_dir: &Path,
        args: &[String],
        primary_output: &NormalizedPath,
        secondary_outputs: &[NormalizedPath],
        cwd: &Path,
    ) -> io::Result<Option<Self>> {
        if !super::staged_link_lane_enabled() {
            return Ok(None);
        }
        if has_unmodeled_link_output_option(args) {
            return Ok(None);
        }
        let root = staging_dir.join(format!(
            ".link-{}-{}",
            std::process::id(),
            PLAN_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&root)?;
        let result = (|| {
            let mut requested_outputs = Vec::with_capacity(1 + secondary_outputs.len());
            requested_outputs.push(absolute(primary_output, cwd));
            requested_outputs.extend(secondary_outputs.iter().map(|output| absolute(output, cwd)));
            let mut outputs = Vec::with_capacity(requested_outputs.len());
            let mut rewritten_args = args.to_vec();
            for requested in requested_outputs {
                let requested_text = requested.to_string_lossy();
                if requested_text.contains('%') || requested.as_path().is_dir() {
                    return Ok(None);
                }
                let filename = requested.file_name().ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidInput, "link output has no filename")
                })?;
                let staged: NormalizedPath = root.join(filename).into();
                if outputs.iter().any(|output: &StagedOutputPlan| {
                    output.requested == requested || output.staged == staged
                }) {
                    return Ok(None);
                }
                let relative = requested
                    .strip_prefix(cwd)
                    .ok()
                    .map(|path| path.to_string_lossy().into_owned());
                let mut replaced = false;
                for arg in &mut rewritten_args {
                    let candidates = [Some(requested_text.as_ref()), relative.as_deref()];
                    match rewrite_link_output_arg(
                        arg,
                        candidates.into_iter().flatten(),
                        staged.to_string_lossy().as_ref(),
                    ) {
                        Some(arg_replaced) => replaced |= arg_replaced,
                        None => return Ok(None),
                    }
                }
                if !replaced {
                    return Ok(None);
                }
                outputs.push(StagedOutputPlan { requested, staged });
            }
            Ok(Some(Self {
                outputs,
                rewritten_args,
                root: root.clone(),
            }))
        })();
        if matches!(result, Ok(None) | Err(_)) {
            let _ = std::fs::remove_dir_all(&root);
        }
        result
    }

    pub(in crate::daemon::server) fn output_paths(&self) -> Vec<NormalizedPath> {
        self.outputs
            .iter()
            .map(|output| output.staged.clone())
            .collect()
    }

    pub(in crate::daemon::server) fn primary_staged(&self) -> &NormalizedPath {
        &self.outputs[0].staged
    }

    pub(in crate::daemon::server) fn staged_for_requested(
        &self,
        requested: &Path,
    ) -> Option<NormalizedPath> {
        self.outputs
            .iter()
            .find(|output| output.requested.as_path() == requested)
            .map(|output| output.staged.clone())
    }

    pub(in crate::daemon::server) fn unexpected_staged_entries(&self) -> io::Result<Vec<PathBuf>> {
        let declared: std::collections::HashSet<PathBuf> = self
            .outputs
            .iter()
            .map(|output| output.staged.as_path().to_path_buf())
            .collect();
        Ok(std::fs::read_dir(&self.root)?
            .map(|entry| entry.map(|entry| entry.path()))
            .collect::<io::Result<Vec<_>>>()?
            .into_iter()
            .filter(|path| !declared.contains(path))
            .collect())
    }

    pub(in crate::daemon::server) fn rewrite_depfile_strategy(
        &self,
        strategy: crate::depgraph::DepfileStrategy,
    ) -> crate::depgraph::DepfileStrategy {
        let path = match &strategy {
            crate::depgraph::DepfileStrategy::Injected { path }
            | crate::depgraph::DepfileStrategy::UserSpecified { path }
            | crate::depgraph::DepfileStrategy::UserDefault { path } => path,
            crate::depgraph::DepfileStrategy::ShowIncludes
            | crate::depgraph::DepfileStrategy::Unsupported => return strategy,
        };
        let Some(staged) = self
            .outputs
            .iter()
            .find(|output| output.requested == *path)
            .map(|output| output.staged.clone())
        else {
            return strategy;
        };
        match strategy {
            crate::depgraph::DepfileStrategy::Injected { .. } => {
                crate::depgraph::DepfileStrategy::Injected { path: staged }
            }
            crate::depgraph::DepfileStrategy::UserSpecified { .. } => {
                crate::depgraph::DepfileStrategy::UserSpecified { path: staged }
            }
            crate::depgraph::DepfileStrategy::UserDefault { .. } => {
                crate::depgraph::DepfileStrategy::UserDefault { path: staged }
            }
            crate::depgraph::DepfileStrategy::ShowIncludes
            | crate::depgraph::DepfileStrategy::Unsupported => strategy,
        }
    }

    pub(in crate::daemon::server) fn materialize(&self) -> io::Result<StagedMaterializationStats> {
        let mut stats = StagedMaterializationStats::default();
        for output in &self.outputs {
            if let Some(parent) = output.requested.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let output_stats = crate::daemon::server::persist::materialize_independent_with_stats(
                output.staged.as_path(),
                output.requested.as_path(),
            )
            .map_err(|error| {
                io::Error::new(
                    error.kind(),
                    format!(
                        "{} -> {}: {error}",
                        output.staged.display(),
                        output.requested.display()
                    ),
                )
            })?;
            stats.reflink_count = stats
                .reflink_count
                .saturating_add(output_stats.reflink_count);
            stats.copy_count = stats.copy_count.saturating_add(output_stats.copy_count);
            stats.copy_bytes = stats.copy_bytes.saturating_add(output_stats.copy_bytes);
        }
        self.cleanup()?;
        Ok(stats)
    }

    /// Rust dep-info is an output too. Translate the private staging prefix
    /// back to the logical requested destination before it is hashed and
    /// published, so cache hits never leak cache-root paths into build files.
    pub(in crate::daemon::server) fn rewrite_logical_side_outputs(&self) {
        for output in &self.outputs {
            if output.requested.extension().and_then(|ext| ext.to_str()) != Some("d") {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(output.staged.as_path()) else {
                continue;
            };
            let staged = output.staged.to_string_lossy();
            let requested = output.requested.to_string_lossy();
            let requested_parent = output
                .requested
                .parent()
                .map_or_else(String::new, |parent| parent.to_string_lossy().into_owned());
            let staged_root = self.root.to_string_lossy();
            let rewritten = text
                .replace(staged.as_ref(), requested.as_ref())
                .replace(staged_root.as_ref(), &requested_parent)
                .replace(
                    &staged_root.replace('\\', "/"),
                    &requested_parent.replace('\\', "/"),
                );
            if rewritten != text {
                let _ = std::fs::write(output.staged.as_path(), rewritten);
            }
        }
    }

    pub(in crate::daemon::server) fn cleanup(&self) -> io::Result<()> {
        std::fs::remove_dir_all(&self.root).or_else(|error| {
            if error.kind() == io::ErrorKind::NotFound {
                Ok(())
            } else {
                Err(error)
            }
        })
    }
}

/// Rewrite one exact or delimiter-bounded linker output path. Returning
/// `None` means the token was ambiguous and the caller must use the legacy
/// path before spawning the linker.
fn rewrite_link_output_arg<'a>(
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

fn has_unmodeled_link_output_option(args: &[String]) -> bool {
    args.iter().any(|arg| {
        let lower = arg.to_ascii_lowercase();
        [
            "/pgd:",
            "-pgd:",
            "/ltcgout:",
            "-ltcgout:",
            "/idlout:",
            "-idlout:",
            "/tlbout:",
            "-tlbout:",
            "/winmdfile:",
            "-winmdfile:",
            "/midl:",
            "-midl:",
            "--stats=",
        ]
        .iter()
        .any(|prefix| lower.starts_with(prefix))
            || matches!(
                lower.as_str(),
                "/winmd" | "-winmd" | "/ltcg:incremental" | "-ltcg:incremental"
            )
            || matches!(
                arg.as_str(),
                "-map" | "-dependency_info" | "-object_path_lto" | "-save-temps"
            )
    })
}

impl Drop for StagedCompilePlan {
    fn drop(&mut self) {
        let _ = self.cleanup();
    }
}

fn absolute(path: &Path, cwd: &Path) -> NormalizedPath {
    if path.is_absolute() {
        path.to_path_buf().into()
    } else {
        cwd.join(path).into()
    }
}

fn cc_has_unsupported_side_outputs(args: &[String]) -> bool {
    args.iter().any(|arg| {
        let lower = arg.to_ascii_lowercase();
        lower == "--serialize-diagnostics"
            || lower == "-mj"
            || lower.starts_with("-fmodule")
            || lower.starts_with("-Winvalid-pch")
            || lower.starts_with("/fd")
            || lower.starts_with("/fp")
            || lower.starts_with("/fa")
            || lower.starts_with("/fi")
    })
}

fn emit_specs(args: &[String]) -> Vec<(String, String)> {
    let mut result = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let value = if args[i] == "--emit" {
            i += 1;
            args.get(i).map(String::as_str)
        } else {
            args[i].strip_prefix("--emit=")
        };
        if let Some(value) = value {
            for part in value.split(',') {
                if let Some((kind, path)) = part.split_once('=') {
                    if path == "-" || path.is_empty() {
                        return Vec::new();
                    }
                    result.push((kind.to_string(), path.to_string()));
                }
            }
        }
        i += 1;
    }
    result
}

fn emit_to_stdout(args: &[String]) -> bool {
    args.iter().enumerate().any(|(index, arg)| {
        let value = arg.strip_prefix("--emit=").or_else(|| {
            (arg == "--emit")
                .then(|| args.get(index + 1).map(String::as_str))
                .flatten()
        });
        value.is_some_and(|value| value.split(',').any(|part| part.ends_with("=-")))
    })
}

fn output_kind(path: &Path) -> String {
    match path.extension().and_then(|extension| extension.to_str()) {
        Some("rmeta") => "metadata",
        Some("d") => "dep-info",
        Some("o") => "obj",
        Some("s") => "asm",
        Some("ll") => "llvm-ir",
        Some("bc") => "llvm-bc",
        Some("mir") => "mir",
        Some("rlib" | "a" | "exe") | None => "link",
        Some(other) => other,
    }
    .to_string()
}

fn rewrite_emit_value(value: &mut String, outputs: &[StagedOutputPlan], cwd: &Path) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn staged_tests_enabled() -> bool {
        std::env::var_os(super::super::staged_store::STAGED_ARTIFACTS_ENV).is_some()
    }

    #[test]
    fn rust_plan_rewrites_output_before_spawn() {
        if !staged_tests_enabled() {
            return;
        }
        let temp = tempdir().unwrap();
        let output: NormalizedPath = temp.path().join("target/libx.rlib").into();
        let plan = StagedCompilePlan::rustc(
            temp.path(),
            &[
                "--crate-type=rlib".into(),
                "-o".into(),
                output.to_string_lossy().into_owned(),
            ],
            &output,
            std::slice::from_ref(&output),
            temp.path(),
        )
        .unwrap()
        .unwrap();
        assert!(!plan
            .rewritten_args
            .contains(&output.to_string_lossy().into_owned()));
        assert!(plan
            .primary_staged()
            .as_path()
            .starts_with(temp.path().join(".staged-v2")));
        plan.cleanup().unwrap();
    }

    #[test]
    fn explicit_emit_destination_is_staged_and_mapped() {
        if !staged_tests_enabled() {
            return;
        }
        let temp = tempdir().unwrap();
        let output: NormalizedPath = temp.path().join("x.rlib").into();
        let plan = StagedCompilePlan::rustc(
            temp.path(),
            &["--emit=link,dep-info=custom.d".into()],
            &output,
            std::slice::from_ref(&output),
            temp.path(),
        )
        .unwrap()
        .unwrap();
        assert!(plan.outputs.iter().any(|output| {
            output.requested.file_name().and_then(|name| name.to_str()) == Some("custom.d")
        }));
        assert!(plan
            .rewritten_args
            .iter()
            .any(|arg| arg.contains("dep-info=") && arg.contains(".staged-v2")));
        plan.cleanup().unwrap();
    }

    #[test]
    fn cc_plan_rewrites_concatenated_output_flag() {
        if !staged_tests_enabled() {
            return;
        }
        let temp = tempdir().unwrap();
        let output: NormalizedPath = temp.path().join("hello.o").into();
        let plan = StagedCompilePlan::cc(
            temp.path(),
            crate::compiler::CompilerFamily::Clang,
            &["-c".into(), "hello.cpp".into(), "-ohello.o".into()],
            &output,
            temp.path(),
            &crate::depgraph::UserDepFlags::default(),
        )
        .unwrap()
        .unwrap();
        assert!(plan
            .rewritten_args
            .iter()
            .any(|arg| arg.starts_with("-o") && arg.contains(".staged-v2")));
        plan.cleanup().unwrap();
    }

    #[test]
    fn cc_plan_stages_user_depfile_without_leaking_private_path() {
        if !staged_tests_enabled() {
            return;
        }
        let temp = tempdir().unwrap();
        let output: NormalizedPath = temp.path().join("hello.o").into();
        let depfile: NormalizedPath = temp.path().join("deps/hello.d").into();
        let dep_flags = crate::depgraph::UserDepFlags {
            has_md: true,
            mf_path: Some(depfile.clone()),
        };
        let plan = StagedCompilePlan::cc(
            temp.path(),
            crate::compiler::CompilerFamily::Clang,
            &[
                "-c".into(),
                "hello.cpp".into(),
                "-o".into(),
                "hello.o".into(),
                "-MD".into(),
                "-MF".into(),
                depfile.to_string_lossy().into_owned(),
            ],
            &output,
            temp.path(),
            &dep_flags,
        )
        .unwrap()
        .unwrap();
        assert_eq!(plan.outputs.len(), 2);
        assert!(plan
            .rewritten_args
            .windows(2)
            .any(|args| args[0] == "-MF" && args[1].contains(".staged-v2")));
        let rewritten =
            plan.rewrite_depfile_strategy(crate::depgraph::DepfileStrategy::UserSpecified {
                path: depfile,
            });
        assert!(matches!(
            rewritten,
            crate::depgraph::DepfileStrategy::UserSpecified { path }
                if path.as_path().starts_with(temp.path().join(".staged-v2"))
        ));
        plan.cleanup().unwrap();
    }

    #[test]
    fn cc_plan_stages_single_precompiled_header_output() {
        if !staged_tests_enabled() {
            return;
        }
        let temp = tempdir().unwrap();
        let output: NormalizedPath = temp.path().join("header.pch").into();
        let plan = StagedCompilePlan::cc(
            temp.path(),
            crate::compiler::CompilerFamily::Clang,
            &[
                "-x".into(),
                "c-header".into(),
                "header.h".into(),
                "-o".into(),
                output.to_string_lossy().into_owned(),
            ],
            &output,
            temp.path(),
            &crate::depgraph::UserDepFlags::default(),
        )
        .unwrap()
        .unwrap();
        let stage_root = plan
            .primary_staged()
            .parent()
            .and_then(Path::parent)
            .unwrap();
        let compile_dir = plan.primary_staged().parent().unwrap();
        assert_eq!(stage_root, temp.path().join(".staged-v2"));
        assert!(compile_dir
            .file_name()
            .is_some_and(|name| name.to_string_lossy().starts_with(".compile-")));
        plan.cleanup().unwrap();
    }

    #[test]
    fn msvc_plan_rewrites_fo_without_inventing_gcc_flags() {
        if !staged_tests_enabled() {
            return;
        }
        let temp = tempdir().unwrap();
        let output: NormalizedPath = temp.path().join("hello.obj").into();
        let plan = StagedCompilePlan::cc(
            temp.path(),
            crate::compiler::CompilerFamily::Msvc,
            &["/c".into(), "hello.cpp".into(), "/Fo:hello.obj".into()],
            &output,
            temp.path(),
            &crate::depgraph::UserDepFlags::default(),
        )
        .unwrap()
        .unwrap();
        assert!(plan
            .rewritten_args
            .iter()
            .any(|arg| arg.to_ascii_lowercase().starts_with("/fo")));
        assert!(!plan.rewritten_args.iter().any(|arg| arg == "-o"));
        plan.cleanup().unwrap();
    }

    #[test]
    fn archive_plan_rewrites_exact_output_token() {
        if !staged_tests_enabled() {
            return;
        }
        let temp = tempdir().unwrap();
        let output: NormalizedPath = temp.path().join("libhello.a").into();
        let plan = StagedCompilePlan::archive(
            temp.path(),
            &["rcs".into(), "libhello.a".into(), "hello.o".into()],
            &output,
            temp.path(),
        )
        .unwrap()
        .unwrap();
        assert!(plan
            .rewritten_args
            .iter()
            .any(|arg| arg.contains(".staged-v2")));
        plan.cleanup().unwrap();
    }

    #[test]
    fn link_plan_rewrites_primary_and_declared_secondary_outputs() {
        if !staged_tests_enabled() {
            return;
        }
        let temp = tempdir().unwrap();
        let primary: NormalizedPath = temp.path().join("app.exe").into();
        let import: NormalizedPath = temp.path().join("app.lib").into();
        let plan = StagedCompilePlan::link(
            temp.path(),
            &[
                "-o".into(),
                "app.exe".into(),
                "-Wl,--out-implib,app.lib".into(),
            ],
            &primary,
            std::slice::from_ref(&import),
            temp.path(),
        )
        .unwrap()
        .unwrap();
        assert_eq!(plan.outputs.len(), 2);
        assert!(plan
            .rewritten_args
            .iter()
            .any(|arg| arg.contains(".staged-v2") && arg.contains("app.exe")));
        assert!(plan
            .rewritten_args
            .iter()
            .any(|arg| arg.contains(".staged-v2") && arg.contains("app.lib")));
        std::fs::write(
            plan.primary_staged().parent().unwrap().join("implicit.pdb"),
            b"debug",
        )
        .unwrap();
        assert_eq!(plan.unexpected_staged_entries().unwrap().len(), 1);
        plan.cleanup().unwrap();
    }

    #[test]
    fn link_plan_rewrites_absolute_output_once_and_ignores_substrings() {
        if !staged_tests_enabled() {
            return;
        }
        let temp = tempdir().unwrap();
        let primary: NormalizedPath = temp.path().join("app.exe").into();
        let absolute = primary.to_string_lossy().into_owned();
        let plan = StagedCompilePlan::link(
            temp.path(),
            &[
                "-o".into(),
                absolute,
                "--trace-symbol=app.exe.helper".into(),
            ],
            &primary,
            &[],
            temp.path(),
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            plan.rewritten_args[1],
            plan.primary_staged().to_string_lossy()
        );
        assert_eq!(plan.rewritten_args[2], "--trace-symbol=app.exe.helper");
        plan.cleanup().unwrap();
    }

    #[test]
    fn link_plan_rejects_ambiguous_output_token() {
        if !staged_tests_enabled() {
            return;
        }
        let temp = tempdir().unwrap();
        let primary: NormalizedPath = temp.path().join("app.exe").into();
        let plan = StagedCompilePlan::link(
            temp.path(),
            &["-Wl,-Map,app.exe,app.exe".into()],
            &primary,
            &[],
            temp.path(),
        )
        .unwrap();
        assert!(plan.is_none());
    }

    #[test]
    fn link_plan_stages_explicit_msvc_debug_output_set() {
        if !staged_tests_enabled() {
            return;
        }
        let temp = tempdir().unwrap();
        let primary: NormalizedPath = temp.path().join("app.exe").into();
        let pdb: NormalizedPath = temp.path().join("app.pdb").into();
        let ilk: NormalizedPath = temp.path().join("app.ilk").into();
        let map: NormalizedPath = temp.path().join("app.map").into();
        let args = [
            format!("/OUT:{}", primary.display()),
            format!("/PDB:{}", pdb.display()),
            format!("/ILK:{}", ilk.display()),
            format!("/MAP:{}", map.display()),
        ];
        let plan =
            StagedCompilePlan::link(temp.path(), &args, &primary, &[pdb, ilk, map], temp.path())
                .unwrap()
                .unwrap();
        assert_eq!(plan.outputs.len(), 4);
        assert!(plan
            .rewritten_args
            .iter()
            .all(|arg| arg.contains(".staged-v2")));
        plan.cleanup().unwrap();
    }

    #[test]
    fn link_plan_rejects_implicit_msvc_side_output_before_spawn() {
        if !staged_tests_enabled() {
            return;
        }
        let temp = tempdir().unwrap();
        let primary: NormalizedPath = temp.path().join("app.exe").into();
        let implicit_pdb: NormalizedPath = temp.path().join("app.pdb").into();
        let plan = StagedCompilePlan::link(
            temp.path(),
            &["/DEBUG".into(), format!("/OUT:{}", primary.display())],
            &primary,
            &[implicit_pdb],
            temp.path(),
        )
        .unwrap();
        assert!(plan.is_none());
    }

    #[test]
    fn link_plan_rejects_semantic_gnu_map_destination() {
        if !staged_tests_enabled() {
            return;
        }
        let temp = tempdir().unwrap();
        let primary: NormalizedPath = temp.path().join("app").into();
        let semantic_map: NormalizedPath = temp.path().join("%.map").into();
        let plan = StagedCompilePlan::link(
            temp.path(),
            &[
                "-o".into(),
                primary.to_string_lossy().into_owned(),
                format!("-Map={}", semantic_map.display()),
            ],
            &primary,
            &[semantic_map],
            temp.path(),
        )
        .unwrap();
        assert!(plan.is_none());
    }

    #[test]
    fn link_plan_rejects_unmodeled_output_option_before_spawn() {
        if !staged_tests_enabled() {
            return;
        }
        let temp = tempdir().unwrap();
        let primary: NormalizedPath = temp.path().join("app.exe").into();
        let plan = StagedCompilePlan::link(
            temp.path(),
            &[
                format!("/OUT:{}", primary.display()),
                "/LTCGOUT:state/app.iobj".into(),
            ],
            &primary,
            &[],
            temp.path(),
        )
        .unwrap();
        assert!(plan.is_none());
    }

    #[test]
    fn link_plan_accepts_case_sensitive_gnu_map_option() {
        if !staged_tests_enabled() {
            return;
        }
        let temp = tempdir().unwrap();
        let primary: NormalizedPath = temp.path().join("app").into();
        let map: NormalizedPath = temp.path().join("app.map").into();
        let plan = StagedCompilePlan::link(
            temp.path(),
            &[
                "-o".into(),
                primary.to_string_lossy().into_owned(),
                "-Map".into(),
                map.to_string_lossy().into_owned(),
            ],
            &primary,
            &[map],
            temp.path(),
        )
        .unwrap();
        assert!(plan.is_some());
    }
}
