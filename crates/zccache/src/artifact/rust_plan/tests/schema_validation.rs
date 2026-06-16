//! Schema and cache-schema version acceptance/rejection, JSON loading,
//! BOM tolerance, and protobuf plan round-trip.

use super::super::*;
use super::{sample_plan, synthetic_target};

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
fn from_json_str_accepts_utf8_bom() {
    let dir = tempfile::tempdir().unwrap();
    let plan = sample_plan(dir.path(), RustPlanMode::Thin);
    let json = serde_json::to_string(&plan).unwrap();
    let loaded = RustArtifactPlanV1::from_json_str(&format!("\u{feff}{json}")).unwrap();
    assert_eq!(loaded.schema_version, 1);
}
