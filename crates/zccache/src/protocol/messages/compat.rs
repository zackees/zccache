//! Bincode compatibility and protocol roundtrip tests.

use super::*;
use crate::core::NormalizedPath;
use serde::Serialize;
use std::sync::Arc;

/// Helper: roundtrip a value through bincode.
fn roundtrip<T: Serialize + serde::de::DeserializeOwned + PartialEq + std::fmt::Debug>(val: &T) {
    let bytes = bincode::serialize(val).unwrap();
    let decoded: T = bincode::deserialize(&bytes).unwrap();
    assert_eq!(*val, decoded);
}

fn variant_index<T: Serialize>(val: &T) -> u32 {
    let bytes = bincode::serialize(val).unwrap();
    u32::from_le_bytes(bytes[0..4].try_into().unwrap())
}

fn sample_session_stats() -> SessionStats {
    SessionStats {
        duration_ms: 1,
        compilations: 2,
        hits: 3,
        misses: 4,
        non_cacheable: 5,
        errors: 6,
        errors_cached: 7,
        time_saved_ms: 8,
        unique_sources: 9,
        bytes_read: 10,
        bytes_written: 11,
        phase_profile: None,
    }
}

fn sample_daemon_status() -> DaemonStatus {
    DaemonStatus {
        version: "1.0.0".to_string(),
        daemon_namespace: "default".to_string(),
        endpoint: "test-endpoint".to_string(),
        private_daemon: PrivateDaemonStatus::shared(),
        artifact_count: 1,
        cache_size_bytes: 2,
        metadata_entries: 3,
        uptime_secs: 4,
        cache_hits: 5,
        cache_misses: 6,
        total_compilations: 7,
        non_cacheable: 8,
        compile_errors: 9,
        compile_errors_cached: 10,
        time_saved_ms: 11,
        total_links: 12,
        link_hits: 13,
        link_misses: 14,
        link_non_cacheable: 15,
        dep_graph_contexts: 16,
        dep_graph_files: 17,
        sessions_total: 18,
        sessions_active: 19,
        cache_dir: "/tmp/zccache".into(),
        dep_graph_version: 20,
        dep_graph_disk_size: 21,
        dep_graph_persisted: true,
    }
}

fn sample_artifact() -> ArtifactData {
    ArtifactData {
        outputs: vec![ArtifactOutput {
            name: "out.o".to_string(),
            payload: ArtifactPayload::Bytes(Arc::new(b"object".to_vec())),
        }],
        stdout: Arc::new(Vec::new()),
        stderr: Arc::new(Vec::new()),
        exit_code: 0,
    }
}

#[test]
fn request_variant_indices_are_append_only() {
    let request_cases: &[(u32, Request)] = &[
        (0, Request::Ping),
        (1, Request::Shutdown),
        (2, Request::Status),
        (
            3,
            Request::Lookup {
                cache_key: "k".to_string(),
            },
        ),
        (
            4,
            Request::Store {
                cache_key: "k".to_string(),
                artifact: sample_artifact(),
            },
        ),
        (
            5,
            Request::SessionStart {
                client_pid: 1,
                working_dir: "/tmp/work".into(),
                log_file: None,
                track_stats: true,
                journal_path: None,
                profile: false,
                private_daemon: None,
            },
        ),
        (
            6,
            Request::Compile {
                session_id: "session".to_string(),
                args: vec!["-c".to_string(), "a.c".to_string()],
                cwd: "/tmp/work".into(),
                compiler: "/usr/bin/cc".into(),
                env: Some(vec![("A".to_string(), "B".to_string())]),
                stdin: Vec::new(),
            },
        ),
        (
            7,
            Request::SessionEnd {
                session_id: "session".to_string(),
            },
        ),
        (8, Request::Clear),
        (
            9,
            Request::CompileEphemeral {
                client_pid: 1,
                working_dir: "/tmp/work".into(),
                compiler: "/usr/bin/cc".into(),
                args: vec!["-c".to_string(), "a.c".to_string()],
                cwd: "/tmp/work".into(),
                env: None,
                stdin: Vec::new(),
            },
        ),
        (
            10,
            Request::LinkEphemeral {
                client_pid: 1,
                tool: "/usr/bin/ar".into(),
                args: vec!["rcs".to_string(), "liba.a".to_string()],
                cwd: "/tmp/work".into(),
                env: None,
            },
        ),
        (
            11,
            Request::SessionStats {
                session_id: "session".to_string(),
            },
        ),
        (
            12,
            Request::FingerprintCheck {
                cache_file: "/tmp/cache.json".into(),
                cache_type: "hash".to_string(),
                root: "/tmp/work".into(),
                extensions: vec!["rs".to_string()],
                include_globs: vec![],
                exclude: vec![],
            },
        ),
        (
            13,
            Request::FingerprintMarkSuccess {
                cache_file: "/tmp/cache.json".into(),
            },
        ),
        (
            14,
            Request::FingerprintMarkFailure {
                cache_file: "/tmp/cache.json".into(),
            },
        ),
        (
            15,
            Request::FingerprintInvalidate {
                cache_file: "/tmp/cache.json".into(),
            },
        ),
        (16, Request::ListRustArtifacts),
        (
            17,
            Request::GenericToolExec {
                tool: "/usr/bin/tool".into(),
                args: vec!["--flag".to_string()],
                cwd: "/tmp/work".into(),
                env: vec![("A".to_string(), "B".to_string())],
                input_files: vec!["/tmp/work/input.txt".into()],
                input_extra: Arc::new(b"extra".to_vec()),
                output_streams: ExecOutputStreams::default(),
                output_files: vec!["/tmp/work/out.txt".into()],
                tool_hash: Some([1; 32]),
                cache_policy: ExecCachePolicy::Normal,
                cwd_in_key: true,
                include_scan_files: vec![],
                include_dirs: vec![],
                system_include_dirs: vec![],
                iquote_dirs: vec![],
                depfile: None,
                non_deterministic: false,
                key_args_filter: vec![],
            },
        ),
    ];

    for (expected, request) in request_cases {
        assert_eq!(variant_index(request), *expected, "{request:?}");
    }
}

#[test]
fn response_variant_indices_are_append_only() {
    let response_cases: &[(u32, Response)] = &[
        (0, Response::Pong),
        (1, Response::ShuttingDown),
        (2, Response::Status(sample_daemon_status())),
        (3, Response::LookupResult(LookupResult::Miss)),
        (4, Response::StoreResult(StoreResult::Stored)),
        (
            5,
            Response::SessionStarted {
                session_id: "session".to_string(),
                journal_path: None,
            },
        ),
        (
            6,
            Response::CompileResult {
                exit_code: 0,
                stdout: Arc::new(Vec::new()),
                stderr: Arc::new(Vec::new()),
                cached: false,
            },
        ),
        (
            7,
            Response::SessionEnded {
                stats: Some(sample_session_stats()),
            },
        ),
        (
            8,
            Response::LinkResult {
                exit_code: 0,
                stdout: Arc::new(Vec::new()),
                stderr: Arc::new(Vec::new()),
                cached: true,
                warning: None,
            },
        ),
        (
            9,
            Response::Error {
                message: "error".to_string(),
            },
        ),
        (
            10,
            Response::Cleared {
                artifacts_removed: 1,
                metadata_cleared: 2,
                dep_graph_contexts_cleared: 3,
                on_disk_bytes_freed: 4,
            },
        ),
        (
            11,
            Response::SessionStatsResult {
                stats: Some(sample_session_stats()),
            },
        ),
        (
            12,
            Response::FingerprintCheckResult {
                decision: "run".to_string(),
                reason: Some("changed".to_string()),
                changed_files: vec!["a.rs".to_string()],
            },
        ),
        (13, Response::FingerprintAck),
        (
            14,
            Response::RustArtifactList {
                artifacts: vec![RustArtifactInfo {
                    cache_key: "k".to_string(),
                    output_names: vec!["liba.rlib".to_string()],
                    payload_count: 1,
                }],
            },
        ),
        (
            15,
            Response::GenericToolExecResult {
                exit_code: 0,
                stdout: Arc::new(Vec::new()),
                stderr: Arc::new(Vec::new()),
                output_files: vec![],
                cached: false,
                cache_key_hex: "abc".to_string(),
            },
        ),
    ];

    for (expected, response) in response_cases {
        assert_eq!(variant_index(response), *expected, "{response:?}");
    }
}

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
        }),
    };
    roundtrip(&stats);

    // serde-json round-trip — written to last-session-stats.json and
    // read by both `zccache analyze` and the perf harness's
    // `perf_local.py render_summary`.
    let json = serde_json::to_string(&stats).expect("serialize");
    let decoded: SessionStats = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(stats, decoded);

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
    assert_eq!(decoded.errors_cached, 0);
}

#[test]
fn daemon_status_expanded_roundtrip() {
    let status = DaemonStatus {
        version: env!("CARGO_PKG_VERSION").to_string(),
        daemon_namespace: "soldr-dev".to_string(),
        endpoint: "test://soldr-dev".to_string(),
        private_daemon: PrivateDaemonStatus {
            enabled: true,
            owners: vec![PrivateDaemonOwnerStatus {
                pid: 1234,
                ref_count: 2,
            }],
            private_env_keys: vec!["ZCCACHE_PATH_REMAP".to_string()],
        },
        artifact_count: 892,
        cache_size_bytes: 147_000_000,
        metadata_entries: 5430,
        uptime_secs: 8040,
        cache_hits: 1089,
        cache_misses: 143,
        total_compilations: 1247,
        non_cacheable: 15,
        compile_errors: 3,
        compile_errors_cached: 2,
        time_saved_ms: 750_000,
        total_links: 50,
        link_hits: 38,
        link_misses: 10,
        link_non_cacheable: 2,
        dep_graph_contexts: 892,
        dep_graph_files: 4201,
        sessions_total: 41,
        sessions_active: 3,
        cache_dir: "/home/user/.zccache".into(),
        dep_graph_version: 1,
        dep_graph_disk_size: 2_500_000,
        dep_graph_persisted: true,
    };
    roundtrip(&status);
}

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
        phase_profile: None,
    };
    let resp = Response::SessionEnded { stats: Some(stats) };
    roundtrip(&resp);

    let resp_no_stats = Response::SessionEnded { stats: None };
    roundtrip(&resp_no_stats);
}

#[test]
fn clear_request_roundtrip() {
    roundtrip(&Request::Clear);
}

#[test]
fn cleared_response_roundtrip() {
    roundtrip(&Response::Cleared {
        artifacts_removed: 42,
        metadata_cleared: 100,
        dep_graph_contexts_cleared: 25,
        on_disk_bytes_freed: 1024 * 1024,
    });
}

#[test]
fn compile_ephemeral_roundtrip() {
    roundtrip(&Request::CompileEphemeral {
        client_pid: 9876,
        working_dir: "/home/user/project".into(),
        compiler: "/usr/bin/clang++".into(),
        args: vec!["-c".into(), "main.cpp".into(), "-o".into(), "main.o".into()],
        cwd: "/home/user/project/build".into(),
        env: Some(vec![("PATH".into(), "/usr/bin".into())]),
        stdin: Vec::new(),
    });
    // Non-empty stdin payload must round-trip byte-for-byte — including
    // embedded NULs and binary bytes — so `rustc -` style invocations
    // through the wrapper see the same input the parent sent us.
    roundtrip(&Request::CompileEphemeral {
        client_pid: 1,
        working_dir: ".".into(),
        compiler: "gcc".into(),
        args: vec![],
        cwd: ".".into(),
        env: None,
        stdin: b"hello\x00world\nbinary\xff\xfe".to_vec(),
    });
}

#[test]
fn link_ephemeral_roundtrip() {
    roundtrip(&Request::LinkEphemeral {
        client_pid: 5555,
        tool: "/usr/bin/ar".into(),
        args: vec!["rcs".into(), "libfoo.a".into(), "a.o".into(), "b.o".into()],
        cwd: "/home/user/project/build".into(),
        env: Some(vec![("PATH".into(), "/usr/bin".into())]),
    });
    roundtrip(&Request::LinkEphemeral {
        client_pid: 1,
        tool: "lib.exe".into(),
        args: vec!["/OUT:foo.lib".into(), "a.obj".into()],
        cwd: ".".into(),
        env: None,
    });
}

#[test]
fn link_result_roundtrip() {
    roundtrip(&Response::LinkResult {
        exit_code: 0,
        stdout: Arc::new(vec![]),
        stderr: Arc::new(vec![]),
        cached: true,
        warning: None,
    });
    roundtrip(&Response::LinkResult {
        exit_code: 0,
        stdout: Arc::new(vec![]),
        stderr: Arc::new(b"some warning".to_vec()),
        cached: false,
        warning: Some("non-deterministic: missing D flag".into()),
    });
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
        phase_profile: None,
    };
    roundtrip(&Response::SessionStatsResult { stats: Some(stats) });
    roundtrip(&Response::SessionStatsResult { stats: None });
}

#[test]
fn existing_request_variants_still_work() {
    roundtrip(&Request::Ping);
    roundtrip(&Request::Shutdown);
    roundtrip(&Request::Status);
    roundtrip(&Request::SessionEnd {
        session_id: "550e8400-e29b-41d4-a716-446655440000".into(),
    });
    roundtrip(&Request::Compile {
        session_id: "550e8400-e29b-41d4-a716-446655440000".into(),
        args: vec!["-c".into(), "foo.c".into()],
        cwd: "/tmp".into(),
        compiler: "/usr/bin/gcc".into(),
        env: None,
        stdin: Vec::new(),
    });
}

#[test]
fn existing_response_variants_still_work() {
    roundtrip(&Response::Pong);
    roundtrip(&Response::ShuttingDown);
    roundtrip(&Response::CompileResult {
        exit_code: 0,
        stdout: Arc::new(vec![]),
        stderr: Arc::new(vec![]),
        cached: true,
    });
    roundtrip(&Response::Error {
        message: "test".into(),
    });
}

#[test]
fn daemon_status_version_field_roundtrips() {
    let with_version = DaemonStatus {
        version: "1.2.3".to_string(),
        daemon_namespace: crate::core::config::DEFAULT_DAEMON_NAMESPACE.to_string(),
        endpoint: String::new(),
        private_daemon: PrivateDaemonStatus::shared(),
        artifact_count: 0,
        cache_size_bytes: 0,
        metadata_entries: 0,
        uptime_secs: 0,
        cache_hits: 0,
        cache_misses: 0,
        total_compilations: 0,
        non_cacheable: 0,
        compile_errors: 0,
        compile_errors_cached: 0,
        time_saved_ms: 0,
        total_links: 0,
        link_hits: 0,
        link_misses: 0,
        link_non_cacheable: 0,
        dep_graph_contexts: 0,
        dep_graph_files: 0,
        sessions_total: 0,
        sessions_active: 0,
        cache_dir: "".into(),
        dep_graph_version: 0,
        dep_graph_disk_size: 0,
        dep_graph_persisted: false,
    };
    roundtrip(&with_version);
}

// Compile-time check: PROTOCOL_VERSION must be positive.
const _: () = assert!(super::super::PROTOCOL_VERSION > 0);
// Compile-time check: PROTOCOL_VERSION == 14 after private daemon
// SessionStart/status diagnostics were added. v13 was the pin after daemon
// namespace diagnostics were added to DaemonStatus. v12 was the pin after
// cached-error counters were added for rustc negative-result caching.
// v11 was the pin after
// `GenericToolExec` gained Path A (include scan) + Path B (depfile) +
// non_deterministic + key_args_filter, fully implementing issue #272.
// v10 was the prior pin when `GenericToolExec` was added. v9 was the pin
// after SessionStats gained `phase_profile`. v8 was the pin after
// Compile/CompileEphemeral gained `stdin` and ArtifactPayload replaced
// ArtifactOutput.data: Arc<Vec<u8>> (issue #296 Option B).
const _FINGERPRINT_VERSION: () = assert!(super::super::PROTOCOL_VERSION == 14);

#[test]
fn fingerprint_check_roundtrip() {
    roundtrip(&Request::FingerprintCheck {
        cache_file: "/tmp/lint.json".into(),
        cache_type: "two-layer".into(),
        root: "/home/user/project/src".into(),
        extensions: vec!["rs".into(), "toml".into()],
        include_globs: vec![],
        exclude: vec![".git".into(), "target".into()],
    });
    roundtrip(&Request::FingerprintCheck {
        cache_file: "cache.json".into(),
        cache_type: "hash".into(),
        root: ".".into(),
        extensions: vec![],
        include_globs: vec!["**/*.cpp".into(), "**/*.h".into()],
        exclude: vec![],
    });
}

#[test]
fn fingerprint_mark_success_roundtrip() {
    roundtrip(&Request::FingerprintMarkSuccess {
        cache_file: "/tmp/lint.json".into(),
    });
}

#[test]
fn fingerprint_mark_failure_roundtrip() {
    roundtrip(&Request::FingerprintMarkFailure {
        cache_file: "/tmp/lint.json".into(),
    });
}

#[test]
fn fingerprint_invalidate_roundtrip() {
    roundtrip(&Request::FingerprintInvalidate {
        cache_file: "/tmp/lint.json".into(),
    });
}

#[test]
fn fingerprint_check_result_roundtrip() {
    roundtrip(&Response::FingerprintCheckResult {
        decision: "skip".into(),
        reason: None,
        changed_files: vec![],
    });
    roundtrip(&Response::FingerprintCheckResult {
        decision: "run".into(),
        reason: Some("content changed".into()),
        changed_files: vec!["src/main.rs".into(), "src/lib.rs".into()],
    });
    roundtrip(&Response::FingerprintCheckResult {
        decision: "run".into(),
        reason: Some("no cache file".into()),
        changed_files: vec![],
    });
}

#[test]
fn fingerprint_ack_roundtrip() {
    roundtrip(&Response::FingerprintAck);
}

#[test]
fn list_rust_artifacts_request_roundtrip() {
    roundtrip(&Request::ListRustArtifacts);
}

#[test]
fn rust_artifact_list_response_roundtrip() {
    roundtrip(&Response::RustArtifactList {
        artifacts: vec![
            RustArtifactInfo {
                cache_key: "abc123def456".into(),
                output_names: vec![
                    "libfoo-abc123.rlib".into(),
                    "libfoo-abc123.rmeta".into(),
                    "foo-abc123.d".into(),
                ],
                payload_count: 3,
            },
            RustArtifactInfo {
                cache_key: "deadbeef".into(),
                output_names: vec!["libbar-deadbeef.rlib".into()],
                payload_count: 1,
            },
        ],
    });
    // Empty list
    roundtrip(&Response::RustArtifactList { artifacts: vec![] });
}

#[test]
fn generic_tool_exec_roundtrip() {
    let req = Request::GenericToolExec {
        tool: "/usr/local/bin/fastled-lint".into(),
        args: vec!["src/foo.cpp".into(), "--json".into()],
        cwd: "/home/user/project".into(),
        env: vec![
            ("PATH".into(), "/usr/bin".into()),
            ("LINT_VERSION".into(), "1.2.3".into()),
        ],
        input_files: vec!["src/foo.cpp".into(), "ci/lint_cpp_rs/rules.json".into()],
        input_extra: Arc::new(b"namespace-tag".to_vec()),
        output_streams: ExecOutputStreams::default(),
        output_files: vec!["report.json".into()],
        tool_hash: Some([0x42; 32]),
        cache_policy: ExecCachePolicy::Normal,
        cwd_in_key: true,
        include_scan_files: vec!["src/foo.cpp".into()],
        include_dirs: vec!["src".into(), "include".into()],
        system_include_dirs: vec!["/usr/include".into()],
        iquote_dirs: vec!["thirdparty/q".into()],
        depfile: Some("target/lint/foo.d".into()),
        non_deterministic: false,
        key_args_filter: vec!["^--verbose$".into(), "^--no-color$".into()],
    };
    roundtrip(&req);

    // Bypass + None tool_hash + empty inputs path.
    let req_bypass = Request::GenericToolExec {
        tool: "/bin/true".into(),
        args: vec![],
        cwd: ".".into(),
        env: vec![],
        input_files: vec![],
        input_extra: Arc::new(Vec::new()),
        output_streams: ExecOutputStreams {
            stdout: true,
            stderr: false,
        },
        output_files: vec![],
        tool_hash: None,
        cache_policy: ExecCachePolicy::Bypass,
        cwd_in_key: false,
        include_scan_files: vec![],
        include_dirs: vec![],
        system_include_dirs: vec![],
        iquote_dirs: vec![],
        depfile: None,
        non_deterministic: true,
        key_args_filter: vec![],
    };
    roundtrip(&req_bypass);

    let resp = Response::GenericToolExecResult {
        exit_code: 0,
        stdout: Arc::new(b"linted ok\n".to_vec()),
        stderr: Arc::new(Vec::new()),
        output_files: vec![ArtifactOutput {
            name: "report.json".into(),
            payload: ArtifactPayload::Bytes(Arc::new(b"{}".to_vec())),
        }],
        cached: true,
        cache_key_hex: "deadbeef".repeat(8),
    };
    roundtrip(&resp);
}

#[test]
fn exec_output_streams_default_captures_both() {
    let s = ExecOutputStreams::default();
    assert!(s.stdout);
    assert!(s.stderr);
}

#[test]
fn exec_cache_policy_default_is_normal() {
    assert_eq!(ExecCachePolicy::default(), ExecCachePolicy::Normal);
}

#[test]
fn rust_artifact_info_roundtrip() {
    roundtrip(&RustArtifactInfo {
        cache_key: "0123456789abcdef".into(),
        output_names: vec!["test.o".into()],
        payload_count: 1,
    });
}

#[test]
fn artifact_clone_shares_payload_via_arc() {
    let bytes = Arc::new(vec![1u8, 2, 3, 4]);
    let artifact = ArtifactData {
        outputs: vec![ArtifactOutput {
            name: "test.o".into(),
            payload: ArtifactPayload::Bytes(Arc::clone(&bytes)),
        }],
        stdout: Arc::new(vec![5, 6]),
        stderr: Arc::new(vec![7, 8]),
        exit_code: 0,
    };

    let cloned = artifact.clone();

    // Arc::clone bumps refcount — both point to the same allocation.
    let orig_inner = artifact.outputs[0].payload.as_bytes().unwrap();
    let cloned_inner = cloned.outputs[0].payload.as_bytes().unwrap();
    assert!(Arc::ptr_eq(orig_inner, cloned_inner));
    assert!(Arc::ptr_eq(orig_inner, &bytes));
    assert!(Arc::ptr_eq(&artifact.stdout, &cloned.stdout));
    assert!(Arc::ptr_eq(&artifact.stderr, &cloned.stderr));
}

#[test]
fn artifact_payload_size_bytes_for_bytes_variant() {
    let p = ArtifactPayload::Bytes(Arc::new(vec![0u8; 1234]));
    assert_eq!(p.size_bytes(), 1234);
}

#[test]
fn artifact_payload_size_bytes_for_path_variant() {
    let tmp = tempfile::NamedTempFile::new().expect("tempfile");
    std::fs::write(tmp.path(), vec![0u8; 4321]).expect("write");
    let p = ArtifactPayload::Path(NormalizedPath::from(tmp.path()));
    assert_eq!(p.size_bytes(), 4321);
}

#[test]
fn artifact_payload_size_bytes_for_missing_path_is_zero() {
    let p = ArtifactPayload::Path(NormalizedPath::from(std::path::Path::new(
        "/this/path/does/not/exist/zccache",
    )));
    assert_eq!(p.size_bytes(), 0);
}

#[test]
fn artifact_payload_round_trips_through_bincode() {
    let bytes_variant = ArtifactPayload::Bytes(Arc::new(b"hello".to_vec()));
    let encoded = bincode::serialize(&bytes_variant).expect("serialize bytes");
    let decoded: ArtifactPayload = bincode::deserialize(&encoded).expect("deserialize bytes");
    assert_eq!(decoded, bytes_variant);

    let path_variant = ArtifactPayload::Path(NormalizedPath::from(std::path::Path::new(
        "/tmp/some/place.rlib",
    )));
    let encoded = bincode::serialize(&path_variant).expect("serialize path");
    let decoded: ArtifactPayload = bincode::deserialize(&encoded).expect("deserialize path");
    assert_eq!(decoded, path_variant);
}

#[test]
fn arc_vec_u8_roundtrip_matches_plain_vec() {
    // Prove Arc<Vec<u8>> serializes identically to Vec<u8>.
    let plain: Vec<u8> = vec![0xDE, 0xAD, 0xBE, 0xEF];
    let arc_wrapped: Arc<Vec<u8>> = Arc::new(plain.clone());

    let plain_bytes = bincode::serialize(&plain).unwrap();
    let arc_bytes = bincode::serialize(&arc_wrapped).unwrap();
    assert_eq!(
        plain_bytes, arc_bytes,
        "Arc<Vec<u8>> must serialize identically to Vec<u8>"
    );

    // Deserialize Arc bytes back as plain Vec and vice versa.
    let decoded_plain: Vec<u8> = bincode::deserialize(&arc_bytes).unwrap();
    let decoded_arc: Arc<Vec<u8>> = bincode::deserialize(&plain_bytes).unwrap();
    assert_eq!(decoded_plain, plain);
    assert_eq!(*decoded_arc, plain);
}
