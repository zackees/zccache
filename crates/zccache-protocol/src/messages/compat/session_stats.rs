//! `SessionStats` bincode + serde-json roundtrip tests, including the
//! `PhaseProfileSummary` field added at PROTOCOL_VERSION 9.

use super::*;

#[test]
fn session_stats_roundtrip() {
    let stats = SessionStats {
        duration_ms: 12345,
        compilations: 100,
        hits: 80,
        misses: 15,
        non_cacheable: 5,
        errors: 2,
        errors_cached: 1,
        time_saved_ms: 8000,
        unique_sources: 42,
        bytes_read: 1024 * 1024,
        bytes_written: 512 * 1024,
        lookup_outcomes: LookupOutcomes::default(),
        phase_profile: None,
    };
    roundtrip(&stats);
}

#[test]
fn session_stats_default_zeros() {
    let stats = SessionStats {
        duration_ms: 0,
        compilations: 0,
        hits: 0,
        misses: 0,
        non_cacheable: 0,
        errors: 0,
        errors_cached: 0,
        time_saved_ms: 0,
        unique_sources: 0,
        bytes_read: 0,
        bytes_written: 0,
        lookup_outcomes: LookupOutcomes::default(),
        phase_profile: None,
    };
    roundtrip(&stats);
}

#[test]
fn session_stats_with_phase_profile_roundtrip() {
    // Regression guard for PROTOCOL_VERSION 9 — a populated phase_profile
    // must round-trip both bincode (IPC wire) and serde-json (the form
    // soldr writes to last-session-stats.json).
    let stats = SessionStats {
        duration_ms: 12345,
        compilations: 146,
        hits: 103,
        misses: 12,
        non_cacheable: 31,
        errors: 3,
        errors_cached: 2,
        time_saved_ms: 223,
        unique_sources: 115,
        bytes_read: 143_812_577,
        bytes_written: 62_500_000,
        lookup_outcomes: LookupOutcomes {
            depgraph_hit_artifact_hit: 103,
            depgraph_hit_artifact_miss: 2,
            depgraph_cold_skip: 7,
            depgraph_other_miss: [
                ("headers_changed".to_string(), 3),
                ("source_content_changed".to_string(), 1),
            ]
            .into_iter()
            .collect(),
        },
        phase_profile: Some(PhaseProfileSummary {
            hit_count: 103,
            miss_count: 12,
            parse_args_ns: 4_000_000,
            build_context_ns: 19_000_000,
            hash_source_ns: 6_000_000,
            hash_headers_ns: 11_000_000,
            depgraph_check_ns: 28_000_000,
            request_cache_lookup_ns: 2_500_000,
            cross_root_validate_ns: 1_200_000,
            artifact_lookup_ns: 8_700_000,
            write_output_ns: 540_000_000,
            bookkeeping_ns: 3_300_000,
            total_hit_ns: 623_700_000,
            compiler_exec_ns: 11_400_000_000,
            include_scan_ns: 270_000_000,
            hash_all_ns: 95_000_000,
            artifact_store_ns: 120_000_000,
            total_miss_ns: 11_885_000_000,
            staged: StagedProfileSummary::default(),
        }),
    };
    roundtrip(&stats);

    // serde-json round-trip — written to last-session-stats.json and
    // read by both `zccache analyze` and the perf harness's
    // `perf_local.py render_summary`.
    let json = serde_json::to_string(&stats).expect("serialize");
    let decoded: SessionStats = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(stats, decoded);
    assert_eq!(decoded.lookup_outcomes.depgraph_hit_artifact_hit, 103);
    assert_eq!(decoded.lookup_outcomes.depgraph_hit_artifact_miss, 2);
    assert_eq!(decoded.lookup_outcomes.depgraph_cold_skip, 7);

    let mut pre_v18_json = serde_json::to_value(&stats).expect("serialize value");
    pre_v18_json["phase_profile"]
        .as_object_mut()
        .expect("phase profile object")
        .remove("staged");
    let pre_v18: SessionStats = serde_json::from_value(pre_v18_json).expect("pre-v18 decode");
    assert_eq!(
        pre_v18.phase_profile.unwrap().staged,
        StagedProfileSummary::default()
    );

    // An old-daemon-style JSON that omits phase_profile must decode to
    // None (back-compat with PROTOCOL_VERSION 8 consumers that haven't
    // upgraded the field expectation).
    let legacy = r#"{
        "duration_ms": 0, "compilations": 0, "hits": 0, "misses": 0,
        "non_cacheable": 0, "errors": 0, "time_saved_ms": 0,
        "unique_sources": 0, "bytes_read": 0, "bytes_written": 0
    }"#;
    let decoded: SessionStats = serde_json::from_str(legacy).expect("legacy decode");
    assert!(decoded.phase_profile.is_none());
    assert_eq!(decoded.lookup_outcomes, LookupOutcomes::default());
    assert_eq!(decoded.errors_cached, 0);
}
