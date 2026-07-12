//! Private compiler-output staging plans.
//!
//! Planning is deliberately separate from persistence: the compiler must be
//! redirected before it starts, and unsupported output shapes must fall back
//! before spawn rather than publishing a partial set afterwards.

#[cfg(test)]
use super::staged_store::materialization_error_progress;
#[cfg(test)]
use super::staged_store::{inject_staged_fault, StagedFaultGuard, StagedFaultPoint};
use super::staged_store::{materialization_error, staged_lane_enabled, StagedMaterializationStats};
use crate::core::path::NormalizedPath;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static PLAN_COUNTER: AtomicU64 = AtomicU64::new(1);

/// Bounded planner decisions used for aggregate telemetry. These IDs are part
/// of the observability contract: never derive them from argv, paths, or OS
/// error text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::daemon::server) enum StagedPlanReason {
    LaneDisabled,
    OutputToStdout,
    OutputNameCollision,
    UnmodeledSideOutput,
    UnsupportedOutputRole,
    MissingRequiredOutputFlag,
    MissingOptionValue,
    OutputMissingFilename,
    UnsupportedOutputPath,
    AmbiguousOutputArgument,
    OutputNotInArguments,
    NoDeclaredOutputs,
    StagingDirectoryCreate,
}

impl StagedPlanReason {
    #[cfg(test)]
    pub(in crate::daemon::server) const ALL: [Self; 13] = [
        Self::LaneDisabled,
        Self::OutputToStdout,
        Self::OutputNameCollision,
        Self::UnmodeledSideOutput,
        Self::UnsupportedOutputRole,
        Self::MissingRequiredOutputFlag,
        Self::MissingOptionValue,
        Self::OutputMissingFilename,
        Self::UnsupportedOutputPath,
        Self::AmbiguousOutputArgument,
        Self::OutputNotInArguments,
        Self::NoDeclaredOutputs,
        Self::StagingDirectoryCreate,
    ];

    pub(in crate::daemon::server) const fn id(self) -> &'static str {
        match self {
            Self::LaneDisabled => "lane_disabled",
            Self::OutputToStdout => "output_to_stdout",
            Self::OutputNameCollision => "output_name_collision",
            Self::UnmodeledSideOutput => "unmodeled_side_output",
            Self::UnsupportedOutputRole => "unsupported_output_role",
            Self::MissingRequiredOutputFlag => "missing_required_output_flag",
            Self::MissingOptionValue => "missing_option_value",
            Self::OutputMissingFilename => "output_missing_filename",
            Self::UnsupportedOutputPath => "unsupported_output_path",
            Self::AmbiguousOutputArgument => "ambiguous_output_argument",
            Self::OutputNotInArguments => "output_not_in_arguments",
            Self::NoDeclaredOutputs => "no_declared_outputs",
            Self::StagingDirectoryCreate => "staging_directory_create",
        }
    }

    pub(in crate::daemon::server) const fn failure(
        self,
    ) -> crate::daemon::staged_stats::StagedFailure {
        use crate::daemon::staged_stats::StagedFailure;
        match self {
            Self::LaneDisabled => StagedFailure::PlanLaneDisabled,
            Self::OutputToStdout => StagedFailure::PlanOutputToStdout,
            Self::OutputNameCollision => StagedFailure::PlanOutputNameCollision,
            Self::UnmodeledSideOutput => StagedFailure::PlanUnmodeledSideOutput,
            Self::UnsupportedOutputRole => StagedFailure::PlanUnsupportedOutputRole,
            Self::MissingRequiredOutputFlag => StagedFailure::PlanMissingRequiredOutputFlag,
            Self::MissingOptionValue => StagedFailure::PlanMissingOptionValue,
            Self::OutputMissingFilename => StagedFailure::PlanOutputMissingFilename,
            Self::UnsupportedOutputPath => StagedFailure::PlanUnsupportedOutputPath,
            Self::AmbiguousOutputArgument => StagedFailure::PlanAmbiguousOutputArgument,
            Self::OutputNotInArguments => StagedFailure::PlanOutputNotInArguments,
            Self::NoDeclaredOutputs => StagedFailure::PlanNoDeclaredOutputs,
            Self::StagingDirectoryCreate => StagedFailure::PlanStagingDirectoryCreate,
        }
    }
}

#[derive(Debug)]
pub(in crate::daemon::server) struct StagedPlanError {
    pub(in crate::daemon::server) reason: StagedPlanReason,
    pub(in crate::daemon::server) source: io::Error,
}

#[derive(Debug)]
pub(in crate::daemon::server) enum StagedPlanOutcome<T> {
    Enabled(T),
    Unsupported(StagedPlanReason),
    Error(StagedPlanError),
}

fn planning_error(reason: StagedPlanReason, source: io::Error) -> StagedPlanError {
    StagedPlanError { reason, source }
}

fn missing_filename(reason: StagedPlanReason) -> StagedPlanError {
    planning_error(
        reason,
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "planned output has no filename",
        ),
    )
}

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
    #[cfg(test)]
    pub(in crate::daemon::server) fn for_test(
        root: PathBuf,
        outputs: Vec<StagedOutputPlan>,
    ) -> Self {
        Self {
            outputs,
            rewritten_args: Vec::new(),
            root,
        }
    }

    /// Build a plan for the narrow Phase 3 Rust lane.  A plan is absent for
    /// unsupported invocations, preserving the proven legacy path.
    pub(in crate::daemon::server) fn rustc(
        staging_dir: &Path,
        args: &[String],
        primary_output: &NormalizedPath,
        expected_outputs: &[NormalizedPath],
        cwd: &Path,
    ) -> StagedPlanOutcome<Self> {
        if !staged_lane_enabled(crate::compiler::CompilerFamily::Rustc) {
            return StagedPlanOutcome::Unsupported(StagedPlanReason::LaneDisabled);
        }
        if emit_to_stdout(args) {
            return StagedPlanOutcome::Unsupported(StagedPlanReason::OutputToStdout);
        }
        if rustc_has_missing_option_value(args) {
            return StagedPlanOutcome::Unsupported(StagedPlanReason::MissingOptionValue);
        }
        let root = staging_dir.join(format!(
            ".compile-{}-{}",
            std::process::id(),
            PLAN_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        if let Err(source) = std::fs::create_dir_all(&root) {
            return StagedPlanOutcome::Error(planning_error(
                StagedPlanReason::StagingDirectoryCreate,
                source,
            ));
        }

        let result = (|| -> Result<StagedPlanOutcome<Self>, StagedPlanError> {
            let mut primary = absolute(primary_output, cwd);
            let mut outputs = Vec::with_capacity(expected_outputs.len());
            for requested in expected_outputs {
                let requested = absolute(requested, cwd);
                let file_name = requested
                    .file_name()
                    .ok_or_else(|| missing_filename(StagedPlanReason::OutputMissingFilename))?;
                let staged = root.join(file_name);
                if outputs
                    .iter()
                    .any(|output: &StagedOutputPlan| output.staged.as_path() == staged)
                {
                    return Ok(StagedPlanOutcome::Unsupported(
                        StagedPlanReason::OutputNameCollision,
                    ));
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
                                missing_filename(StagedPlanReason::OutputMissingFilename)
                            })?)
                            .into(),
                    });
                }
            }
            let mut names = std::collections::HashSet::new();
            outputs.retain(|output| names.insert(output.requested.clone()));
            for output in &mut outputs {
                let file_name = output
                    .requested
                    .file_name()
                    .ok_or_else(|| missing_filename(StagedPlanReason::OutputMissingFilename))?;
                output.staged = root.join(file_name).into();
            }
            if outputs.iter().enumerate().any(|(index, output)| {
                outputs[..index]
                    .iter()
                    .any(|previous| previous.staged == output.staged)
            }) {
                return Ok(StagedPlanOutcome::Unsupported(
                    StagedPlanReason::OutputNameCollision,
                ));
            }
            let staged_primary = outputs
                .iter()
                .find(|output| output.requested == primary)
                .map(|output| output.staged.clone())
                .ok_or_else(|| {
                    planning_error(
                        StagedPlanReason::MissingRequiredOutputFlag,
                        io::Error::new(
                            io::ErrorKind::InvalidInput,
                            "primary output missing from Rust plan",
                        ),
                    )
                })?;

            let mut rewritten_args = args.to_vec();
            let mut replaced_output = false;
            let mut replaced_out_dir = false;
            let mut i = 0;
            while i < rewritten_args.len() {
                if rewritten_args[i] == "-o" {
                    if i + 1 >= rewritten_args.len() {
                        return Ok(StagedPlanOutcome::Unsupported(
                            StagedPlanReason::MissingOptionValue,
                        ));
                    }
                    rewritten_args[i + 1] = staged_primary.to_string_lossy().into_owned();
                    replaced_output = true;
                    i += 2;
                    continue;
                }
                if rewritten_args[i] == "--out-dir" {
                    if i + 1 >= rewritten_args.len() {
                        return Ok(StagedPlanOutcome::Unsupported(
                            StagedPlanReason::MissingOptionValue,
                        ));
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

            Ok(StagedPlanOutcome::Enabled(Self {
                outputs,
                rewritten_args,
                root: root.clone(),
            }))
        })();
        if !matches!(result, Ok(StagedPlanOutcome::Enabled(_))) {
            let _ = std::fs::remove_dir_all(&root);
        }
        result.unwrap_or_else(StagedPlanOutcome::Error)
    }

    pub(in crate::daemon::server) fn cc(
        staging_dir: &Path,
        family: crate::compiler::CompilerFamily,
        args: &[String],
        primary_output: &NormalizedPath,
        cwd: &Path,
        dep_flags: &crate::depgraph::UserDepFlags,
    ) -> StagedPlanOutcome<Self> {
        if !staged_lane_enabled(family) {
            return StagedPlanOutcome::Unsupported(StagedPlanReason::LaneDisabled);
        }
        if cc_has_unsupported_side_outputs(args) {
            return StagedPlanOutcome::Unsupported(StagedPlanReason::UnmodeledSideOutput);
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
            return StagedPlanOutcome::Unsupported(StagedPlanReason::UnsupportedOutputRole);
        }
        if family == crate::compiler::CompilerFamily::Msvc
            && !args.iter().any(|arg| {
                arg.get(..3)
                    .is_some_and(|prefix| prefix.eq_ignore_ascii_case("/fo"))
            })
        {
            return StagedPlanOutcome::Unsupported(StagedPlanReason::MissingRequiredOutputFlag);
        }
        let root = staging_dir.join(format!(
            ".compile-{}-{}",
            std::process::id(),
            PLAN_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        if let Err(source) = std::fs::create_dir_all(&root) {
            return StagedPlanOutcome::Error(planning_error(
                StagedPlanReason::StagingDirectoryCreate,
                source,
            ));
        }
        let result = (|| -> Result<StagedPlanOutcome<Self>, StagedPlanError> {
            let requested = absolute(primary_output, cwd);
            let file_name = requested
                .file_name()
                .ok_or_else(|| missing_filename(StagedPlanReason::OutputMissingFilename))?;
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
                        return Ok(StagedPlanOutcome::Unsupported(
                            StagedPlanReason::MissingOptionValue,
                        ));
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
                        return Ok(StagedPlanOutcome::Unsupported(
                            StagedPlanReason::MissingOptionValue,
                        ));
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
                            return Ok(StagedPlanOutcome::Unsupported(
                                StagedPlanReason::MissingOptionValue,
                            ));
                        }
                        let staged_depfile =
                            root.join(requested_depfile.file_name().ok_or_else(|| {
                                missing_filename(StagedPlanReason::OutputMissingFilename)
                            })?);
                        rewritten_args[i + 1] = staged_depfile.to_string_lossy().into_owned();
                        i += 2;
                        continue;
                    }
                    if let Some(value) = rewritten_args[i].strip_prefix("-MF") {
                        if !value.is_empty() {
                            let staged_depfile =
                                root.join(requested_depfile.file_name().ok_or_else(|| {
                                    missing_filename(StagedPlanReason::OutputMissingFilename)
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
                return Ok(StagedPlanOutcome::Unsupported(
                    StagedPlanReason::MissingRequiredOutputFlag,
                ));
            }
            if !replaced {
                rewritten_args.push("-o".to_string());
                rewritten_args.push(staged.to_string_lossy().into_owned());
            }
            let mut outputs = vec![StagedOutputPlan { requested, staged }];
            if let Some(requested_depfile) = requested_depfile {
                let staged_depfile =
                    root.join(requested_depfile.file_name().ok_or_else(|| {
                        missing_filename(StagedPlanReason::OutputMissingFilename)
                    })?);
                outputs.push(StagedOutputPlan {
                    requested: requested_depfile,
                    staged: staged_depfile.into(),
                });
            }
            Ok(StagedPlanOutcome::Enabled(Self {
                outputs,
                rewritten_args,
                root: root.clone(),
            }))
        })();
        if !matches!(result, Ok(StagedPlanOutcome::Enabled(_))) {
            let _ = std::fs::remove_dir_all(&root);
        }
        result.unwrap_or_else(StagedPlanOutcome::Error)
    }

    /// Stage a pure archive invocation. Linkers with secondary side outputs
    /// use a separate planner because silently omitting a PDB/import library
    /// would violate complete-set publication.
    pub(in crate::daemon::server) fn archive(
        staging_dir: &Path,
        args: &[String],
        primary_output: &NormalizedPath,
        cwd: &Path,
    ) -> StagedPlanOutcome<Self> {
        if !staged_lane_enabled(crate::compiler::CompilerFamily::Gcc) {
            return StagedPlanOutcome::Unsupported(StagedPlanReason::LaneDisabled);
        }
        let root = staging_dir.join(format!(
            ".compile-{}-{}",
            std::process::id(),
            PLAN_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        if let Err(source) = std::fs::create_dir_all(&root) {
            return StagedPlanOutcome::Error(planning_error(
                StagedPlanReason::StagingDirectoryCreate,
                source,
            ));
        }
        let result = (|| -> Result<StagedPlanOutcome<Self>, StagedPlanError> {
            let requested = absolute(primary_output, cwd);
            let file_name = requested
                .file_name()
                .ok_or_else(|| missing_filename(StagedPlanReason::OutputMissingFilename))?;
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
                return Ok(StagedPlanOutcome::Unsupported(
                    StagedPlanReason::OutputNotInArguments,
                ));
            }
            Ok(StagedPlanOutcome::Enabled(Self {
                outputs: vec![StagedOutputPlan { requested, staged }],
                rewritten_args,
                root: root.clone(),
            }))
        })();
        if !matches!(result, Ok(StagedPlanOutcome::Enabled(_))) {
            let _ = std::fs::remove_dir_all(&root);
        }
        result.unwrap_or_else(StagedPlanOutcome::Error)
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
    ) -> StagedPlanOutcome<Self> {
        if !super::staged_link_lane_enabled() {
            return StagedPlanOutcome::Unsupported(StagedPlanReason::LaneDisabled);
        }
        if has_unmodeled_link_output_option(args) {
            return StagedPlanOutcome::Unsupported(StagedPlanReason::UnmodeledSideOutput);
        }
        let root = staging_dir.join(format!(
            ".link-{}-{}",
            std::process::id(),
            PLAN_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        if let Err(source) = std::fs::create_dir_all(&root) {
            return StagedPlanOutcome::Error(planning_error(
                StagedPlanReason::StagingDirectoryCreate,
                source,
            ));
        }
        let result = (|| -> Result<StagedPlanOutcome<Self>, StagedPlanError> {
            let mut requested_outputs = Vec::with_capacity(1 + secondary_outputs.len());
            requested_outputs.push(absolute(primary_output, cwd));
            requested_outputs.extend(secondary_outputs.iter().map(|output| absolute(output, cwd)));
            let mut outputs = Vec::with_capacity(requested_outputs.len());
            let mut rewritten_args = args.to_vec();
            for requested in requested_outputs {
                let requested_text = requested.to_string_lossy();
                if requested_text.contains('%') || requested.as_path().is_dir() {
                    return Ok(StagedPlanOutcome::Unsupported(
                        StagedPlanReason::UnsupportedOutputPath,
                    ));
                }
                let filename = requested
                    .file_name()
                    .ok_or_else(|| missing_filename(StagedPlanReason::OutputMissingFilename))?;
                let staged: NormalizedPath = root.join(filename).into();
                if outputs.iter().any(|output: &StagedOutputPlan| {
                    output.requested == requested || output.staged == staged
                }) {
                    return Ok(StagedPlanOutcome::Unsupported(
                        StagedPlanReason::OutputNameCollision,
                    ));
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
                        None => {
                            return Ok(StagedPlanOutcome::Unsupported(
                                StagedPlanReason::AmbiguousOutputArgument,
                            ));
                        }
                    }
                }
                if !replaced {
                    return Ok(StagedPlanOutcome::Unsupported(
                        StagedPlanReason::OutputNotInArguments,
                    ));
                }
                outputs.push(StagedOutputPlan { requested, staged });
            }
            Ok(StagedPlanOutcome::Enabled(Self {
                outputs,
                rewritten_args,
                root: root.clone(),
            }))
        })();
        if !matches!(result, Ok(StagedPlanOutcome::Enabled(_))) {
            let _ = std::fs::remove_dir_all(&root);
        }
        result.unwrap_or_else(StagedPlanOutcome::Error)
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
        for (fault_index, output) in self.outputs.iter().enumerate() {
            #[cfg(not(test))]
            let _ = fault_index;
            #[cfg(test)]
            {
                inject_staged_fault(
                    output.requested.as_path(),
                    StagedFaultPoint::MaterializeOutput(fault_index),
                )
                .map_err(|error| materialization_error(error, stats))?;
            }
            if let Some(parent) = output.requested.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|error| materialization_error(error, stats))?;
            }
            let output_stats = crate::daemon::server::persist::materialize_independent_with_stats(
                output.staged.as_path(),
                output.requested.as_path(),
            )
            .map_err(|error| {
                materialization_error(
                    io::Error::new(
                        error.kind(),
                        format!(
                            "{} -> {}: {error}",
                            output.staged.display(),
                            output.requested.display()
                        ),
                    ),
                    stats,
                )
            })?;
            stats.reflink_count = stats
                .reflink_count
                .saturating_add(output_stats.reflink_count);
            stats.copy_count = stats.copy_count.saturating_add(output_stats.copy_count);
            stats.copy_bytes = stats.copy_bytes.saturating_add(output_stats.copy_bytes);
        }
        self.cleanup()
            .map_err(|error| materialization_error(error, stats))?;
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

fn rustc_has_missing_option_value(args: &[String]) -> bool {
    args.iter().enumerate().any(|(index, arg)| {
        if matches!(arg.as_str(), "-o" | "--out-dir" | "--emit") {
            return args.get(index + 1).is_none_or(String::is_empty);
        }
        arg.strip_prefix("--out-dir=").is_some_and(str::is_empty)
            || arg.strip_prefix("--emit=").is_some_and(|value| {
                value.is_empty()
                    || value.split(',').any(|part| {
                        part.split_once('=')
                            .is_some_and(|(_, path)| path.is_empty())
                    })
            })
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
#[path = "staged_plan_tests.rs"]
mod tests;
