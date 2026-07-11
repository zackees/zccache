//! Thin and full save/restore happy paths: dependency selection, mtime
//! preservation, final-binary skipping, mismatched-bundle warnings.

use super::super::*;
use super::{
    file_mtime_nanos, load_manifest, sample_plan, set_mtime_nanos, synthetic_target,
    synthetic_target_with_final_binary, write_manifest,
};

#[test]
fn thin_save_restore_selects_dependency_artifacts_and_metadata() {
    let dir = tempfile::tempdir().unwrap();
    synthetic_target(dir.path());
    let plan = sample_plan(dir.path(), RustPlanMode::Thin);
    let cache = dir.path().join("cache");

    let saved = save_rust_plan_local(&plan, &cache).unwrap();
    assert_eq!(saved.saved_file_count, 6);
    assert_eq!(
        saved
            .skipped_reasons
            .get("workspace_or_path_dependency_excluded_by_plan"),
        Some(&2)
    );
    assert_eq!(saved.skipped_reasons.get("transient_state"), Some(&1));

    std::fs::remove_dir_all(plan.target_dir.as_path()).unwrap();
    let restored = restore_rust_plan_local(&plan, &cache).unwrap();
    assert_eq!(restored.restored_file_count, 6);
    assert!(plan
        .target_dir
        .join("debug/deps/libserde-abc.rlib")
        .exists());
    assert!(plan
        .target_dir
        .join("debug/.fingerprint/serde-abc/dep-lib-serde")
        .exists());
    assert!(!plan.target_dir.join("debug/deps/libapp-abc.rlib").exists());
    assert!(!plan.target_dir.join("debug/incremental/state.bin").exists());
}

#[test]
fn complete_cargo_closure_avoids_unreported_target_files() {
    let dir = tempfile::tempdir().unwrap();
    synthetic_target(dir.path());
    let plan = RustArtifactPlanV1 {
        cargo_artifact_paths: vec![
            "debug/deps/libserde-abc.rlib".to_string(),
            "debug/.fingerprint/serde-abc/dep-lib-serde".to_string(),
        ],
        cargo_artifacts_complete: true,
        ..sample_plan(dir.path(), RustPlanMode::Thin)
    };
    let cache = dir.path().join("cache");

    let saved = save_rust_plan_local(&plan, &cache).unwrap();
    assert_eq!(saved.saved_file_count, 2);
    let manifest = load_manifest(&rust_plan_bundle_dir(&cache, &rust_plan_cache_key(&plan)));
    assert!(manifest
        .artifacts
        .iter()
        .all(|artifact| artifact.relative_path != "debug/deps/libapp-abc.rlib"));
}

#[test]
fn invalid_cargo_closure_falls_back_to_recursive_walk() {
    let dir = tempfile::tempdir().unwrap();
    synthetic_target(dir.path());
    let plan = RustArtifactPlanV1 {
        cargo_artifact_paths: vec!["../outside.rlib".to_string()],
        cargo_artifacts_complete: true,
        ..sample_plan(dir.path(), RustPlanMode::Thin)
    };
    let cache = dir.path().join("cache");

    let saved = save_rust_plan_local(&plan, &cache).unwrap();
    assert_eq!(saved.saved_file_count, 6);
}

#[test]
fn save_writes_protobuf_manifest_and_restore_preserves_mtime() {
    let dir = tempfile::tempdir().unwrap();
    synthetic_target(dir.path());
    let plan = sample_plan(dir.path(), RustPlanMode::Thin);
    let cache = dir.path().join("cache");
    let selected_file = plan.target_dir.join("debug/deps/libserde-abc.rlib");
    let expected_mtime = 1_700_000_000_000_000_000;
    set_mtime_nanos(&selected_file, expected_mtime);

    save_rust_plan_local(&plan, &cache).unwrap();
    let bundle_dir = rust_plan_bundle_dir(&cache, &rust_plan_cache_key(&plan));
    assert!(bundle_dir.join(BUNDLE_MANIFEST_NAME).exists());
    assert!(!bundle_dir.join(LEGACY_BUNDLE_MANIFEST_NAME).exists());
    let manifest = load_manifest(&bundle_dir);
    let artifact = manifest
        .artifacts
        .iter()
        .find(|artifact| artifact.relative_path == "debug/deps/libserde-abc.rlib")
        .unwrap();
    assert_eq!(artifact.mtime_unix_nanos, expected_mtime);

    std::fs::remove_dir_all(plan.target_dir.as_path()).unwrap();
    restore_rust_plan_local(&plan, &cache).unwrap();
    assert_eq!(file_mtime_nanos(&selected_file), expected_mtime);
}

#[test]
fn thin_plan_skips_final_binary_outputs() {
    let dir = tempfile::tempdir().unwrap();
    synthetic_target_with_final_binary(dir.path());
    let plan = sample_plan(dir.path(), RustPlanMode::Thin);
    let cache = dir.path().join("cache");

    let saved = save_rust_plan_local(&plan, &cache).unwrap();
    assert_eq!(saved.saved_file_count, 6);
    assert_eq!(
        saved
            .skipped_reasons
            .get("artifact_class_disallowed_by_plan"),
        Some(&1)
    );
    assert!(saved.skipped_samples.iter().any(|sample| {
        sample.path.ends_with("debug/app.exe") || sample.path.ends_with("debug/app")
    }));

    std::fs::remove_dir_all(plan.target_dir.as_path()).unwrap();
    let restored = restore_rust_plan_local(&plan, &cache).unwrap();
    assert_eq!(restored.restored_file_count, 6);
    assert!(!plan.target_dir.join("debug/app.exe").exists());
    assert!(!plan.target_dir.join("debug/app").exists());
}

#[test]
fn restore_skips_mismatched_bundles_without_mutating_target_dir() {
    let dir = tempfile::tempdir().unwrap();
    synthetic_target(dir.path());
    let plan = sample_plan(dir.path(), RustPlanMode::Thin);
    let cache = dir.path().join("cache");

    save_rust_plan_local(&plan, &cache).unwrap();
    let bundle_dir = rust_plan_bundle_dir(&cache, &rust_plan_cache_key(&plan));
    let mut manifest = load_manifest(&bundle_dir);
    manifest.cache_key = "rust-plan-v1-deadbeefdeadbeefdeadbeefdeadbeef".to_string();
    manifest.mode = RustPlanMode::Full;
    manifest.plan_identity_hash =
        "deadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef".to_string();
    write_manifest(&bundle_dir, &manifest);

    std::fs::remove_dir_all(plan.target_dir.as_path()).unwrap();
    std::fs::create_dir_all(plan.target_dir.as_path()).unwrap();

    let restored = restore_rust_plan_local(&plan, &cache).unwrap();
    assert_eq!(restored.restored_file_count, 0);
    assert_eq!(restored.compatibility.status, "warning");
    assert_eq!(restored.key_input_mismatches.len(), 3);
    assert_eq!(
        restored
            .miss_classifications
            .get("toolchain_profile_rustflags_target_mismatch"),
        Some(&2)
    );
    assert_eq!(
        restored
            .miss_classifications
            .get("lockfile_config_manifest_hash_mismatch"),
        Some(&2)
    );
    assert!(std::fs::read_dir(plan.target_dir.as_path())
        .unwrap()
        .next()
        .is_none());
}

#[test]
fn full_save_restore_includes_workspace_outputs_but_prunes_incremental() {
    let dir = tempfile::tempdir().unwrap();
    synthetic_target(dir.path());
    let plan = sample_plan(dir.path(), RustPlanMode::Full);
    let cache = dir.path().join("cache");

    let saved = save_rust_plan_local(&plan, &cache).unwrap();
    assert_eq!(saved.skipped_reasons.get("transient_state"), Some(&1));
    assert!(saved.saved_file_count > 6);

    std::fs::remove_dir_all(plan.target_dir.as_path()).unwrap();
    let restored = restore_rust_plan_local(&plan, &cache).unwrap();
    assert!(restored.restored_file_count > 6);
    assert!(plan.target_dir.join("debug/deps/libapp-abc.rlib").exists());
    assert!(!plan.target_dir.join("debug/incremental/state.bin").exists());
}
