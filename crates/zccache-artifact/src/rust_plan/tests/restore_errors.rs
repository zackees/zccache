//! Restore-side error handling: corrupt/missing payloads, manifest entries
//! that try to escape the target dir, and the underlying `safe_join`
//! primitive that guards every restore write.

use super::super::*;
use super::{load_manifest, sample_plan, synthetic_target, write_manifest};
use std::path::Path;
use std::sync::{Mutex, MutexGuard, OnceLock};

const FULL_VERIFY_ENV: &str = "ZCCACHE_RUST_PLAN_RESTORE_VERIFY_BLAKE3";

#[allow(clippy::permissions_set_readonly_false)]
fn make_writable_for_test(path: &Path) {
    let mut permissions = std::fs::metadata(path).unwrap().permissions();
    permissions.set_readonly(false);
    std::fs::set_permissions(path, permissions).unwrap();
}

static ENV_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

struct FullVerifyEnvGuard {
    _lock: MutexGuard<'static, ()>,
    previous: Option<std::ffi::OsString>,
}

impl FullVerifyEnvGuard {
    fn enable() -> Self {
        let lock = ENV_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let previous = std::env::var_os(FULL_VERIFY_ENV);
        std::env::set_var(FULL_VERIFY_ENV, "1");
        Self {
            _lock: lock,
            previous,
        }
    }
}

impl Drop for FullVerifyEnvGuard {
    fn drop(&mut self) {
        match &self.previous {
            Some(value) => std::env::set_var(FULL_VERIFY_ENV, value),
            None => std::env::remove_var(FULL_VERIFY_ENV),
        }
    }
}

#[test]
fn restore_skips_missing_wrong_size_and_wrong_hash_payloads() {
    let _verify = FullVerifyEnvGuard::enable();
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
fn restore_overlays_missing_files_without_overwriting_existing_conflicts() {
    let dir = tempfile::tempdir().unwrap();
    synthetic_target(dir.path());
    let plan = sample_plan(dir.path(), RustPlanMode::Thin);
    let cache = dir.path().join("cache");

    save_rust_plan_local(&plan, &cache).unwrap();
    let existing = plan.target_dir.join("debug/deps/libserde-abc.rlib");
    std::fs::write(&existing, b"local-conflict").unwrap();
    let missing = plan.target_dir.join("debug/deps/libserde-abc.rmeta");
    let missing_original = std::fs::read(&missing).unwrap();
    std::fs::remove_file(&missing).unwrap();

    let restored = restore_rust_plan_local(&plan, &cache).unwrap();

    assert_eq!(std::fs::read(&existing).unwrap(), b"local-conflict");
    assert_eq!(std::fs::read(&missing).unwrap(), missing_original);
    assert_eq!(
        restored.skipped_reasons.get("destination_conflict"),
        Some(&1)
    );
    assert_eq!(restored.restored_file_count, 1);
}

#[test]
fn restore_rejects_corrupted_payload_when_full_verification_is_enabled() {
    let _verify = FullVerifyEnvGuard::enable();
    let dir = tempfile::tempdir().unwrap();
    synthetic_target(dir.path());
    let plan = sample_plan(dir.path(), RustPlanMode::Thin);
    let cache = dir.path().join("cache");

    save_rust_plan_local(&plan, &cache).unwrap();
    let bundle_dir = rust_plan_bundle_dir(&cache, &rust_plan_cache_key(&plan));
    let payload = bundle_dir
        .join(BUNDLE_FILES_DIR)
        .join("debug/deps/libserde-abc.rlib");
    make_writable_for_test(&payload);
    let mut bytes = std::fs::read(&payload).unwrap();
    bytes[0] ^= 0xff;
    std::fs::write(&payload, bytes).unwrap();

    std::fs::remove_dir_all(plan.target_dir.as_path()).unwrap();
    let restored = restore_rust_plan_local(&plan, &cache).unwrap();

    assert_eq!(restored.restored_file_count, 5);
    assert_eq!(
        restored
            .skipped_reasons
            .get("restored_payload_missing_or_corrupt"),
        Some(&1)
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
