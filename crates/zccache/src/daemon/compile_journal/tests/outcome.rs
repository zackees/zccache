//! Tests for `extract_outcome`: `Response` -> `(outcome, exit_code, miss_reason)`.

use std::sync::Arc;

use zccache::protocol::Response;

use super::super::{extract_outcome, miss_reason};

#[test]
fn test_extract_outcome_compile_hit() {
    let resp = Response::CompileResult {
        exit_code: 0,
        stdout: Arc::new(vec![]),
        stderr: Arc::new(vec![]),
        cached: true,
    };
    assert_eq!(extract_outcome(&resp), Some(("hit", 0, None)));
}

#[test]
fn test_extract_outcome_compile_miss() {
    let resp = Response::CompileResult {
        exit_code: 0,
        stdout: Arc::new(vec![]),
        stderr: Arc::new(vec![]),
        cached: false,
    };
    assert_eq!(
        extract_outcome(&resp),
        Some(("miss", 0, Some(miss_reason::UNKNOWN)))
    );
}

#[test]
fn test_extract_outcome_compile_error() {
    let resp = Response::CompileResult {
        exit_code: 1,
        stdout: Arc::new(vec![]),
        stderr: Arc::new(vec![]),
        cached: false,
    };
    assert_eq!(extract_outcome(&resp), Some(("error", 1, None)));
}

#[test]
fn test_extract_outcome_link_hit() {
    let resp = Response::LinkResult {
        exit_code: 0,
        stdout: Arc::new(vec![]),
        stderr: Arc::new(vec![]),
        cached: true,
        warning: None,
    };
    assert_eq!(extract_outcome(&resp), Some(("link_hit", 0, None)));
}

#[test]
fn test_extract_outcome_link_miss() {
    let resp = Response::LinkResult {
        exit_code: 0,
        stdout: Arc::new(vec![]),
        stderr: Arc::new(vec![]),
        cached: false,
        warning: None,
    };
    assert_eq!(
        extract_outcome(&resp),
        Some(("link_miss", 0, Some(miss_reason::UNKNOWN)))
    );
}

#[test]
fn test_extract_outcome_error_response() {
    let resp = Response::Error {
        message: "something broke".to_string(),
    };
    assert_eq!(extract_outcome(&resp), Some(("error", -1, None)));
}

#[test]
fn test_extract_outcome_non_compile() {
    assert_eq!(extract_outcome(&Response::Pong), None);
}

// ─── Adversarial / edge cases ─────────────────────────────────────────────

#[test]
fn test_extract_outcome_compile_cached_nonzero_exit() {
    // exit_code != 0 takes priority over cached flag
    let resp = Response::CompileResult {
        exit_code: 1,
        stdout: Arc::new(vec![]),
        stderr: Arc::new(vec![]),
        cached: true,
    };
    assert_eq!(extract_outcome(&resp), Some(("error", 1, None)));
}

#[test]
fn test_extract_outcome_link_cached_nonzero_exit() {
    let resp = Response::LinkResult {
        exit_code: 2,
        stdout: Arc::new(vec![]),
        stderr: Arc::new(vec![]),
        cached: true,
        warning: None,
    };
    assert_eq!(extract_outcome(&resp), Some(("error", 2, None)));
}

#[test]
fn test_extract_outcome_link_error() {
    let resp = Response::LinkResult {
        exit_code: 1,
        stdout: Arc::new(vec![]),
        stderr: Arc::new(vec![]),
        cached: false,
        warning: None,
    };
    assert_eq!(extract_outcome(&resp), Some(("error", 1, None)));
}

#[test]
fn test_extract_outcome_all_non_journalable() {
    use zccache::core::NormalizedPath;
    use zccache::protocol::{DaemonStatus, LookupResult as LR, SessionStats, StoreResult as SR};

    let non_journalable: Vec<Response> = vec![
        Response::Pong,
        Response::ShuttingDown,
        Response::Status(DaemonStatus {
            version: String::new(),
            artifact_count: 0,
            cache_size_bytes: 0,
            metadata_entries: 0,
            uptime_secs: 0,
            cache_hits: 0,
            cache_misses: 0,
            total_compilations: 0,
            non_cacheable: 0,
            compile_errors: 0,
            time_saved_ms: 0,
            total_links: 0,
            link_hits: 0,
            link_misses: 0,
            link_non_cacheable: 0,
            dep_graph_contexts: 0,
            dep_graph_files: 0,
            sessions_total: 0,
            sessions_active: 0,
            cache_dir: NormalizedPath::from(""),
            dep_graph_version: 0,
            dep_graph_disk_size: 0,
            dep_graph_persisted: false,
        }),
        Response::LookupResult(LR::Miss),
        Response::StoreResult(SR::Stored),
        Response::SessionStarted {
            session_id: "x".into(),
            journal_path: None,
        },
        Response::SessionEnded { stats: None },
        Response::Cleared {
            artifacts_removed: 0,
            metadata_cleared: 0,
            dep_graph_contexts_cleared: 0,
            on_disk_bytes_freed: 0,
        },
        Response::SessionStatsResult {
            stats: Some(SessionStats {
                duration_ms: 0,
                compilations: 0,
                hits: 0,
                misses: 0,
                non_cacheable: 0,
                errors: 0,
                time_saved_ms: 0,
                unique_sources: 0,
                bytes_read: 0,
                bytes_written: 0,
                phase_profile: None,
            }),
        },
    ];
    for (i, resp) in non_journalable.iter().enumerate() {
        assert_eq!(
            extract_outcome(resp),
            None,
            "variant {i} should not be journalable"
        );
    }
}

#[test]
fn test_extract_outcome_negative_exit_codes() {
    let resp_neg1 = Response::CompileResult {
        exit_code: -1,
        stdout: Arc::new(vec![]),
        stderr: Arc::new(vec![]),
        cached: false,
    };
    assert_eq!(extract_outcome(&resp_neg1), Some(("error", -1, None)));

    let resp_min = Response::CompileResult {
        exit_code: i32::MIN,
        stdout: Arc::new(vec![]),
        stderr: Arc::new(vec![]),
        cached: true,
    };
    assert_eq!(extract_outcome(&resp_min), Some(("error", i32::MIN, None)));

    let resp_link_neg = Response::LinkResult {
        exit_code: -1,
        stdout: Arc::new(vec![]),
        stderr: Arc::new(vec![]),
        cached: false,
        warning: None,
    };
    assert_eq!(extract_outcome(&resp_link_neg), Some(("error", -1, None)));
}

#[test]
fn test_extract_outcome_i32_max_exit_code() {
    let resp = Response::CompileResult {
        exit_code: i32::MAX,
        stdout: Arc::new(vec![]),
        stderr: Arc::new(vec![]),
        cached: false,
    };
    assert_eq!(extract_outcome(&resp), Some(("error", i32::MAX, None)));
}

// ─── Issue #322 — miss_reason wiring ──────────────────────────────────────

#[test]
fn extract_outcome_compile_miss_supplies_default_reason() {
    // Acceptance criteria #1: every miss must carry a reason. The
    // canonical translation point (extract_outcome) must default to
    // miss_reason::UNKNOWN so the journal-writer side cannot forget.
    let resp = Response::CompileResult {
        exit_code: 0,
        stdout: Arc::new(vec![]),
        stderr: Arc::new(vec![]),
        cached: false,
    };
    let (outcome, exit_code, miss) = extract_outcome(&resp).unwrap();
    assert_eq!(outcome, "miss");
    assert_eq!(exit_code, 0);
    assert_eq!(miss, Some(miss_reason::UNKNOWN));
}

#[test]
fn extract_outcome_link_miss_supplies_default_reason() {
    let resp = Response::LinkResult {
        exit_code: 0,
        stdout: Arc::new(vec![]),
        stderr: Arc::new(vec![]),
        cached: false,
        warning: None,
    };
    let (outcome, _exit, miss) = extract_outcome(&resp).unwrap();
    assert_eq!(outcome, "link_miss");
    assert_eq!(miss, Some(miss_reason::UNKNOWN));
}

#[test]
fn extract_outcome_hit_has_no_miss_reason() {
    // miss_reason must not be set on hits — keeps legacy hit records lean.
    let resp = Response::CompileResult {
        exit_code: 0,
        stdout: Arc::new(vec![]),
        stderr: Arc::new(vec![]),
        cached: true,
    };
    let (outcome, _, miss) = extract_outcome(&resp).unwrap();
    assert_eq!(outcome, "hit");
    assert_eq!(miss, None);
}

#[test]
fn extract_outcome_error_has_no_miss_reason() {
    // Errors are a distinct outcome category — no miss_reason.
    let resp = Response::CompileResult {
        exit_code: 1,
        stdout: Arc::new(vec![]),
        stderr: Arc::new(vec![]),
        cached: false,
    };
    let (outcome, _, miss) = extract_outcome(&resp).unwrap();
    assert_eq!(outcome, "error");
    assert_eq!(miss, None);
}
