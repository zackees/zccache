//! Allowed-class gating, proc-macro vs shared-library heuristics, and
//! package-id-based exclusion of workspace/path dependencies. Also
//! covers `package_name_from_id` parsing.

use super::super::*;
use super::{
    sample_plan, synthetic_target, synthetic_target_with_package_exclusions,
    synthetic_target_with_proc_macro_outputs,
};

#[test]
fn thin_plan_respects_explicit_class_gates_for_dependency_metadata_and_outputs() {
    let dir = tempfile::tempdir().unwrap();
    synthetic_target(dir.path());
    let mut plan = sample_plan(dir.path(), RustPlanMode::Thin);
    plan.allowed_artifact_classes = vec![RustArtifactClass::Rlib, RustArtifactClass::Rmeta];
    let cache = dir.path().join("cache");

    let saved = save_rust_plan_local(&plan, &cache).unwrap();
    assert_eq!(saved.saved_file_count, 2);
    assert_eq!(
        saved
            .skipped_reasons
            .get("artifact_class_disallowed_by_plan"),
        Some(&4)
    );
    assert_eq!(
        saved
            .skipped_reasons
            .get("workspace_or_path_dependency_excluded_by_plan"),
        Some(&2)
    );
    assert!(saved
        .skipped_samples
        .iter()
        .any(|sample| sample.path.ends_with("debug/deps/serde-abc.d")));
    assert!(saved.skipped_samples.iter().any(|sample| sample
        .path
        .ends_with("debug/.fingerprint/serde-abc/dep-lib-serde")));
    assert!(saved.skipped_samples.iter().any(|sample| sample
        .path
        .ends_with("debug/build/serde-abc/invoked.timestamp")));
    assert!(saved
        .skipped_samples
        .iter()
        .any(|sample| sample.path.ends_with("debug/build/serde-abc/out/gen.rs")));
}

#[test]
fn thin_plan_saves_and_restores_likely_proc_macro_dylibs_without_shared_libs() {
    let dir = tempfile::tempdir().unwrap();
    synthetic_target_with_proc_macro_outputs(dir.path());
    let mut plan = sample_plan(dir.path(), RustPlanMode::Thin);
    plan.allowed_artifact_classes = vec![
        RustArtifactClass::Rlib,
        RustArtifactClass::Rmeta,
        RustArtifactClass::DepInfo,
        RustArtifactClass::ProcMacro,
        RustArtifactClass::CargoFingerprint,
        RustArtifactClass::BuildScriptMetadata,
        RustArtifactClass::BuildScriptOutput,
    ];
    let cache = dir.path().join("cache");

    let saved = save_rust_plan_local(&plan, &cache).unwrap();
    assert_eq!(saved.saved_file_count, 7);
    assert_eq!(
        saved
            .skipped_reasons
            .get("artifact_class_disallowed_by_plan"),
        Some(&1)
    );
    assert!(saved.skipped_samples.iter().any(|sample| {
        sample
            .path
            .ends_with("debug/deps/libserde_shared-def456.dll")
            || sample
                .path
                .ends_with("debug/deps/libserde_shared-def456.so")
    }));

    std::fs::remove_dir_all(plan.target_dir.as_path()).unwrap();
    let restored = restore_rust_plan_local(&plan, &cache).unwrap();
    assert_eq!(restored.restored_file_count, 7);
    assert!(plan
        .target_dir
        .join(if cfg!(windows) {
            "debug/deps/libproc_macro2-def456.dll"
        } else {
            "debug/deps/libproc_macro2-def456.so"
        })
        .exists());
    assert!(!plan
        .target_dir
        .join(if cfg!(windows) {
            "debug/deps/libserde_shared-def456.dll"
        } else {
            "debug/deps/libserde_shared-def456.so"
        })
        .exists());
}

#[test]
fn thin_plan_skips_likely_proc_macro_dylibs_when_disallowed() {
    let dir = tempfile::tempdir().unwrap();
    synthetic_target_with_proc_macro_outputs(dir.path());
    let mut plan = sample_plan(dir.path(), RustPlanMode::Thin);
    plan.allowed_artifact_classes = vec![
        RustArtifactClass::Rlib,
        RustArtifactClass::Rmeta,
        RustArtifactClass::DepInfo,
        RustArtifactClass::SharedLib,
        RustArtifactClass::CargoFingerprint,
        RustArtifactClass::BuildScriptMetadata,
        RustArtifactClass::BuildScriptOutput,
    ];
    let cache = dir.path().join("cache");

    let saved = save_rust_plan_local(&plan, &cache).unwrap();
    assert_eq!(saved.saved_file_count, 7);
    assert_eq!(
        saved
            .skipped_reasons
            .get("artifact_class_disallowed_by_plan"),
        Some(&1)
    );
    assert!(saved.skipped_samples.iter().any(|sample| {
        sample
            .path
            .ends_with("debug/deps/libproc_macro2-def456.dll")
            || sample.path.ends_with("debug/deps/libproc_macro2-def456.so")
    }));
}

#[test]
fn package_name_parsing_handles_cargo_package_id_shapes() {
    assert_eq!(
        package_name_from_id("registry+https://github.com/rust-lang/crates.io-index#serde@1.0.0"),
        Some("serde".to_string())
    );
    assert_eq!(
        package_name_from_id("path+file:///repo#my-crate@0.1.0"),
        Some("my_crate".to_string())
    );
}

#[test]
fn thin_package_exclusions_match_deps_fingerprint_and_build_by_package_stem() {
    let dir = tempfile::tempdir().unwrap();
    synthetic_target_with_package_exclusions(dir.path());
    let mut plan = sample_plan(dir.path(), RustPlanMode::Thin);
    plan.packages.workspace_package_ids =
        vec!["registry+https://github.com/rust-lang/crates.io-index#app@0.1.0".to_string()];
    plan.packages.excluded_path_package_ids =
        vec!["path+file:///workspace/local_dep#local-dep@0.1.0".to_string()];
    let cache = dir.path().join("cache");

    let saved = save_rust_plan_local(&plan, &cache).unwrap();
    assert_eq!(saved.saved_file_count, 6);
    assert_eq!(
        saved
            .skipped_reasons
            .get("workspace_or_path_dependency_excluded_by_plan"),
        Some(&12)
    );
    assert_eq!(saved.skipped_reasons.get("transient_state"), Some(&1));
    assert!(saved
        .skipped_samples
        .iter()
        .any(|sample| sample.path.ends_with("debug/deps/libapp-abc.rlib")));
    assert!(saved.skipped_samples.iter().any(|sample| sample
        .path
        .ends_with("debug/.fingerprint/app-abc/dep-lib-app")));
    assert!(saved.skipped_samples.iter().any(|sample| sample
        .path
        .ends_with("debug/build/local_dep-abc/out/gen.rs")));
}
