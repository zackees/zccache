#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::panic_in_result_fn,
    clippy::unwrap_in_result
)]

use bytes::BytesMut;
use prost::Message;
use zccache::protocol::wire_prost::{
    decode_prost_message, encode_prost_message, supported_control_request_from_prost,
    supported_control_request_to_prost, supported_control_response_from_prost,
    supported_control_response_to_prost, wire_format_for_protocol_version, zccache_v1 as pb,
    WireFormat,
};
use zccache::protocol::{decode_message, encode_message, PROST_PROTOCOL_VERSION};

#[test]
fn prost_request_frame_roundtrips_with_v16_header() {
    let request = pb::Request {
        body: Some(pb::request::Body::Ping(pb::Empty {})),
        request_id: "req-1".to_string(),
    };

    let encoded = encode_prost_message(&request).unwrap();
    let version = u32::from_le_bytes([encoded[4], encoded[5], encoded[6], encoded[7]]);
    assert_eq!(version, PROST_PROTOCOL_VERSION);

    let mut buf = BytesMut::from(&encoded[..]);
    let decoded = decode_prost_message::<pb::Request>(&mut buf)
        .unwrap()
        .unwrap();
    assert_eq!(decoded.request_id, "req-1");
    assert!(matches!(decoded.body, Some(pb::request::Body::Ping(_))));
    assert!(buf.is_empty());
}

#[test]
fn prost_response_frame_roundtrips_release_worktree_result() {
    let response = pb::Response {
        body: Some(pb::response::Body::ReleaseWorktreeHandlesResult(
            pb::ReleaseWorktreeHandlesResult {
                inspected: 2,
                released: 1,
                sessions_dropped: vec!["session-a".to_string()],
                unreleased: vec![pb::Path {
                    value: "/tmp/worktree/locked.obj".to_string(),
                }],
            },
        )),
        request_id: "req-release".to_string(),
    };

    let encoded = encode_prost_message(&response).unwrap();
    let mut buf = BytesMut::from(&encoded[..]);
    let decoded = decode_prost_message::<pb::Response>(&mut buf)
        .unwrap()
        .unwrap();

    match decoded.body {
        Some(pb::response::Body::ReleaseWorktreeHandlesResult(result)) => {
            assert_eq!(result.inspected, 2);
            assert_eq!(result.released, 1);
            assert_eq!(result.sessions_dropped, ["session-a"]);
            assert_eq!(result.unreleased[0].value, "/tmp/worktree/locked.obj");
        }
        _ => panic!("expected release-worktree response"),
    }
}

#[test]
fn prost_clear_request_and_response_convert_to_protocol_enums() {
    let request = zccache::protocol::Request::Clear;
    let prost_request = supported_control_request_to_prost(&request).unwrap();
    assert_eq!(prost_request.request_id, "control-clear");
    assert!(matches!(
        prost_request.body,
        Some(pb::request::Body::Clear(_))
    ));
    assert_eq!(
        supported_control_request_from_prost(prost_request).unwrap(),
        request
    );

    let response = zccache::protocol::Response::Cleared {
        artifacts_removed: 1,
        metadata_cleared: 2,
        dep_graph_contexts_cleared: 3,
        on_disk_bytes_freed: 4,
    };
    let prost_response = supported_control_response_to_prost(&response, "clear-1").unwrap();
    assert_eq!(prost_response.request_id, "clear-1");
    assert!(matches!(
        prost_response.body,
        Some(pb::response::Body::Cleared(_))
    ));
    assert_eq!(
        supported_control_response_from_prost(prost_response).unwrap(),
        response
    );
}

#[test]
fn prost_release_worktree_handles_request_and_response_convert_to_protocol_enums() {
    let request = zccache::protocol::Request::ReleaseWorktreeHandles {
        path: "/tmp/worktree".into(),
    };
    let prost_request = supported_control_request_to_prost(&request).unwrap();
    assert_eq!(prost_request.request_id, "control-release-worktree-handles");
    assert!(matches!(
        prost_request.body,
        Some(pb::request::Body::ReleaseWorktreeHandles(_))
    ));
    assert_eq!(
        supported_control_request_from_prost(prost_request).unwrap(),
        request
    );

    let response = zccache::protocol::Response::ReleaseWorktreeHandlesResult {
        inspected: 2,
        released: 1,
        sessions_dropped: vec!["session-a".to_string()],
        unreleased: vec!["/tmp/worktree/locked.obj".into()],
    };
    let prost_response = supported_control_response_to_prost(&response, "release-1").unwrap();
    assert_eq!(prost_response.request_id, "release-1");
    assert!(matches!(
        prost_response.body,
        Some(pb::response::Body::ReleaseWorktreeHandlesResult(_))
    ));
    assert_eq!(
        supported_control_response_from_prost(prost_response).unwrap(),
        response
    );
}

#[test]
fn generated_frame_envelope_can_carry_opaque_payload() {
    let request = pb::Request {
        body: Some(pb::request::Body::Status(pb::Empty {})),
        request_id: "req-status".to_string(),
    };
    let payload = request.encode_to_vec();
    let frame = pb::Frame {
        protocol_version: PROST_PROTOCOL_VERSION,
        payload,
        payload_type: "zccache.v1.Request".to_string(),
    };

    let encoded = encode_prost_message(&frame).unwrap();
    let mut buf = BytesMut::from(&encoded[..]);
    let decoded = decode_prost_message::<pb::Frame>(&mut buf)
        .unwrap()
        .unwrap();
    let request = pb::Request::decode(&decoded.payload[..]).unwrap();

    assert_eq!(decoded.protocol_version, PROST_PROTOCOL_VERSION);
    assert_eq!(decoded.payload_type, "zccache.v1.Request");
    assert!(matches!(request.body, Some(pb::request::Body::Status(_))));
}

#[test]
fn bincode_v15_frame_still_roundtrips_on_current_api() {
    let encoded = encode_message(&zccache::protocol::Request::Ping).unwrap();
    let mut buf = BytesMut::from(&encoded[..]);
    let decoded = decode_message::<zccache::protocol::Request>(&mut buf)
        .unwrap()
        .unwrap();

    assert_eq!(decoded, zccache::protocol::Request::Ping);
}

#[test]
fn protocol_version_dispatch_models_v15_and_v16() {
    assert_eq!(
        wire_format_for_protocol_version(WireFormat::BincodeV15.protocol_version().unwrap()),
        Some(WireFormat::BincodeV15)
    );
    assert_eq!(
        wire_format_for_protocol_version(WireFormat::ProstV16.protocol_version().unwrap()),
        Some(WireFormat::ProstV16)
    );
    // The Frame lane has no inner zccache protocol-version header; it is
    // identified by the running-process envelope byte instead.
    assert_eq!(WireFormat::FrameV1.protocol_version(), None);
    assert_eq!(wire_format_for_protocol_version(99), None);
}

// ── Full message-family round-trips (issue rp#383, staged PR 2) ─────────
//
// Every non-control request/response family must survive a
// prost-conversion round trip exactly, so the v16 lane can carry the
// full protocol when `ZCCACHE_DAEMON_WIRE=prost` is selected.

mod full_family {
    use std::sync::Arc;
    use zccache::protocol::wire_prost::{
        default_request_id, request_from_prost, request_to_prost, response_from_prost,
        response_to_prost,
    };
    use zccache::protocol::{
        ArtifactData, ArtifactOutput, ArtifactPayload, ExecCachePolicy, ExecOutputStreams,
        LookupOutcomes, LookupResult, PhaseProfileSummary, PrivateDaemonSessionOptions, Request,
        Response, RustArtifactInfo, SessionStats, StagedProfileSummary, StoreResult,
    };

    fn roundtrip_request(request: Request) {
        let request_id = default_request_id(&request);
        let prost = request_to_prost(&request, request_id);
        assert_eq!(prost.request_id, request_id);
        assert_eq!(request_from_prost(prost).unwrap(), request);
    }

    fn roundtrip_response(response: Response) {
        let prost = response_to_prost(&response, "resp-1");
        assert_eq!(prost.request_id, "resp-1");
        assert_eq!(response_from_prost(prost).unwrap(), response);
    }

    fn sample_artifact() -> ArtifactData {
        ArtifactData {
            outputs: vec![
                ArtifactOutput {
                    name: "foo.o".to_string(),
                    payload: ArtifactPayload::Bytes(Arc::new(vec![1, 2, 3])),
                },
                ArtifactOutput {
                    name: "foo.d".to_string(),
                    payload: ArtifactPayload::Path("/tmp/foo.d".into()),
                },
            ],
            stdout: Arc::new(b"out".to_vec()),
            stderr: Arc::new(b"err".to_vec()),
            exit_code: 0,
        }
    }

    fn sample_stats() -> SessionStats {
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
            lookup_outcomes: LookupOutcomes {
                depgraph_hit_artifact_hit: 12,
                depgraph_hit_artifact_miss: 13,
                depgraph_cold_skip: 14,
                depgraph_other_miss: [("headers_changed".to_string(), 15)].into_iter().collect(),
            },
            phase_profile: Some(PhaseProfileSummary {
                hit_count: 1,
                miss_count: 2,
                parse_args_ns: 3,
                build_context_ns: 4,
                hash_source_ns: 5,
                hash_headers_ns: 6,
                depgraph_check_ns: 7,
                request_cache_lookup_ns: 8,
                cross_root_validate_ns: 9,
                artifact_lookup_ns: 10,
                write_output_ns: 11,
                bookkeeping_ns: 12,
                total_hit_ns: 13,
                compiler_exec_ns: 14,
                include_scan_ns: 15,
                hash_all_ns: 16,
                artifact_store_ns: 17,
                total_miss_ns: 18,
                staged: StagedProfileSummary {
                    counters: [("plan_enabled".to_string(), 2)].into_iter().collect(),
                    timings_ns: [("planning".to_string(), 19)].into_iter().collect(),
                    bytes: [("publication_copied".to_string(), 20)]
                        .into_iter()
                        .collect(),
                    failures: [("unsupported_shape".to_string(), 1)].into_iter().collect(),
                },
            }),
        }
    }

    #[test]
    fn compile_request_roundtrips() {
        roundtrip_request(Request::Compile {
            session_id: "sess-1".to_string(),
            args: vec!["-c".to_string(), "hello.cpp".to_string()],
            cwd: "/src".into(),
            compiler: "/usr/bin/g++".into(),
            env: Some(vec![("PATH".to_string(), "/usr/bin".to_string())]),
            stdin: b"stdin-bytes".to_vec(),
        });
        // env: None must be distinguishable from Some(vec![]).
        roundtrip_request(Request::Compile {
            session_id: "sess-2".to_string(),
            args: vec![],
            cwd: "/src".into(),
            compiler: "/usr/bin/cc".into(),
            env: None,
            stdin: Vec::new(),
        });
        roundtrip_request(Request::Compile {
            session_id: "sess-3".to_string(),
            args: vec![],
            cwd: "/src".into(),
            compiler: "/usr/bin/cc".into(),
            env: Some(Vec::new()),
            stdin: Vec::new(),
        });
    }

    #[test]
    fn compile_ephemeral_request_roundtrips() {
        roundtrip_request(Request::CompileEphemeral {
            client_pid: 42,
            working_dir: "/work".into(),
            compiler: "/usr/bin/clang".into(),
            args: vec!["-c".to_string(), "a.c".to_string()],
            cwd: "/work/sub".into(),
            env: Some(vec![("CC".to_string(), "clang".to_string())]),
            stdin: vec![9, 8, 7],
        });
    }

    #[test]
    fn link_ephemeral_request_roundtrips() {
        roundtrip_request(Request::LinkEphemeral {
            client_pid: 7,
            tool: "/usr/bin/ar".into(),
            args: vec!["rcs".to_string(), "libfoo.a".to_string()],
            cwd: "/work".into(),
            env: None,
        });
    }

    #[test]
    fn session_request_family_roundtrips() {
        roundtrip_request(Request::SessionStart {
            client_pid: 1234,
            working_dir: "/proj".into(),
            log_file: Some("/proj/log.txt".into()),
            track_stats: true,
            journal_path: Some("/proj/journal.jsonl".into()),
            profile: true,
            private_daemon: Some(PrivateDaemonSessionOptions {
                daemon_name: Some("soldr-dev".to_string()),
                endpoint: Some("endpoint-1".to_string()),
                cache_dir: Some("/cache".into()),
                owner_pids: vec![1, 2, 3],
                env: vec![("SECRET".to_string(), "value".to_string())],
            }),
        });
        roundtrip_request(Request::SessionStart {
            client_pid: 1,
            working_dir: "/proj".into(),
            log_file: None,
            track_stats: false,
            journal_path: None,
            profile: false,
            private_daemon: None,
        });
        roundtrip_request(Request::SessionEnd {
            session_id: "sess-1".to_string(),
        });
        roundtrip_request(Request::SessionStats {
            session_id: "sess-1".to_string(),
        });
    }

    #[test]
    fn lookup_and_store_request_roundtrips() {
        roundtrip_request(Request::Lookup {
            cache_key: "abc123".to_string(),
        });
        roundtrip_request(Request::Store {
            cache_key: "abc123".to_string(),
            artifact: sample_artifact(),
        });
    }

    #[test]
    fn fingerprint_request_family_roundtrips() {
        roundtrip_request(Request::FingerprintCheck {
            cache_file: "/proj/.cache/lint.json".into(),
            cache_type: "two-layer".to_string(),
            root: "/proj".into(),
            extensions: vec!["rs".to_string()],
            include_globs: vec!["**/*.rs".to_string()],
            exclude: vec!["target".to_string()],
        });
        roundtrip_request(Request::FingerprintMarkSuccess {
            cache_file: "/proj/.cache/lint.json".into(),
        });
        roundtrip_request(Request::FingerprintMarkFailure {
            cache_file: "/proj/.cache/lint.json".into(),
        });
        roundtrip_request(Request::FingerprintInvalidate {
            cache_file: "/proj/.cache/lint.json".into(),
        });
    }

    #[test]
    fn list_rust_artifacts_request_roundtrips() {
        roundtrip_request(Request::ListRustArtifacts);
    }

    #[test]
    fn generic_tool_exec_request_roundtrips() {
        roundtrip_request(Request::GenericToolExec {
            tool: "/usr/bin/protoc".into(),
            args: vec!["--version".to_string()],
            cwd: "/work".into(),
            env: vec![("LANG".to_string(), "C".to_string())],
            input_files: vec!["/work/a.proto".into()],
            input_extra: Arc::new(vec![1, 2, 3, 4]),
            output_streams: ExecOutputStreams {
                stdout: true,
                stderr: false,
            },
            output_files: vec!["/work/a.pb.rs".into()],
            tool_hash: Some([7u8; 32]),
            cache_policy: ExecCachePolicy::ReadOnly,
            cwd_in_key: true,
            include_scan_files: vec!["/work/a.c".into()],
            include_dirs: vec!["/work/include".into()],
            system_include_dirs: vec!["/usr/include".into()],
            iquote_dirs: vec!["/work/quoted".into()],
            depfile: Some("/work/a.d".into()),
            non_deterministic: false,
            key_args_filter: vec!["--verbose".to_string()],
        });
        // No tool hash, bypass policy, empty collections.
        roundtrip_request(Request::GenericToolExec {
            tool: "/usr/bin/true".into(),
            args: vec![],
            cwd: "/".into(),
            env: vec![],
            input_files: vec![],
            input_extra: Arc::new(Vec::new()),
            output_streams: ExecOutputStreams::default(),
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
        });
    }

    #[test]
    fn generic_tool_exec_rejects_bad_tool_hash_length() {
        let request = Request::GenericToolExec {
            tool: "/usr/bin/true".into(),
            args: vec![],
            cwd: "/".into(),
            env: vec![],
            input_files: vec![],
            input_extra: Arc::new(Vec::new()),
            output_streams: ExecOutputStreams::default(),
            output_files: vec![],
            tool_hash: None,
            cache_policy: ExecCachePolicy::Normal,
            cwd_in_key: false,
            include_scan_files: vec![],
            include_dirs: vec![],
            system_include_dirs: vec![],
            iquote_dirs: vec![],
            depfile: None,
            non_deterministic: false,
            key_args_filter: vec![],
        };
        let mut prost = request_to_prost(&request, "generic-tool-exec");
        match prost.body {
            Some(zccache::protocol::wire_prost::zccache_v1::request::Body::GenericToolExec(
                ref mut exec,
            )) => exec.tool_hash = Some(vec![1, 2, 3]),
            _ => panic!("expected generic-tool-exec body"),
        }
        let err = request_from_prost(prost).unwrap_err();
        assert!(err.contains("tool_hash"), "unexpected error: {err}");
    }

    #[test]
    fn compile_result_response_roundtrips() {
        roundtrip_response(Response::CompileResult {
            exit_code: 1,
            stdout: Arc::new(b"warning".to_vec()),
            stderr: Arc::new(b"error".to_vec()),
            cached: true,
        });
    }

    #[test]
    fn link_result_response_roundtrips() {
        roundtrip_response(Response::LinkResult {
            exit_code: 0,
            stdout: Arc::new(Vec::new()),
            stderr: Arc::new(b"link-err".to_vec()),
            cached: false,
            warning: Some("non-deterministic archive flags".to_string()),
        });
        roundtrip_response(Response::LinkResult {
            exit_code: 0,
            stdout: Arc::new(Vec::new()),
            stderr: Arc::new(Vec::new()),
            cached: true,
            warning: None,
        });
    }

    #[test]
    fn session_response_family_roundtrips() {
        roundtrip_response(Response::SessionStarted {
            session_id: "sess-1".to_string(),
            journal_path: Some("/proj/journal.jsonl".into()),
        });
        roundtrip_response(Response::SessionEnded {
            stats: Some(sample_stats()),
        });
        roundtrip_response(Response::SessionEnded { stats: None });
        roundtrip_response(Response::SessionStatsResult {
            stats: Some(sample_stats()),
        });
    }

    #[test]
    fn lookup_and_store_response_roundtrips() {
        roundtrip_response(Response::LookupResult(LookupResult::Hit {
            artifact: sample_artifact(),
        }));
        roundtrip_response(Response::LookupResult(LookupResult::Miss));
        roundtrip_response(Response::StoreResult(StoreResult::Stored));
        roundtrip_response(Response::StoreResult(StoreResult::AlreadyExists));
    }

    #[test]
    fn fingerprint_response_family_roundtrips() {
        roundtrip_response(Response::FingerprintCheckResult {
            decision: "run".to_string(),
            reason: Some("content changed".to_string()),
            changed_files: vec!["src/main.rs".to_string()],
        });
        roundtrip_response(Response::FingerprintAck);
    }

    #[test]
    fn rust_artifact_list_response_roundtrips() {
        roundtrip_response(Response::RustArtifactList {
            artifacts: vec![RustArtifactInfo {
                cache_key: "abc".to_string(),
                output_names: vec!["libfoo.rlib".to_string()],
                payload_count: 2,
            }],
        });
    }

    #[test]
    fn generic_tool_exec_response_roundtrips() {
        roundtrip_response(Response::GenericToolExecResult {
            exit_code: 0,
            stdout: Arc::new(b"tool-out".to_vec()),
            stderr: Arc::new(Vec::new()),
            output_files: vec![ArtifactOutput {
                name: "gen.rs".to_string(),
                payload: ArtifactPayload::Bytes(Arc::new(vec![5, 6])),
            }],
            cached: true,
            cache_key_hex: "deadbeef".to_string(),
        });
    }

    #[test]
    fn backpressure_response_roundtrips() {
        roundtrip_response(Response::Backpressure {
            queue_depth: 12,
            retry_after_ms: 250,
            reason: "compile_queue_full".to_string(),
        });
    }

    #[test]
    fn control_converters_reject_non_control_families() {
        use zccache::protocol::wire_prost::{
            supported_control_request_from_prost, supported_control_response_from_prost,
        };
        let request = Request::SessionEnd {
            session_id: "sess-1".to_string(),
        };
        let prost = request_to_prost(&request, "session-end");
        assert!(supported_control_request_from_prost(prost).is_err());

        let response = Response::FingerprintAck;
        let prost = response_to_prost(&response, "resp-1");
        assert!(supported_control_response_from_prost(prost).is_err());
    }
}
