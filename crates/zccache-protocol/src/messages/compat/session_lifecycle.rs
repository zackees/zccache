//! `SessionStart` / `SessionStarted` / `SessionEnded` / `SessionStats[Result]`
//! bincode roundtrip tests.

use super::*;

#[test]
fn session_start_with_track_stats_roundtrip() {
    let req = Request::SessionStart {
        client_pid: 1234,
        working_dir: "/home/user/project".into(),
        log_file: None,
        track_stats: true,
        journal_path: None,
        profile: false,
        private_daemon: None,
    };
    roundtrip(&req);

    let req_no_stats = Request::SessionStart {
        client_pid: 1234,
        working_dir: "/home/user/project".into(),
        log_file: None,
        track_stats: false,
        journal_path: None,
        profile: false,
        private_daemon: None,
    };
    roundtrip(&req_no_stats);
}

#[test]
fn session_start_with_journal_path_roundtrip() {
    let req = Request::SessionStart {
        client_pid: 5678,
        working_dir: "/home/user/project".into(),
        log_file: None,
        track_stats: false,
        journal_path: Some("/tmp/build.jsonl".into()),
        profile: false,
        private_daemon: None,
    };
    roundtrip(&req);

    let req_no_journal = Request::SessionStart {
        client_pid: 5678,
        working_dir: "/home/user/project".into(),
        log_file: None,
        track_stats: false,
        journal_path: None,
        profile: false,
        private_daemon: None,
    };
    roundtrip(&req_no_journal);
}

#[test]
fn session_start_with_private_daemon_options_roundtrip() {
    let req = Request::SessionStart {
        client_pid: 5678,
        working_dir: "/home/user/project".into(),
        log_file: None,
        track_stats: true,
        journal_path: Some("/tmp/build.jsonl".into()),
        profile: true,
        private_daemon: Some(PrivateDaemonSessionOptions {
            daemon_name: Some("soldr-dev".to_string()),
            endpoint: Some("test://soldr-dev".to_string()),
            cache_dir: Some("/tmp/zccache-soldr-dev".into()),
            owner_pids: vec![111, 222],
            env: vec![("ZCCACHE_PATH_REMAP".to_string(), "auto".to_string())],
        }),
    };
    roundtrip(&req);
}

#[test]
fn session_started_with_journal_path_roundtrip() {
    let resp = Response::SessionStarted {
        session_id: "550e8400-e29b-41d4-a716-446655440000".into(),
        journal_path: Some("/home/user/.zccache/logs/sessions/test.jsonl".into()),
    };
    roundtrip(&resp);

    let resp_no_journal = Response::SessionStarted {
        session_id: "550e8400-e29b-41d4-a716-446655440000".into(),
        journal_path: None,
    };
    roundtrip(&resp_no_journal);
}

#[test]
fn session_ended_with_stats_roundtrip() {
    let stats = SessionStats {
        duration_ms: 34000,
        compilations: 32,
        hits: 28,
        misses: 3,
        non_cacheable: 1,
        errors: 0,
        errors_cached: 0,
        time_saved_ms: 8200,
        unique_sources: 30,
        bytes_read: 2_000_000,
        bytes_written: 500_000,
        lookup_outcomes: LookupOutcomes::default(),
        phase_profile: None,
    };
    let resp = Response::SessionEnded { stats: Some(stats) };
    roundtrip(&resp);

    let resp_no_stats = Response::SessionEnded { stats: None };
    roundtrip(&resp_no_stats);
}

#[test]
fn session_stats_request_roundtrip() {
    roundtrip(&Request::SessionStats {
        session_id: "550e8400-e29b-41d4-a716-446655440000".into(),
    });
}

#[test]
fn session_stats_result_roundtrip() {
    let stats = SessionStats {
        duration_ms: 5000,
        compilations: 10,
        hits: 7,
        misses: 2,
        non_cacheable: 1,
        errors: 0,
        errors_cached: 0,
        time_saved_ms: 3000,
        unique_sources: 9,
        bytes_read: 50_000,
        bytes_written: 20_000,
        lookup_outcomes: LookupOutcomes::default(),
        phase_profile: None,
    };
    roundtrip(&Response::SessionStatsResult { stats: Some(stats) });
    roundtrip(&Response::SessionStatsResult { stats: None });
}
