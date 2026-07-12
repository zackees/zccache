//! Private staging plan for one unit of a multi-source C/C++ invocation.

use super::*;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static MULTI_PLAN_COUNTER: AtomicU64 = AtomicU64::new(1);

#[derive(Debug)]
pub(in crate::daemon::server) struct StagedMultiUnitPlan {
    pub(in crate::daemon::server) rewritten_args: Vec<String>,
    pub(in crate::daemon::server) depfile: NormalizedPath,
    pub(in crate::daemon::server) outputs: Vec<StagedOutputPlan>,
    pub(in crate::daemon::server) msvc_syntax: bool,
    root: PathBuf,
}

impl StagedMultiUnitPlan {
    pub(in crate::daemon::server) fn build(
        staging_dir: &Path,
        family: crate::compiler::CompilerFamily,
        args: Vec<String>,
        requested_output: &NormalizedPath,
        cwd: &Path,
    ) -> StagedPlanOutcome<Self> {
        if !staged_lane_enabled(family) {
            return StagedPlanOutcome::Unsupported(StagedPlanReason::LaneDisabled);
        }
        if !matches!(
            family,
            crate::compiler::CompilerFamily::Gcc
                | crate::compiler::CompilerFamily::Clang
                | crate::compiler::CompilerFamily::Msvc
        ) {
            return StagedPlanOutcome::Unsupported(StagedPlanReason::UnsupportedOutputRole);
        }
        let msvc_syntax = family == crate::compiler::CompilerFamily::Msvc
            || crate::compiler::parse_msvc::looks_like_msvc_args(&args);
        if has_unmodeled_multi_output(&args, msvc_syntax) {
            return StagedPlanOutcome::Unsupported(StagedPlanReason::UnmodeledSideOutput);
        }
        if (!msvc_syntax && has_gnu_explicit_output(&args))
            || (msvc_syntax && has_file_valued_msvc_output(&args))
        {
            return StagedPlanOutcome::Unsupported(StagedPlanReason::AmbiguousOutputArgument);
        }

        let requested = if requested_output.is_absolute() {
            requested_output.clone()
        } else {
            cwd.join(requested_output).into()
        };
        let Some(filename) = requested.file_name() else {
            return StagedPlanOutcome::Error(StagedPlanError {
                reason: StagedPlanReason::OutputMissingFilename,
                source: std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "multi-source output has no filename",
                ),
            });
        };
        let root = staging_dir.join(format!(
            ".multi-{}-{}",
            std::process::id(),
            MULTI_PLAN_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        if let Err(source) = std::fs::create_dir_all(&root) {
            return StagedPlanOutcome::Error(StagedPlanError {
                reason: StagedPlanReason::StagingDirectoryCreate,
                source,
            });
        }
        let staged: NormalizedPath = root.join(filename).into();
        let depfile: NormalizedPath = root.join("unit.d").into();
        let user_depfile = args
            .iter()
            .any(|arg| matches!(arg.as_str(), "-MD" | "-MMD"));
        let rewritten_args = if msvc_syntax {
            rewrite_msvc_output(args, &staged)
        } else {
            let mut rewritten = args;
            let mut injected = vec!["-o".to_string(), staged.to_string_lossy().into_owned()];
            if !user_depfile {
                injected.push("-MD".to_string());
            }
            injected.extend(["-MF".to_string(), depfile.to_string_lossy().into_owned()]);
            if !has_depfile_target(&rewritten) {
                injected.extend(["-MT".to_string(), requested.to_string_lossy().into_owned()]);
            }
            let insert_at = rewritten
                .iter()
                .position(|argument| argument == "--")
                .unwrap_or(rewritten.len());
            rewritten.splice(insert_at..insert_at, injected);
            rewritten
        };
        let mut outputs = vec![StagedOutputPlan { requested, staged }];
        if user_depfile {
            outputs.push(StagedOutputPlan {
                requested: outputs[0].requested.with_extension("d").into(),
                staged: depfile.clone(),
            });
        }
        StagedPlanOutcome::Enabled(Self {
            rewritten_args,
            depfile,
            outputs,
            msvc_syntax,
            root,
        })
    }

    pub(in crate::daemon::server) fn materialize(
        &self,
    ) -> std::io::Result<StagedMaterializationStats> {
        let mut observed = StagedMaterializationStats::default();
        for (fault_index, output) in self.outputs.iter().enumerate() {
            #[cfg(not(test))]
            let _ = fault_index;
            #[cfg(test)]
            {
                inject_staged_fault(
                    output.requested.as_path(),
                    StagedFaultPoint::MaterializeOutput(fault_index),
                )
                .map_err(|error| materialization_error(error, observed))?;
            }
            if let Some(parent) = output.requested.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|error| materialization_error(error, observed))?;
            }
            let output_stats = materialize_independent_with_stats(
                output.staged.as_path(),
                output.requested.as_path(),
            )
            .map_err(|error| materialization_error(error, observed))?;
            observed.reflink_count = observed
                .reflink_count
                .saturating_add(output_stats.reflink_count);
            observed.copy_count = observed.copy_count.saturating_add(output_stats.copy_count);
            observed.copy_bytes = observed.copy_bytes.saturating_add(output_stats.copy_bytes);
        }
        self.cleanup()
            .map_err(|error| materialization_error(error, observed))?;
        Ok(observed)
    }

    pub(in crate::daemon::server) fn validated_output_sizes(&self) -> std::io::Result<Vec<u64>> {
        self.outputs
            .iter()
            .map(|output| match std::fs::metadata(&output.staged) {
                Ok(metadata) if metadata.is_file() && metadata.len() > 0 => Ok(metadata.len()),
                Ok(metadata) if metadata.is_file() => Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!(
                        "compiler produced empty output {}",
                        output.requested.display()
                    ),
                )),
                Ok(_) => Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!(
                        "compiler produced non-file output {}",
                        output.requested.display()
                    ),
                )),
                Err(error) => Err(std::io::Error::new(
                    error.kind(),
                    format!("compiler omitted {}: {error}", output.requested.display()),
                )),
            })
            .collect()
    }

    fn cleanup(&self) -> std::io::Result<()> {
        std::fs::remove_dir_all(&self.root).or_else(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                Ok(())
            } else {
                Err(error)
            }
        })
    }
}

fn has_gnu_explicit_output(args: &[String]) -> bool {
    args.iter().any(|arg| {
        arg == "-o"
            || (arg.starts_with("-o") && arg.len() > 2)
            || arg == "--output"
            || arg.starts_with("--output=")
            || arg == "-MF"
            || (arg.starts_with("-MF") && arg.len() > 3)
    })
}

fn has_depfile_target(args: &[String]) -> bool {
    args.iter().any(|arg| {
        matches!(arg.as_str(), "-MT" | "-MQ")
            || (arg.starts_with("-MT") && arg.len() > 3)
            || (arg.starts_with("-MQ") && arg.len() > 3)
    })
}

fn msvc_fo_value<'a>(arg: &'a str, next: Option<&'a str>) -> Option<&'a str> {
    let body = arg.strip_prefix('/').or_else(|| arg.strip_prefix('-'))?;
    let rest = body.strip_prefix("Fo")?;
    if rest.is_empty() {
        next
    } else {
        Some(rest.strip_prefix(':').unwrap_or(rest))
    }
}

fn has_file_valued_msvc_output(args: &[String]) -> bool {
    args.iter().enumerate().any(|(index, arg)| {
        msvc_fo_value(arg, args.get(index + 1).map(String::as_str))
            .is_some_and(|path| !path.ends_with('/') && !path.ends_with('\\'))
            || arg == "-o"
            || (arg.starts_with("-o") && arg.len() > 2)
    })
}

fn has_unmodeled_multi_output(args: &[String], msvc_syntax: bool) -> bool {
    args.iter().any(|arg| {
        let lower = arg.to_ascii_lowercase();
        if msvc_syntax {
            [
                "/fd",
                "-fd",
                "/fp",
                "-fp",
                "/fa",
                "-fa",
                "/fr",
                "-fr",
                "/yc",
                "-yc",
                "/doc",
                "-doc",
                "/module:",
                "-module:",
                "/headerunit",
                "-headerunit",
                "/sourcedependencies",
                "-sourcedependencies",
            ]
            .iter()
            .any(|prefix| lower.starts_with(prefix))
                || arg.starts_with("/Fi")
                || arg.starts_with("-Fi")
                || matches!(lower.as_str(), "/zi" | "-zi")
                || lower.starts_with("/ifc")
                || lower.starts_with("-ifc")
                || lower == "/interface"
                || lower == "-interface"
        } else {
            lower.starts_with("--serialize-diagnostics")
                || lower.starts_with("-mj")
                || lower.starts_with("-fmodule")
                || lower == "-save-temps"
                || lower.starts_with("-save-temps=")
                || lower == "-gsplit-dwarf"
                || lower.starts_with("-fdump-")
                || lower == "--coverage"
                || lower == "-coverage"
                || lower == "-fprofile-arcs"
                || lower == "-ftest-coverage"
                || lower == "-ftime-trace"
                || lower.starts_with("-ftime-trace=")
                || lower == "-fstack-usage"
                || lower.starts_with("-fcallgraph-info")
                || lower == "-fsave-optimization-record"
                || lower.starts_with("-foptimization-record-file=")
                || lower.starts_with("-fopt-info")
                || lower.starts_with("-fdiagnostics-file=")
                || lower.starts_with("-fdiagnostics-format=sarif-file")
                || (lower.starts_with("-wa,") && lower.contains("="))
                || matches!(
                    lower.as_str(),
                    "c++-module"
                        | "c-header"
                        | "c++-header"
                        | "objective-c-header"
                        | "objective-c++-header"
                        | "c-header-unit"
                        | "c++-header-unit"
                )
                || lower.ends_with(".cppm")
                || lower.ends_with(".ixx")
        }
    })
}

fn rewrite_msvc_output(args: Vec<String>, staged: &NormalizedPath) -> Vec<String> {
    let private = staged.to_string_lossy();
    let mut rewritten = Vec::with_capacity(args.len() + 1);
    let mut index = 0;
    let mut replaced = false;
    while index < args.len() {
        let arg = &args[index];
        if msvc_fo_value(arg, args.get(index + 1).map(String::as_str)).is_some() {
            rewritten.push(format!("/Fo{private}"));
            replaced = true;
            if arg.eq_ignore_ascii_case("/Fo") || arg.eq_ignore_ascii_case("-Fo") {
                index += 2;
            } else {
                index += 1;
            }
            continue;
        }
        rewritten.push(arg.clone());
        index += 1;
    }
    if !replaced {
        rewritten.push(format!("/Fo{private}"));
    }
    rewritten
}

impl Drop for StagedMultiUnitPlan {
    fn drop(&mut self) {
        let _ = self.cleanup();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn multi_unit_plan_keeps_one_source_and_redirects_private_outputs() {
        let temp = tempfile::tempdir().unwrap();
        let requested: NormalizedPath = temp.path().join("first.o").into();
        let plan = match StagedMultiUnitPlan::build(
            temp.path(),
            crate::compiler::CompilerFamily::Clang,
            vec!["-c".into(), "first.c".into(), "-Iinc".into()],
            &requested,
            temp.path(),
        ) {
            StagedPlanOutcome::Enabled(plan) => plan,
            other => panic!("expected enabled plan, got {other:?}"),
        };
        assert!(plan.rewritten_args.iter().any(|arg| arg == "first.c"));
        assert!(plan.rewritten_args.iter().any(|arg| arg == "-Iinc"));
        assert!(plan.rewritten_args.iter().any(|arg| arg == "-MF"));
        assert!(plan.outputs[0].staged.starts_with(temp.path()));
        assert_ne!(plan.outputs[0].staged, requested);
    }

    #[test]
    fn multi_unit_plan_rejects_shared_explicit_depfile() {
        let temp = tempfile::tempdir().unwrap();
        let requested: NormalizedPath = temp.path().join("first.o").into();
        assert!(matches!(
            StagedMultiUnitPlan::build(
                temp.path(),
                crate::compiler::CompilerFamily::Clang,
                vec!["-c".into(), "first.c".into(), "-MFshared.d".into()],
                &requested,
                temp.path(),
            ),
            StagedPlanOutcome::Unsupported(StagedPlanReason::AmbiguousOutputArgument)
        ));
    }

    #[test]
    fn multi_unit_plan_injects_output_flags_before_end_of_options() {
        let temp = tempfile::tempdir().unwrap();
        let requested: NormalizedPath = temp.path().join("dash.o").into();
        let plan = match StagedMultiUnitPlan::build(
            temp.path(),
            crate::compiler::CompilerFamily::Clang,
            vec!["-c".into(), "--".into(), "-dash.c".into()],
            &requested,
            temp.path(),
        ) {
            StagedPlanOutcome::Enabled(plan) => plan,
            other => panic!("expected enabled plan, got {other:?}"),
        };

        let end_of_options = plan
            .rewritten_args
            .iter()
            .position(|argument| argument == "--")
            .unwrap();
        let output_flag = plan
            .rewritten_args
            .iter()
            .position(|argument| argument == "-o")
            .unwrap();
        let source = plan
            .rewritten_args
            .iter()
            .position(|argument| argument == "-dash.c")
            .unwrap();
        assert!(output_flag < end_of_options);
        assert!(source > end_of_options);
    }

    #[test]
    fn multi_unit_plan_rejects_gnu_explicit_batch_output() {
        let temp = tempfile::tempdir().unwrap();
        let requested: NormalizedPath = temp.path().join("first.o").into();
        for output_args in [
            vec!["-o", "batch.o"],
            vec!["-obatch.o"],
            vec!["--output", "batch.o"],
            vec!["--output=batch.o"],
        ] {
            let mut args = vec!["-c".to_string(), "first.c".to_string()];
            args.extend(output_args.into_iter().map(str::to_string));
            assert!(matches!(
                StagedMultiUnitPlan::build(
                    temp.path(),
                    crate::compiler::CompilerFamily::Clang,
                    args,
                    &requested,
                    temp.path(),
                ),
                StagedPlanOutcome::Unsupported(StagedPlanReason::AmbiguousOutputArgument)
            ));
        }
    }

    #[test]
    fn multi_unit_plan_rejects_gnu_shared_and_phase_six_outputs() {
        let temp = tempfile::tempdir().unwrap();
        let requested: NormalizedPath = temp.path().join("first.o").into();
        for side_output in [
            "--serialize-diagnostics=shared.dia",
            "-MJshared.json",
            "-fmodule-file=math.pcm",
            "-save-temps",
            "-gsplit-dwarf",
            "-fdump-tree-all",
            "--coverage",
            "-ftime-trace",
            "-fstack-usage",
            "-fsave-optimization-record",
            "-fopt-info-vec=optimization.txt",
            "-fdiagnostics-format=sarif-file",
            "-Wa,-adhln=listing.txt",
            "c++-header",
            "interface.cppm",
        ] {
            assert!(matches!(
                StagedMultiUnitPlan::build(
                    temp.path(),
                    crate::compiler::CompilerFamily::Clang,
                    vec!["-c".into(), "first.c".into(), side_output.into()],
                    &requested,
                    temp.path(),
                ),
                StagedPlanOutcome::Unsupported(StagedPlanReason::UnmodeledSideOutput)
            ));
        }
    }

    #[test]
    fn multi_unit_plan_models_default_user_depfile() {
        let temp = tempfile::tempdir().unwrap();
        let requested: NormalizedPath = temp.path().join("obj/first.o").into();
        let plan = match StagedMultiUnitPlan::build(
            temp.path(),
            crate::compiler::CompilerFamily::Clang,
            vec!["-c".into(), "first.c".into(), "-MMD".into()],
            &requested,
            temp.path(),
        ) {
            StagedPlanOutcome::Enabled(plan) => plan,
            other => panic!("expected enabled plan, got {other:?}"),
        };
        assert_eq!(plan.outputs.len(), 2);
        assert_eq!(plan.outputs[1].requested, requested.with_extension("d"));
        assert_eq!(plan.outputs[1].staged, plan.depfile);
        assert!(!plan.rewritten_args.iter().any(|arg| arg == "-MD"));
    }

    #[test]
    fn multi_unit_plan_rewrites_msvc_fo_directory() {
        let temp = tempfile::tempdir().unwrap();
        let requested: NormalizedPath = temp.path().join("obj/first.obj").into();
        let plan = match StagedMultiUnitPlan::build(
            temp.path(),
            crate::compiler::CompilerFamily::Msvc,
            vec![
                "/c".into(),
                "first.c".into(),
                "/FIforced.h".into(),
                "/Foobj\\".into(),
            ],
            &requested,
            temp.path(),
        ) {
            StagedPlanOutcome::Enabled(plan) => plan,
            other => panic!("expected enabled plan, got {other:?}"),
        };
        assert!(plan
            .rewritten_args
            .iter()
            .any(|arg| arg.starts_with("/Fo") && arg.contains(".multi-")));
        assert!(!plan.rewritten_args.iter().any(|arg| arg == "/Foobj\\"));
        assert!(plan.rewritten_args.iter().any(|arg| arg == "/FIforced.h"));
    }

    #[test]
    fn multi_unit_plan_rejects_msvc_shared_side_outputs() {
        let temp = tempfile::tempdir().unwrap();
        let requested: NormalizedPath = temp.path().join("first.obj").into();
        for side_output in [
            "/Fdshared.pdb",
            "/Fpshared.pch",
            "/Fashared.asm",
            "/Zi",
            "/Ycstdafx.h",
            "/FRbrowse.sbr",
            "/doccomments.xdc",
            "/module:outputmod.ifc",
            "/sourceDependenciesdeps.json",
        ] {
            assert!(matches!(
                StagedMultiUnitPlan::build(
                    temp.path(),
                    crate::compiler::CompilerFamily::Msvc,
                    vec!["/c".into(), "first.c".into(), side_output.into()],
                    &requested,
                    temp.path(),
                ),
                StagedPlanOutcome::Unsupported(StagedPlanReason::UnmodeledSideOutput)
            ));
        }
    }

    #[test]
    fn multi_unit_plan_rejects_empty_output_before_publication() {
        let temp = tempfile::tempdir().unwrap();
        let requested: NormalizedPath = temp.path().join("first.o").into();
        let plan = match StagedMultiUnitPlan::build(
            temp.path(),
            crate::compiler::CompilerFamily::Clang,
            vec!["-c".into(), "first.c".into()],
            &requested,
            temp.path(),
        ) {
            StagedPlanOutcome::Enabled(plan) => plan,
            other => panic!("expected enabled plan, got {other:?}"),
        };
        std::fs::write(&plan.outputs[0].staged, []).unwrap();

        let error = plan.validated_output_sizes().unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("empty output"));
    }
}
