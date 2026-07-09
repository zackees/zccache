use serde_json::json;

use super::super::engine_profile::{
    engine_profile_report_from_value, engine_profile_report_json, render_engine_profile_report,
    EngineProfileError,
};

#[test]
fn engine_profile_json_reports_counts_averages_and_dominant_phases() {
    let report = engine_profile_report_from_value(&profile_fixture()).unwrap();
    let json = engine_profile_report_json(&report);

    assert_eq!(json["status"], "ok");
    assert_eq!(json["hit_path"]["count"], 3);
    assert_eq!(json["hit_path"]["total_ns"], 900);
    assert_eq!(json["hit_path"]["avg_ns"], 300);
    assert_eq!(json["hit_path"]["avg_ms"].as_f64().unwrap(), 0.0003);
    assert_eq!(json["hit_path"]["dominant_phase"], "write_output");
    assert_eq!(json["miss_path"]["count"], 2);
    assert_eq!(json["miss_path"]["total_ns"], 4_000_000);
    assert_eq!(json["miss_path"]["avg_ns"], 2_000_000);
    assert_eq!(json["miss_path"]["avg_ms"].as_f64().unwrap(), 2.0);
    assert_eq!(json["miss_path"]["dominant_phase"], "compiler_exec");
}

#[test]
fn engine_profile_human_report_names_paths_and_dominant_phase() {
    let report = engine_profile_report_from_value(&profile_fixture()).unwrap();
    let text = render_engine_profile_report(&report);

    assert!(text.contains("zccache engine profile"));
    assert!(text.contains("hit path: 3 samples"));
    assert!(text.contains("miss path: 2 samples"));
    assert!(text.contains("dominant write_output"));
    assert!(text.contains("compiler_exec"));
}

#[test]
fn engine_profile_zero_counts_have_zero_averages_and_no_dominant_phase() {
    let value = json!({
        "status": "ok",
        "phase_profile": {
            "hit_count": 0,
            "miss_count": 0,
            "total_hit_ns": 0,
            "total_miss_ns": 0
        }
    });
    let report = engine_profile_report_from_value(&value).unwrap();
    let json = engine_profile_report_json(&report);

    assert_eq!(json["hit_path"]["avg_ns"], 0);
    assert_eq!(json["hit_path"]["avg_ms"].as_f64().unwrap(), 0.0);
    assert!(json["hit_path"]["dominant_phase"].is_null());
    assert_eq!(json["miss_path"]["avg_ns"], 0);
    assert_eq!(json["miss_path"]["avg_ms"].as_f64().unwrap(), 0.0);
    assert!(json["miss_path"]["dominant_phase"].is_null());
}

#[test]
fn engine_profile_missing_phase_profile_is_clear_error() {
    let err = engine_profile_report_from_value(&json!({
        "status": "ok",
        "phase_profile": null
    }))
    .unwrap_err();

    assert!(matches!(err, EngineProfileError::MissingPhaseProfile));
    assert!(err.to_string().contains("missing phase_profile"));
}

fn profile_fixture() -> serde_json::Value {
    json!({
        "status": "ok",
        "session_id": "session-123",
        "phase_profile": {
            "hit_count": 3,
            "miss_count": 2,
            "parse_args_ns": 30,
            "build_context_ns": 60,
            "hash_source_ns": 90,
            "hash_headers_ns": 120,
            "depgraph_check_ns": 150,
            "request_cache_lookup_ns": 180,
            "cross_root_validate_ns": 210,
            "artifact_lookup_ns": 240,
            "write_output_ns": 270,
            "bookkeeping_ns": 15,
            "total_hit_ns": 900,
            "compiler_exec_ns": 3_000_000,
            "include_scan_ns": 500_000,
            "hash_all_ns": 300_000,
            "artifact_store_ns": 200_000,
            "total_miss_ns": 4_000_000
        }
    })
}
