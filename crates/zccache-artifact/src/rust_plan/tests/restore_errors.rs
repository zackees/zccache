//! Restore-side error handling: corrupt/missing payloads, manifest entries
//! that try to escape the target dir, and the underlying `safe_join`
//! primitive that guards every restore write.

use super::super::*;
use super::{load_manifest, sample_plan, synthetic_target, write_manifest};
use std::path::Path;

#[test]
fn restore_skips_missing_wrong_size_and_wrong_hash_payloads() {
    let dir = tempfile::tempdir().unwrap();
    synthetic_target(dir.path());
    let plan = sample_plan(dir.path(), RustPlanMode::Thin);
    let cache = dir.path().join("cache");

    let saved = save_rust_plan_local(&plan, &cache).unwrap();
    assert_eq!(saved.saved_file_count, 6);

    let bundle_dir = rust_plan_bundle_dir(&cache, &rust_plan_cache_key(&plan));
    let mut manifest = load_manifest(&bundle_dir);

    std::fs::remove_file(
        bundle_dir
            .join(BUNDLE_FILES_DIR)
            .join("debug/deps/libserde-abc.rlib"),
    )
    .unwrap();
    manifest.artifacts[1].size += 1;
    manifest.artifacts[2].content_hash = "not-the-right-hash".to_string();
    write_manifest(&bundle_dir, &manifest);

    std::fs::remove_dir_all(plan.target_dir.as_path()).unwrap();
    let restored = restore_rust_plan_local(&plan, &cache).unwrap();

    assert_eq!(restored.restored_file_count, 3);
    assert_eq!(
        restored
            .skipped_reasons
            .get("restored_payload_missing_or_corrupt"),
        Some(&3)
    );
    assert_eq!(
        restored
            .miss_classifications
            .get("restored_payload_missing_or_corrupt"),
        Some(&3)
    );
    assert!(!plan
        .target_dir
        .join("debug/deps/libserde-abc.rlib")
        .exists());
}

#[test]
fn restore_skips_manifest_path_traversal_entries() {
    let dir = tempfile::tempdir().unwrap();
    synthetic_target(dir.path());
    let plan = sample_plan(dir.path(), RustPlanMode::Thin);
    let cache = dir.path().join("cache");

    save_rust_plan_local(&plan, &cache).unwrap();

    let bundle_dir = rust_plan_bundle_dir(&cache, &rust_plan_cache_key(&plan));
    let mut manifest = load_manifest(&bundle_dir);
    manifest.artifacts[0].relative_path = "../escape.txt".to_string();
    write_manifest(&bundle_dir, &manifest);

    std::fs::remove_dir_all(plan.target_dir.as_path()).unwrap();
    let restored = restore_rust_plan_local(&plan, &cache).unwrap();

    assert_eq!(restored.restored_file_count, 5);
    assert_eq!(restored.skipped_count, 1);
    assert_eq!(restored.skipped_reasons.get("path_traversal"), Some(&1));
    assert!(!dir.path().join("escape.txt").exists());
}

#[test]
fn restore_missing_bundle_is_a_diagnostic_cache_miss() {
    let dir = tempfile::tempdir().unwrap();
    let plan = sample_plan(dir.path(), RustPlanMode::Thin);
    let summary = restore_rust_plan_local(&plan, &dir.path().join("cache")).unwrap();
    assert_eq!(summary.backend, "local");
    assert_eq!(summary.restored_file_count, 0);
    assert_eq!(
        summary
            .skipped_reasons
            .get("artifact_absent_from_restored_plan"),
        Some(&1)
    );
    assert_eq!(
        summary
            .miss_classifications
            .get("artifact_absent_from_restored_plan"),
        Some(&1)
    );
}

#[test]
fn safe_join_rejects_path_traversal() {
    let err = safe_join(Path::new("root"), "../outside").unwrap_err();
    assert!(matches!(err, RustPlanError::UnsafeRelativePath(_)));
}
