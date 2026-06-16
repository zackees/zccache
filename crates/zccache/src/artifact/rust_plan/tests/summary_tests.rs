//! `RustPlanSummary` behavior: backend identity, manual skip recording,
//! miss-classification derivation from existing diagnostics, and
//! serialization-time recomputation from compile-cache session stats.

use super::super::*;
use super::sample_plan;

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
