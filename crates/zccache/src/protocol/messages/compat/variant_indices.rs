//! Append-only bincode variant index guards for `Request` and `Response`.
//!
//! Bincode encodes enum variants by declaration order; reordering or
//! inserting a variant in the middle silently breaks every older client
//! talking to a newer daemon (and vice versa). These tests pin each
//! variant to its expected `u32` discriminant — any reorder fails them.

use super::*;

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
        (
            18,
            Request::ReleaseWorktreeHandles {
                path: "/tmp/work".into(),
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
        (
            16,
            Response::Backpressure {
                queue_depth: 0,
                retry_after_ms: 0,
                reason: "compile_queue_full".to_string(),
            },
        ),
        (
            17,
            Response::ReleaseWorktreeHandlesResult {
                inspected: 0,
                released: 0,
                sessions_dropped: vec![],
                unreleased: vec![],
            },
        ),
    ];

    for (expected, response) in response_cases {
        assert_eq!(variant_index(response), *expected, "{response:?}");
    }
}
