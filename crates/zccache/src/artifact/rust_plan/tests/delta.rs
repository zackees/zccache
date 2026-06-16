//! Delta save and layered restore: overlay a base bundle with a delta
//! bundle of changed/added files plus tombstones for deleted ones.

use super::super::*;
use super::{
    file_mtime_nanos, load_manifest, sample_plan, set_mtime_nanos, synthetic_target, write,
};

#[test]
fn delta_save_and_layered_restore_overlay_base_with_changes_and_tombstones() {
    let dir = tempfile::tempdir().unwrap();
    synthetic_target(dir.path());
    let plan = sample_plan(dir.path(), RustPlanMode::Thin);
    let base_cache = dir.path().join("base-cache");
    let delta_cache = dir.path().join("delta-cache");

    save_rust_plan_local(&plan, &base_cache).unwrap();

    let unchanged_large = plan.target_dir.join("debug/deps/libserde-abc.rlib");
    let changed = plan.target_dir.join("debug/deps/libserde-abc.rmeta");
    let deleted = plan.target_dir.join("debug/deps/serde-abc.d");
    write(&changed, b"serde rmeta changed");
    let changed_mtime = 1_700_000_100_000_000_000;
    set_mtime_nanos(&changed, changed_mtime);
    std::fs::remove_file(&deleted).unwrap();

    let saved_delta = save_rust_plan_delta_local(&plan, &base_cache, &delta_cache).unwrap();
    assert_eq!(saved_delta.saved_file_count, 1);
    let delta_bundle = rust_plan_bundle_dir(&delta_cache, &rust_plan_cache_key(&plan));
    let delta_manifest = load_manifest(&delta_bundle);
    assert_eq!(
        delta_manifest.layer_kind,
        RustArtifactBundleLayerKind::Delta
    );
    assert_eq!(delta_manifest.artifacts.len(), 1);
    assert_eq!(
        delta_manifest.artifacts[0].relative_path,
        "debug/deps/libserde-abc.rmeta"
    );
    assert_eq!(delta_manifest.artifacts[0].mtime_unix_nanos, changed_mtime);
    assert!(delta_manifest
        .deleted_paths
        .contains(&"debug/deps/serde-abc.d".to_string()));
    assert!(!delta_bundle
        .join(BUNDLE_FILES_DIR)
        .join("debug/deps/libserde-abc.rlib")
        .exists());

    std::fs::remove_dir_all(plan.target_dir.as_path()).unwrap();
    let restored = restore_rust_plan_layered_local(&plan, &base_cache, &delta_cache).unwrap();
    assert_eq!(restored.restored_file_count, 7);
    assert_eq!(std::fs::read(&changed).unwrap(), b"serde rmeta changed");
    assert_eq!(file_mtime_nanos(&changed), changed_mtime);
    assert_eq!(std::fs::read(&unchanged_large).unwrap(), b"serde rlib");
    assert!(!deleted.exists());
}

#[test]
fn delta_save_treats_mtime_only_changes_as_different() {
    let dir = tempfile::tempdir().unwrap();
    synthetic_target(dir.path());
    let plan = sample_plan(dir.path(), RustPlanMode::Thin);
    let base_cache = dir.path().join("base-cache");
    let delta_cache = dir.path().join("delta-cache");
    let changed = plan.target_dir.join("debug/deps/libserde-abc.rlib");

    save_rust_plan_local(&plan, &base_cache).unwrap();
    let changed_mtime = 1_700_000_200_000_000_000;
    set_mtime_nanos(&changed, changed_mtime);

    let saved_delta = save_rust_plan_delta_local(&plan, &base_cache, &delta_cache).unwrap();
    assert_eq!(saved_delta.saved_file_count, 1);
    let delta_bundle = rust_plan_bundle_dir(&delta_cache, &rust_plan_cache_key(&plan));
    let delta_manifest = load_manifest(&delta_bundle);
    assert_eq!(
        delta_manifest.artifacts[0].relative_path,
        "debug/deps/libserde-abc.rlib"
    );
    assert_eq!(delta_manifest.artifacts[0].mtime_unix_nanos, changed_mtime);
}
