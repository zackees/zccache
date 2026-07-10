//! soldr#461: thin-v2 wire-format support. Covers `cache_profile`,
//! `dropped_artifact_classes` (with drop-list-wins-over-allow-list
//! semantics), the `.fingerprint/` meta-vs-output split, classifier
//! coverage for the new classes, and the identity-hash distinction
//! between thin-v1 and thin-v2 plans.

use super::super::*;
use super::{load_manifest, sample_plan, synthetic_target, write};
use std::path::Path;

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
    // allowed -- drop list wins.
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
    write(
        &target.join(".fingerprint/serde-abc/build-script-build-script-build"),
        b"build-script-hash",
    );
    write(
        &target.join(".fingerprint/serde-abc/run-build-script-build-script-build"),
        b"run-build-script-hash",
    );
    write(
        &target.join(".fingerprint/serde-abc/build-script-build-script-build.json"),
        b"diagnostic",
    );
    write(
        &target.join(".fingerprint/serde-abc/run-build-script-build-script-build.json"),
        b"diagnostic",
    );
    write(
        &target.join(".fingerprint/serde-abc/bin-serde.json"),
        b"diagnostic",
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
        kept_paths
            .iter()
            .any(|p| p.ends_with(".fingerprint/serde-abc/build-script-build-script-build")),
        "build-script-* hashes must be kept; got {kept_paths:?}",
    );
    assert!(
        kept_paths
            .iter()
            .any(|p| p.ends_with(".fingerprint/serde-abc/run-build-script-build-script-build")),
        "run-build-script-* hashes must be kept; got {kept_paths:?}",
    );
    assert!(
        !kept_paths
            .iter()
            .any(|p| p.ends_with(".fingerprint/serde-abc/serde-abc.json")),
        "fingerprint output .json must be dropped; got {kept_paths:?}",
    );
    assert!(
        !kept_paths.iter().any(|p| p.ends_with(".json")),
        "all fingerprint diagnostic JSON must be dropped; got {kept_paths:?}",
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
    for name in [
        "build-script-build-script-build",
        "run-build-script-build-script-build",
    ] {
        let path = Path::new("debug/.fingerprint/serde-abc").join(name);
        assert_eq!(
            classify_artifact(&path, RustPlanMode::Thin, true),
            Some(RustArtifactClass::CargoFingerprintMeta),
            "{name} is a load-bearing Cargo freshness hash",
        );
    }
    for name in [
        "build-script-build-script-build.json",
        "run-build-script-build-script-build.json",
        "bin-serde.json",
    ] {
        let path = Path::new("debug/.fingerprint/serde-abc").join(name);
        assert_eq!(
            classify_artifact(&path, RustPlanMode::Thin, true),
            Some(RustArtifactClass::CargoFingerprintOutputs),
            "{name} is diagnostic JSON, not a freshness hash",
        );
    }
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
    // versa) -- and the file sets are different, so restore would
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
