#![cfg(test)]

use super::*;
use std::path::Path;

fn sample_plan(root: &Path, mode: RustPlanMode) -> RustArtifactPlanV1 {
    RustArtifactPlanV1 {
        schema_version: 1,
        mode,
        workspace_root: root.into(),
        target_dir: root.join("target").into(),
        toolchain: RustToolchainIdentity {
            rustc: "rustc 1.94.1".to_string(),
            cargo: "cargo 1.94.1".to_string(),
            channel: "1.94.1".to_string(),
            host: "x86_64-pc-windows-msvc".to_string(),
        },
        target_triple: "x86_64-pc-windows-msvc".to_string(),
        profile: "debug".to_string(),
        inputs: RustPlanInputs {
            features_hash: "features".to_string(),
            rustflags_hash: "rustflags".to_string(),
            env_hash: "env".to_string(),
            lockfile_hash: "lock".to_string(),
            cargo_config_hash: "config".to_string(),
            manifest_hashes: vec!["manifest".to_string()],
        },
        packages: RustPlanPackages {
            selected_package_ids: vec!["app 0.1.0".to_string()],
            workspace_package_ids: vec!["app 0.1.0".to_string()],
            excluded_path_package_ids: vec!["local_dep 0.1.0".to_string()],
        },
        allowed_artifact_classes: vec![
            RustArtifactClass::Rlib,
            RustArtifactClass::Rmeta,
            RustArtifactClass::DepInfo,
            RustArtifactClass::CargoFingerprint,
            RustArtifactClass::BuildScriptMetadata,
            RustArtifactClass::BuildScriptOutput,
        ],
        cache_schema_version: 1,
        journal_log_path: Some(root.join("zccache-session.jsonl").into()),
        cache_profile: None,
        dropped_artifact_classes: Vec::new(),
    }
}

fn write(path: &Path, bytes: &[u8]) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, bytes).unwrap();
}

fn set_mtime_nanos(path: &Path, nanos: u64) {
    let time = unix_nanos_to_system_time(nanos);
    let file_times = std::fs::FileTimes::new()
        .set_accessed(time)
        .set_modified(time);
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .unwrap();
    file.set_times(file_times).unwrap();
}

fn file_mtime_nanos(path: &Path) -> u64 {
    system_time_to_unix_nanos(std::fs::metadata(path).unwrap().modified().unwrap())
}

fn load_manifest(bundle_dir: &Path) -> RustArtifactBundleManifest {
    read_bundle_manifest(bundle_dir).unwrap()
}

fn write_manifest(bundle_dir: &Path, manifest: &RustArtifactBundleManifest) {
    write_bundle_manifest(bundle_dir, manifest).unwrap();
}

fn synthetic_target(root: &Path) {
    let target = root.join("target").join("debug");
    write(
        &target.join("deps").join("libserde-abc.rlib"),
        b"serde rlib",
    );
    write(
        &target.join("deps").join("libserde-abc.rmeta"),
        b"serde rmeta",
    );
    write(&target.join("deps").join("serde-abc.d"), b"serde depinfo");
    write(
        &target.join("deps").join("libapp-abc.rlib"),
        b"workspace rlib",
    );
    write(
        &target.join("deps").join("liblocal_dep-abc.rlib"),
        b"path dep rlib",
    );
    write(
        &target
            .join(".fingerprint")
            .join("serde-abc")
            .join("dep-lib-serde"),
        b"fingerprint",
    );
    write(
        &target
            .join("build")
            .join("serde-abc")
            .join("invoked.timestamp"),
        b"timestamp",
    );
    write(
        &target
            .join("build")
            .join("serde-abc")
            .join("out")
            .join("gen.rs"),
        b"generated",
    );
    write(&target.join("incremental").join("state.bin"), b"transient");
}

fn synthetic_target_with_final_binary(root: &Path) {
    synthetic_target(root);
    let target = root.join("target").join("debug");
    #[cfg(windows)]
    write(&target.join("app.exe"), b"final binary");
    #[cfg(not(windows))]
    write(&target.join("app"), b"final binary");
}

fn synthetic_target_with_proc_macro_outputs(root: &Path) {
    synthetic_target(root);
    let target = root.join("target").join("debug");
    #[cfg(windows)]
    let proc_macro = target.join("deps").join("libproc_macro2-def456.dll");
    #[cfg(not(windows))]
    let proc_macro = target.join("deps").join("libproc_macro2-def456.so");
    write(&proc_macro, b"proc-macro dylib");

    #[cfg(windows)]
    let shared_lib = target.join("deps").join("libserde_shared-def456.dll");
    #[cfg(not(windows))]
    let shared_lib = target.join("deps").join("libserde_shared-def456.so");
    write(&shared_lib, b"shared lib");
}

fn synthetic_target_with_package_exclusions(root: &Path) {
    synthetic_target(root);
    let target = root.join("target").join("debug");
    write(
        &target.join("deps").join("libapp-abc.rmeta"),
        b"workspace rmeta",
    );
    write(&target.join("deps").join("app-abc.d"), b"workspace depinfo");
    write(
        &target.join("deps").join("liblocal_dep-abc.rmeta"),
        b"path dep rmeta",
    );
    write(
        &target.join("deps").join("local_dep-abc.d"),
        b"path dep depinfo",
    );
    write(
        &target
            .join(".fingerprint")
            .join("app-abc")
            .join("dep-lib-app"),
        b"workspace fingerprint",
    );
    write(
        &target
            .join(".fingerprint")
            .join("local_dep-abc")
            .join("dep-lib-local_dep"),
        b"path dep fingerprint",
    );
    write(
        &target
            .join("build")
            .join("app-abc")
            .join("invoked.timestamp"),
        b"workspace timestamp",
    );
    write(
        &target
            .join("build")
            .join("local_dep-abc")
            .join("invoked.timestamp"),
        b"path dep timestamp",
    );
    write(
        &target
            .join("build")
            .join("app-abc")
            .join("out")
            .join("gen.rs"),
        b"workspace generated",
    );
    write(
        &target
            .join("build")
            .join("local_dep-abc")
            .join("out")
            .join("gen.rs"),
        b"path dep generated",
    );
}

#[test]
fn rejects_unsupported_schema_before_deserializing_unknown_fields() {
    let raw = serde_json::json!({
        "schema_version": 99,
        "cache_schema_version": 1,
        "unexpected_future_field": true
    });
    let err = RustArtifactPlanV1::from_json_value(raw).unwrap_err();
    assert!(matches!(
        err,
        RustPlanError::UnsupportedSchemaVersion {
            found: 99,
            supported: 1
        }
    ));
}

#[test]
fn rejects_unsupported_cache_schema_before_filesystem_mutation() {
    let dir = tempfile::tempdir().unwrap();
    synthetic_target(dir.path());
    let plan = RustArtifactPlanV1 {
        cache_schema_version: 99,
        ..sample_plan(dir.path(), RustPlanMode::Thin)
    };
    let cache = dir.path().join("cache");

    let err = save_rust_plan_local(&plan, &cache).unwrap_err();
    // soldr#461: zccache now accepts both v1 (legacy) and v2 (thin-v2)
    // wire schemas; the error reports the most-recent supported version.
    assert!(matches!(
        err,
        RustPlanError::UnsupportedCacheSchemaVersion {
            found: 99,
            supported: 2
        }
    ));
    assert!(!cache.exists());
}

#[test]
fn restore_rejects_unsupported_cache_schema_before_filesystem_mutation() {
    let dir = tempfile::tempdir().unwrap();
    synthetic_target(dir.path());
    let plan = RustArtifactPlanV1 {
        cache_schema_version: 99,
        ..sample_plan(dir.path(), RustPlanMode::Thin)
    };
    let cache = dir.path().join("cache");
    std::fs::create_dir_all(&cache).unwrap();
    std::fs::create_dir_all(plan.target_dir.as_path()).unwrap();
    let sentinel = plan.target_dir.join("sentinel.txt");
    std::fs::write(&sentinel, b"keep me").unwrap();

    let err = restore_rust_plan_local(&plan, &cache).unwrap_err();
    assert!(matches!(
        err,
        RustPlanError::UnsupportedCacheSchemaVersion {
            found: 99,
            supported: 2
        }
    ));
    assert!(sentinel.exists());
}

#[test]
fn omitted_or_empty_allowed_classes_default_to_thin_classes() {
    let dir = tempfile::tempdir().unwrap();
    let mut plan_value = serde_json::to_value(sample_plan(dir.path(), RustPlanMode::Thin)).unwrap();

    plan_value
        .as_object_mut()
        .unwrap()
        .remove("allowed_artifact_classes");
    let omitted = RustArtifactPlanV1::from_json_value(plan_value.clone()).unwrap();
    assert_eq!(omitted.effective_allowed_classes(), default_thin_classes());

    plan_value.as_object_mut().unwrap().insert(
        "allowed_artifact_classes".to_string(),
        serde_json::json!([]),
    );
    let empty = RustArtifactPlanV1::from_json_value(plan_value).unwrap();
    assert_eq!(empty.effective_allowed_classes(), default_thin_classes());
}

#[test]
fn rust_plan_load_accepts_protobuf_plan() {
    let dir = tempfile::tempdir().unwrap();
    let mut plan = sample_plan(dir.path(), RustPlanMode::Thin);
    plan.cache_schema_version = 2;
    plan.cache_profile = Some("thin-v2".to_string());
    plan.dropped_artifact_classes = vec![
        RustArtifactClass::CargoFingerprintOutputs,
        RustArtifactClass::BuildScriptBuild,
        RustArtifactClass::Dwo,
    ];
    let plan_path = dir.path().join("plan.pb");

    std::fs::write(&plan_path, plan.to_proto_bytes().unwrap()).unwrap();
    let loaded = RustArtifactPlanV1::load(&plan_path).unwrap();

    assert_eq!(loaded, plan);
}

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
fn summary_records_backend_identity_and_manual_skips() {
    let dir = tempfile::tempdir().unwrap();
    let plan = sample_plan(dir.path(), RustPlanMode::Thin);
    let mut summary = RustPlanSummary::validation_success(&plan, &dir.path().join("cache"));
    assert_eq!(summary.backend, "local");
    assert!(summary.backend_cache_key.is_none());

    summary.set_backend(
        "gha",
        Some("rust-plan-v1-key".to_string()),
        Some("version".to_string()),
    );
    summary.record_skip("<gha-cache>", "backend_cache_miss");

    assert_eq!(summary.backend, "gha");
    assert_eq!(
        summary.backend_cache_key.as_deref(),
        Some("rust-plan-v1-key")
    );
    assert_eq!(summary.backend_cache_version.as_deref(), Some("version"));
    assert_eq!(summary.skipped_reasons.get("backend_cache_miss"), Some(&1));
    assert_eq!(
        summary.miss_classifications.get("backend_cache_miss"),
        Some(&1)
    );
}

#[test]
fn summary_derives_miss_classifications_from_existing_diagnostics() {
    let dir = tempfile::tempdir().unwrap();
    let plan = sample_plan(dir.path(), RustPlanMode::Thin);
    let mut summary = RustPlanSummary::validation_success(&plan, &dir.path().join("cache"));

    summary.record_skip("debug/deps/app.exe", "artifact_class_disallowed_by_plan");
    summary.record_skip(
        "debug/deps/libapp-abc.rlib",
        "workspace_or_path_dependency_excluded_by_plan",
    );
    summary
        .key_input_mismatches
        .push("bundle mode does not match requested plan".to_string());
    summary
        .key_input_mismatches
        .push("bundle input hash does not match requested plan".to_string());
    summary.compile_cache_stats = Some(serde_json::json!({
        "compilations": 4,
        "hits": 1,
        "misses": 3,
    }));
    summary.refresh_miss_classifications();

    assert_eq!(
        summary
            .miss_classifications
            .get("artifact_class_disallowed_by_plan"),
        Some(&1)
    );
    assert_eq!(
        summary
            .miss_classifications
            .get("workspace_or_path_dependency_excluded_by_plan"),
        Some(&1)
    );
    assert_eq!(
        summary
            .miss_classifications
            .get("toolchain_profile_rustflags_target_mismatch"),
        Some(&1)
    );
    assert_eq!(
        summary
            .miss_classifications
            .get("lockfile_config_manifest_hash_mismatch"),
        Some(&1)
    );
    assert_eq!(
        summary
            .miss_classifications
            .get("zccache_compile_cache_miss_despite_equivalent_rustc_command"),
        Some(&3)
    );
}

#[test]
fn serialized_summary_recomputes_miss_classifications_from_session_stats() {
    let dir = tempfile::tempdir().unwrap();
    let plan = sample_plan(dir.path(), RustPlanMode::Thin);
    let mut summary = RustPlanSummary::validation_success(&plan, &dir.path().join("cache"));
    summary.record_skip("<gha-cache>", "backend_cache_miss");

    summary.compile_cache_stats = Some(serde_json::json!({
        "status": "ok",
        "cache_misses": 2,
    }));

    let json = serde_json::to_value(&summary).unwrap();
    assert_eq!(
        json["miss_classifications"]["backend_cache_miss"].as_u64(),
        Some(1)
    );
    assert_eq!(
        json["miss_classifications"]["zccache_compile_cache_miss_despite_equivalent_rustc_command"]
            .as_u64(),
        Some(2)
    );
}

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
fn safe_join_rejects_path_traversal() {
    let err = safe_join(Path::new("root"), "../outside").unwrap_err();
    assert!(matches!(err, RustPlanError::UnsafeRelativePath(_)));
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
#[test]
fn from_json_str_accepts_utf8_bom() {
    let dir = tempfile::tempdir().unwrap();
    let plan = sample_plan(dir.path(), RustPlanMode::Thin);
    let json = serde_json::to_string(&plan).unwrap();
    let loaded = RustArtifactPlanV1::from_json_str(&format!("\u{feff}{json}")).unwrap();
    assert_eq!(loaded.schema_version, 1);
}

// â”€â”€â”€ rust-plan tar thread resolver (issue #177) â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€

#[test]
fn tar_threads_parser_accepts_grammar_from_soldr_273() {
    // unset / auto / empty / whitespace â†’ default (vCPU-bounded, capped at 8)
    let default = default_rust_plan_tar_threads();
    assert!((1..=DEFAULT_RUST_PLAN_TAR_THREADS_CAP).contains(&default));
    assert_eq!(parse_rust_plan_tar_threads(None), default);
    assert_eq!(parse_rust_plan_tar_threads(Some("auto")), default);
    assert_eq!(parse_rust_plan_tar_threads(Some("AUTO")), default);
    assert_eq!(parse_rust_plan_tar_threads(Some("")), default);
    assert_eq!(parse_rust_plan_tar_threads(Some("   ")), default);

    // 1 â†’ sequential escape hatch
    assert_eq!(parse_rust_plan_tar_threads(Some("1")), 1);

    // Positive integer â†’ clamped to MAX_RUST_PLAN_TAR_THREADS
    assert_eq!(parse_rust_plan_tar_threads(Some("4")), 4);
    assert_eq!(
        parse_rust_plan_tar_threads(Some("9999")),
        MAX_RUST_PLAN_TAR_THREADS
    );

    // Garbage / 0 â†’ default (defensive)
    assert_eq!(parse_rust_plan_tar_threads(Some("0")), default);
    assert_eq!(parse_rust_plan_tar_threads(Some("not-a-number")), default);
    assert_eq!(parse_rust_plan_tar_threads(Some("-1")), default);
}

#[test]
fn parallel_bundling_matches_sequential_byte_for_byte() {
    // `select_artifacts` pre-sorts by relative_path; with rayon's ordered
    // `par_iter().collect()` we must end up with the same artifact list,
    // same hashes, same sizes â€” regardless of thread count.
    fn bundle_with(threads: usize) -> Vec<RustBundledArtifact> {
        let dir = tempfile::tempdir().unwrap();
        synthetic_target(dir.path());
        let plan = sample_plan(dir.path(), RustPlanMode::Thin);

        let mut candidates = Vec::new();
        collect_files(plan.target_dir.as_path(), &mut candidates).unwrap();
        candidates.sort();
        let mut summary = RustPlanSummary::new(
            RustPlanOperation::Save,
            plan.mode,
            plan.schema_version,
            plan.cache_schema_version,
            rust_plan_cache_key(&plan),
            None,
            None,
        );
        let selected = select_artifacts(&plan, candidates, &mut summary);

        let files_dir = dir.path().join("out").join(format!("t{threads}"));
        std::fs::create_dir_all(&files_dir).unwrap();
        bundle_selected_artifacts_with_threads(&selected, &files_dir, threads).unwrap()
    }

    let sequential = bundle_with(1);
    let parallel = bundle_with(4);

    assert!(!sequential.is_empty());
    assert_eq!(sequential.len(), parallel.len());
    for (seq, par) in sequential.iter().zip(parallel.iter()) {
        assert_eq!(seq.relative_path, par.relative_path);
        assert_eq!(seq.size, par.size);
        assert_eq!(seq.content_hash, par.content_hash);
        assert_eq!(seq.class, par.class);
    }
}

// â”€â”€â”€ soldr#461: thin-v2 wire-format support â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€

/// Builds the full thin-v2 plan shape soldr emits today: explicit
/// `cache_profile`, the published allow-list, the published drop-list,
/// and `cache_schema_version: 2`. Tests below mutate this baseline.
fn sample_thin_v2_plan(root: &Path) -> RustArtifactPlanV1 {
    RustArtifactPlanV1 {
        allowed_artifact_classes: vec![
            RustArtifactClass::CargoFingerprintMeta,
            RustArtifactClass::DepInfo,
            RustArtifactClass::BuildScriptMetadata,
            RustArtifactClass::BuildScriptOutput,
        ],
        cache_schema_version: 2,
        cache_profile: Some("thin-v2".to_string()),
        dropped_artifact_classes: vec![
            RustArtifactClass::Incremental,
            RustArtifactClass::BuildScriptBuild,
            RustArtifactClass::Rlib,
            RustArtifactClass::Rmeta,
            RustArtifactClass::ProcMacro,
            RustArtifactClass::Dwo,
            RustArtifactClass::Pdb,
            RustArtifactClass::Dsym,
            RustArtifactClass::CargoFingerprintOutputs,
        ],
        ..sample_plan(root, RustPlanMode::Thin)
    }
}

#[test]
fn from_json_value_accepts_thin_v2_cache_profile_and_drop_list() {
    let dir = tempfile::tempdir().unwrap();
    let plan = sample_thin_v2_plan(dir.path());
    let value = serde_json::to_value(&plan).unwrap();
    let loaded = RustArtifactPlanV1::from_json_value(value).unwrap();
    assert_eq!(loaded.cache_profile.as_deref(), Some("thin-v2"));
    assert_eq!(loaded.cache_schema_version, 2);
    assert!(loaded
        .dropped_artifact_classes
        .contains(&RustArtifactClass::Rlib));
    assert!(loaded
        .allowed_artifact_classes
        .contains(&RustArtifactClass::CargoFingerprintMeta));
}

#[test]
fn from_json_value_ignores_unknown_forward_compat_fields() {
    // soldr#461 dropped `#[serde(deny_unknown_fields)]` so future soldr
    // versions can add fields without coordinated zccache releases.
    let dir = tempfile::tempdir().unwrap();
    let plan = sample_plan(dir.path(), RustPlanMode::Thin);
    let mut value = serde_json::to_value(&plan).unwrap();
    value.as_object_mut().unwrap().insert(
        "future_soldr_field_that_does_not_exist_yet".to_string(),
        serde_json::json!({"any": "shape", "version": 9001}),
    );
    let loaded =
        RustArtifactPlanV1::from_json_value(value).expect("unknown fields must be ignored");
    assert_eq!(loaded.schema_version, 1);
}

#[test]
fn legacy_thin_v1_plan_without_new_fields_still_deserializes() {
    // The legacy wire shape (no `cache_profile`, no
    // `dropped_artifact_classes`, `cache_schema_version: 1`) must remain
    // round-trippable so older soldr keeps working unchanged.
    let dir = tempfile::tempdir().unwrap();
    let plan = sample_plan(dir.path(), RustPlanMode::Thin);
    let mut value = serde_json::to_value(&plan).unwrap();
    let obj = value.as_object_mut().unwrap();
    obj.remove("cache_profile");
    obj.remove("dropped_artifact_classes");
    let loaded = RustArtifactPlanV1::from_json_value(value).unwrap();
    assert!(loaded.cache_profile.is_none());
    assert!(loaded.dropped_artifact_classes.is_empty());
    assert_eq!(loaded.cache_schema_version, 1);
}

#[test]
fn from_json_value_accepts_cache_schema_version_2() {
    let dir = tempfile::tempdir().unwrap();
    let mut plan = sample_plan(dir.path(), RustPlanMode::Thin);
    plan.cache_schema_version = 2;
    let value = serde_json::to_value(&plan).unwrap();
    let loaded = RustArtifactPlanV1::from_json_value(value).unwrap();
    assert_eq!(loaded.cache_schema_version, 2);
}

#[test]
fn thin_v2_save_drops_rlib_rmeta_even_when_allowed_list_lists_them() {
    // Confirms the load-bearing property from soldr#461: a file whose
    // class appears in `dropped_artifact_classes` is skipped during
    // save even if the same class is also in `allowed_artifact_classes`.
    let dir = tempfile::tempdir().unwrap();
    synthetic_target(dir.path());
    let mut plan = sample_thin_v2_plan(dir.path());
    // Force the conflict explicitly: keep the drop list, but ALSO put
    // rlib/rmeta into the allow list so the drop semantics has to win.
    plan.allowed_artifact_classes = vec![
        RustArtifactClass::Rlib,
        RustArtifactClass::Rmeta,
        RustArtifactClass::DepInfo,
        RustArtifactClass::CargoFingerprintMeta,
        RustArtifactClass::BuildScriptMetadata,
        RustArtifactClass::BuildScriptOutput,
    ];
    let cache = dir.path().join("cache");

    let saved = save_rust_plan_local(&plan, &cache).unwrap();

    // No `.rlib` / `.rmeta` survives the walk even though they are
    // allowed â€” drop list wins.
    let bundle_dir = rust_plan_bundle_dir(&cache, &rust_plan_cache_key(&plan));
    let manifest = load_manifest(&bundle_dir);
    for artifact in &manifest.artifacts {
        assert!(
            !artifact.relative_path.ends_with(".rlib"),
            "thin-v2 drop list must skip .rlib; got {}",
            artifact.relative_path
        );
        assert!(
            !artifact.relative_path.ends_with(".rmeta"),
            "thin-v2 drop list must skip .rmeta; got {}",
            artifact.relative_path
        );
    }
    // The drops route through the existing summary skip reason so the
    // CI consumer doesn't need a new bucket.
    assert!(saved
        .skipped_reasons
        .get("artifact_class_disallowed_by_plan")
        .is_some_and(|n| *n >= 3));
}

#[test]
fn thin_v2_save_keeps_fingerprint_meta_but_drops_fingerprint_outputs() {
    // Tests the split: files cargo reads to make a freshness decision
    // (`dep-*`, `lib-*`, `bin-*`, `output-*`, `invoked.timestamp`) are
    // kept; everything else in `.fingerprint/<crate>/` is dropped.
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("target").join("debug");
    // Meta files (kept):
    write(
        &target.join(".fingerprint/serde-abc/invoked.timestamp"),
        b"ts",
    );
    write(&target.join(".fingerprint/serde-abc/dep-lib-serde"), b"dep");
    write(
        &target.join(".fingerprint/serde-abc/lib-serde"),
        b"libstamp",
    );
    // Output file (dropped):
    write(
        &target.join(".fingerprint/serde-abc/serde-abc.json"),
        b"output",
    );
    // Need at least one classifiable, non-fingerprint file so the
    // walker has something else to think about (and to verify it
    // doesn't get accidentally swept into the drop bucket).
    write(&target.join("deps/serde-abc.d"), b"depinfo");
    // Drop a transient incremental file so we exercise that path too.
    write(&target.join("incremental/state.bin"), b"transient");

    let plan = sample_thin_v2_plan(dir.path());
    let cache = dir.path().join("cache");

    save_rust_plan_local(&plan, &cache).unwrap();

    let bundle_dir = rust_plan_bundle_dir(&cache, &rust_plan_cache_key(&plan));
    let manifest = load_manifest(&bundle_dir);
    let kept_paths: Vec<&str> = manifest
        .artifacts
        .iter()
        .map(|a| a.relative_path.as_str())
        .collect();

    assert!(
        kept_paths
            .iter()
            .any(|p| p.ends_with(".fingerprint/serde-abc/invoked.timestamp")),
        "invoked.timestamp must be kept; got {kept_paths:?}",
    );
    assert!(
        kept_paths
            .iter()
            .any(|p| p.ends_with(".fingerprint/serde-abc/dep-lib-serde")),
        "dep-* must be kept; got {kept_paths:?}",
    );
    assert!(
        kept_paths
            .iter()
            .any(|p| p.ends_with(".fingerprint/serde-abc/lib-serde")),
        "lib-* must be kept; got {kept_paths:?}",
    );
    assert!(
        !kept_paths
            .iter()
            .any(|p| p.ends_with(".fingerprint/serde-abc/serde-abc.json")),
        "fingerprint output .json must be dropped; got {kept_paths:?}",
    );
    assert!(
        kept_paths.iter().any(|p| p.ends_with("deps/serde-abc.d")),
        "dep_info must still be kept; got {kept_paths:?}",
    );
}

#[test]
fn thin_v2_classifier_recognizes_new_classes() {
    // Smoke tests for the classifier branches the wire-format drop
    // list relies on. These run with `thin_v2 = true` to exercise the
    // `.fingerprint/` split; the other categories ignore the flag.
    let bsb = if cfg!(windows) {
        Path::new("debug/build/serde-abc/build-script-build.exe")
    } else {
        Path::new("debug/build/serde-abc/build-script-build")
    };
    assert_eq!(
        classify_artifact(bsb, RustPlanMode::Thin, true),
        Some(RustArtifactClass::BuildScriptBuild),
    );

    assert_eq!(
        classify_artifact(
            Path::new("debug/deps/libserde-abc.dwo"),
            RustPlanMode::Thin,
            true,
        ),
        Some(RustArtifactClass::Dwo),
    );
    assert_eq!(
        classify_artifact(
            Path::new("debug/deps/libserde-abc.pdb"),
            RustPlanMode::Thin,
            true,
        ),
        Some(RustArtifactClass::Pdb),
    );
    assert_eq!(
        classify_artifact(
            Path::new("debug/deps/app.dSYM/Contents/Info.plist"),
            RustPlanMode::Thin,
            true,
        ),
        Some(RustArtifactClass::Dsym),
    );

    // thin-v2 fingerprint split:
    assert_eq!(
        classify_artifact(
            Path::new("debug/.fingerprint/serde-abc/invoked.timestamp"),
            RustPlanMode::Thin,
            true,
        ),
        Some(RustArtifactClass::CargoFingerprintMeta),
    );
    assert_eq!(
        classify_artifact(
            Path::new("debug/.fingerprint/serde-abc/serde-abc.json"),
            RustPlanMode::Thin,
            true,
        ),
        Some(RustArtifactClass::CargoFingerprintOutputs),
    );
    // Legacy (thin_v2 = false) keeps the umbrella class so older
    // callers see no behavior change.
    assert_eq!(
        classify_artifact(
            Path::new("debug/.fingerprint/serde-abc/invoked.timestamp"),
            RustPlanMode::Thin,
            false,
        ),
        Some(RustArtifactClass::CargoFingerprint),
    );
}

#[test]
fn thin_v2_and_thin_v1_identity_hashes_differ() {
    // Two plans with identical inputs but different `cache_profile`
    // values must produce distinct cache keys, otherwise a thin-v1
    // bundle could be served to a thin-v2 plan request (or vice
    // versa) â€” and the file sets are different, so restore would
    // produce a wrong target/ tree.
    let dir = tempfile::tempdir().unwrap();
    let v1 = sample_plan(dir.path(), RustPlanMode::Thin);
    let v2 = sample_thin_v2_plan(dir.path());
    assert_ne!(
        rust_plan_identity_hash(&v1),
        rust_plan_identity_hash(&v2),
        "thin-v1 and thin-v2 identity hashes must not collide",
    );
}
