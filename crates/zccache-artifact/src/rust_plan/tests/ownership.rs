use super::*;

#[test]
fn cook_partitioned_export_excludes_cook_owned_outputs() {
    let temp = tempfile::tempdir().unwrap();
    synthetic_target(temp.path());
    let mut plan = sample_plan(temp.path(), RustPlanMode::Thin);
    plan.cache_schema_version = 3;
    plan.packages.ownership_policy = Some(THIN_V3_OWNERSHIP_POLICY_ID.to_string());
    plan.packages.ownership_mode = Some(RustPlanOwnershipMode::CookPartitionedV1);
    plan.packages.ownership_complete = true;
    plan.packages.artifact_owners = vec![
        RustPlanArtifactOwner {
            relative_path: "debug/deps/libserde-abc.rlib".to_string(),
            package_id: "registry+serde@1".to_string(),
            owner: RustPlanArtifactOwnerKind::Cook,
        },
        RustPlanArtifactOwner {
            relative_path: "debug/deps/libapp-abc.rlib".to_string(),
            package_id: "path+app@0.1".to_string(),
            owner: RustPlanArtifactOwnerKind::Zccache,
        },
    ];

    save_rust_plan_local(&plan, temp.path()).unwrap();
    let manifest = load_manifest(&rust_plan_bundle_dir(
        temp.path(),
        &rust_plan_cache_key(&plan),
    ));
    let app = manifest
        .artifacts
        .iter()
        .find(|artifact| artifact.relative_path == "debug/deps/libapp-abc.rlib")
        .expect("project output should be exported");
    assert_eq!(app.owner, Some(RustPlanArtifactOwnerKind::Zccache));
    assert!(!manifest
        .artifacts
        .iter()
        .any(|artifact| artifact.relative_path == "debug/deps/libserde-abc.rlib"));
}

#[test]
fn cook_partitioned_plan_rejects_incomplete_ownership() {
    let temp = tempfile::tempdir().unwrap();
    let mut plan = sample_plan(temp.path(), RustPlanMode::Thin);
    plan.cache_schema_version = 3;
    plan.packages.ownership_policy = Some(THIN_V3_OWNERSHIP_POLICY_ID.to_string());
    plan.packages.ownership_mode = Some(RustPlanOwnershipMode::CookPartitionedV1);
    assert!(plan
        .validate()
        .unwrap_err()
        .to_string()
        .contains("ownership must be complete"));
}

#[test]
fn zccache_all_mode_exports_without_partition_filter() {
    let temp = tempfile::tempdir().unwrap();
    synthetic_target(temp.path());
    let mut plan = sample_plan(temp.path(), RustPlanMode::Thin);
    plan.cache_schema_version = 3;
    plan.packages.ownership_policy = Some(THIN_V3_OWNERSHIP_POLICY_ID.to_string());
    plan.packages.ownership_mode = Some(RustPlanOwnershipMode::ZccacheAllV1);

    save_rust_plan_local(&plan, temp.path()).unwrap();
    let manifest = load_manifest(&rust_plan_bundle_dir(
        temp.path(),
        &rust_plan_cache_key(&plan),
    ));
    assert!(manifest
        .artifacts
        .iter()
        .any(|artifact| artifact.relative_path == "debug/deps/libserde-abc.rlib"));
    assert!(manifest
        .artifacts
        .iter()
        .any(|artifact| artifact.relative_path == "debug/deps/libapp-abc.rlib"));
}

#[test]
fn materialization_rejects_owner_tampered_bundle() {
    let temp = tempfile::tempdir().unwrap();
    synthetic_target(temp.path());
    let mut plan = sample_plan(temp.path(), RustPlanMode::Thin);
    plan.cache_schema_version = 3;
    plan.packages.ownership_policy = Some(THIN_V3_OWNERSHIP_POLICY_ID.to_string());
    plan.packages.ownership_mode = Some(RustPlanOwnershipMode::ZccacheAllV1);
    save_rust_plan_local(&plan, temp.path()).unwrap();

    let bundle = rust_plan_bundle_dir(temp.path(), &rust_plan_cache_key(&plan));
    let mut manifest = load_manifest(&bundle);
    manifest.artifacts[0].owner = Some(RustPlanArtifactOwnerKind::Cook);
    write_manifest(&bundle, &manifest);
    std::fs::remove_file(temp.path().join("target/debug/deps/libserde-abc.rlib")).unwrap();

    let summary = restore_rust_plan_local(&plan, temp.path()).unwrap();
    assert_eq!(summary.restored_file_count, 0);
    assert!(summary
        .key_input_mismatches
        .iter()
        .any(|reason| reason.contains("non-zccache owner")));
}
