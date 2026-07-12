//! Adversarial planner and reverse-map tests for immutable staged outputs.
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
    assert!(plan.primary_staged().as_path().starts_with(temp.path()));
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
        .any(|arg| arg.contains("dep-info=") && arg.contains(".compile-")));
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
        .any(|arg| arg.starts_with("-o") && arg.contains(".compile-")));
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
        .any(|args| args[0] == "-MF" && args[1].contains(".compile-")));
    let rewritten =
        plan.rewrite_depfile_strategy(crate::depgraph::DepfileStrategy::UserSpecified {
            path: depfile,
        });
    assert!(matches!(
        rewritten,
        crate::depgraph::DepfileStrategy::UserSpecified { path }
            if path.as_path().starts_with(temp.path())
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
    assert_eq!(stage_root, temp.path());
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
        .any(|arg| arg.contains(".compile-")));
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
        .any(|arg| arg.contains(".link-") && arg.contains("app.exe")));
    assert!(plan
        .rewritten_args
        .iter()
        .any(|arg| arg.contains(".link-") && arg.contains("app.lib")));
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
    );
    assert!(matches!(
        plan,
        StagedPlanOutcome::Unsupported(StagedPlanReason::AmbiguousOutputArgument)
    ));
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
    let plan = StagedCompilePlan::link(temp.path(), &args, &primary, &[pdb, ilk, map], temp.path())
        .unwrap()
        .unwrap();
    assert_eq!(plan.outputs.len(), 4);
    assert!(plan.rewritten_args.iter().all(|arg| arg.contains(".link-")));
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
    );
    assert!(matches!(
        plan,
        StagedPlanOutcome::Unsupported(StagedPlanReason::UnsupportedOutputPath)
    ));
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
    );
    assert!(matches!(
        plan,
        StagedPlanOutcome::Unsupported(StagedPlanReason::UnmodeledSideOutput)
    ));
}

#[test]
fn planner_reason_ids_are_bounded_and_non_sensitive() {
    let profiler = crate::daemon::staged_stats::StagedProfiler::new();
    let mut ids = std::collections::HashSet::new();
    for reason in StagedPlanReason::ALL {
        let id = reason.id();
        assert!(!id.is_empty());
        assert!(id
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte == b'_'));
        assert!(!id.contains('/') && !id.contains('\\') && !id.contains(':'));
        assert!(ids.insert(id), "duplicate planner reason ID: {id}");
        profiler.failure(reason.failure());
    }
    let failures = profiler.snapshot().failures;
    for id in ids {
        assert_eq!(failures[&format!("plan_{id}")], 1);
    }
}

#[test]
fn compile_and_archive_rejections_keep_precise_reasons() {
    if !staged_tests_enabled() {
        return;
    }
    let temp = tempdir().unwrap();
    let output: NormalizedPath = temp.path().join("hello.o").into();
    assert!(matches!(
        StagedCompilePlan::rustc(
            temp.path(),
            &["--emit=link=-".into()],
            &output,
            std::slice::from_ref(&output),
            temp.path(),
        ),
        StagedPlanOutcome::Unsupported(StagedPlanReason::OutputToStdout)
    ));
    assert!(matches!(
        StagedCompilePlan::rustc(
            temp.path(),
            &["-o".into()],
            &output,
            std::slice::from_ref(&output),
            temp.path(),
        ),
        StagedPlanOutcome::Unsupported(StagedPlanReason::MissingOptionValue)
    ));
    let duplicate_a: NormalizedPath = temp.path().join("one/same.rlib").into();
    let duplicate_b: NormalizedPath = temp.path().join("two/same.rlib").into();
    assert!(matches!(
        StagedCompilePlan::rustc(
            temp.path(),
            &["--out-dir".into(), temp.path().display().to_string()],
            &duplicate_a,
            &[duplicate_a.clone(), duplicate_b],
            temp.path(),
        ),
        StagedPlanOutcome::Unsupported(StagedPlanReason::OutputNameCollision)
    ));
    assert!(matches!(
        StagedCompilePlan::cc(
            temp.path(),
            crate::compiler::CompilerFamily::Clang,
            &["-c".into(), "hello.cpp".into(), "-fmodules".into()],
            &output,
            temp.path(),
            &crate::depgraph::UserDepFlags::default(),
        ),
        StagedPlanOutcome::Unsupported(StagedPlanReason::UnmodeledSideOutput)
    ));
    let executable: NormalizedPath = temp.path().join("hello.exe").into();
    assert!(matches!(
        StagedCompilePlan::cc(
            temp.path(),
            crate::compiler::CompilerFamily::Clang,
            &["-o".into(), "hello.exe".into()],
            &executable,
            temp.path(),
            &crate::depgraph::UserDepFlags::default(),
        ),
        StagedPlanOutcome::Unsupported(StagedPlanReason::UnsupportedOutputRole)
    ));
    assert!(matches!(
        StagedCompilePlan::cc(
            temp.path(),
            crate::compiler::CompilerFamily::Msvc,
            &["/c".into(), "hello.cpp".into()],
            &output,
            temp.path(),
            &crate::depgraph::UserDepFlags::default(),
        ),
        StagedPlanOutcome::Unsupported(StagedPlanReason::MissingRequiredOutputFlag)
    ));
    assert!(matches!(
        StagedCompilePlan::cc(
            temp.path(),
            crate::compiler::CompilerFamily::Clang,
            &["-o".into()],
            &output,
            temp.path(),
            &crate::depgraph::UserDepFlags::default(),
        ),
        StagedPlanOutcome::Unsupported(StagedPlanReason::MissingOptionValue)
    ));
    let archive: NormalizedPath = temp.path().join("libhello.a").into();
    assert!(matches!(
        StagedCompilePlan::archive(
            temp.path(),
            &["rcs".into(), "different.a".into()],
            &archive,
            temp.path(),
        ),
        StagedPlanOutcome::Unsupported(StagedPlanReason::OutputNotInArguments)
    ));
}

#[test]
fn planner_errors_keep_precise_reasons() {
    if !staged_tests_enabled() {
        return;
    }
    let temp = tempdir().unwrap();
    let output: NormalizedPath = temp.path().join("hello.rlib").into();
    let blocked_root = temp.path().join("not-a-directory");
    std::fs::write(&blocked_root, b"file").unwrap();
    assert!(matches!(
        StagedCompilePlan::rustc(
            &blocked_root,
            &["-o".into(), "hello.rlib".into()],
            &output,
            std::slice::from_ref(&output),
            temp.path(),
        ),
        StagedPlanOutcome::Error(StagedPlanError {
            reason: StagedPlanReason::StagingDirectoryCreate,
            ..
        })
    ));

    let root_output: NormalizedPath = Path::new(std::path::MAIN_SEPARATOR_STR).into();
    assert!(matches!(
        StagedCompilePlan::rustc(
            temp.path(),
            &["-o".into(), root_output.to_string_lossy().into_owned()],
            &root_output,
            std::slice::from_ref(&root_output),
            temp.path(),
        ),
        StagedPlanOutcome::Error(StagedPlanError {
            reason: StagedPlanReason::OutputMissingFilename,
            ..
        })
    ));
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
