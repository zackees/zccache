//! `ExecProbe` / `ExecStore` (issue #838 slice 1) bincode roundtrip tests.
//!
//! These are the caller-owned-tool variants: the Python (or other foreign)
//! orchestrator runs the tool itself, calls `ExecProbe` to ask the daemon
//! whether the (name + inputs + env + extra) tuple already has a cached
//! result, and posts the bytes via `ExecStore` on a miss.

use super::*;

#[test]
fn exec_probe_request_roundtrip() {
    let req = Request::ExecProbe {
        name: "fastled-parse-ast".to_string(),
        input_files: vec!["src/foo.cpp".into(), "ci/lint_cpp_rs/rules.json".into()],
        input_env: vec![
            ("LINT_VERSION".to_string(), "1.2.3".to_string()),
            ("PATH".to_string(), "/usr/bin".to_string()),
        ],
        input_extra: Arc::new(b"schema-v1".to_vec()),
    };
    roundtrip(&req);

    // Empty-inputs path used by tools that hash only their name + extra.
    let req_empty = Request::ExecProbe {
        name: "rustc-fingerprint".to_string(),
        input_files: vec![],
        input_env: vec![],
        input_extra: Arc::new(Vec::new()),
    };
    roundtrip(&req_empty);
}

#[test]
fn exec_store_request_roundtrip() {
    let req = Request::ExecStore {
        cache_key_hex: "0123456789abcdef".repeat(4),
        result_bytes: Arc::new(b"opaque-result-bytes".to_vec()),
    };
    roundtrip(&req);
}

#[test]
fn exec_probe_result_roundtrip_miss_and_hit() {
    let miss = Response::ExecProbeResult {
        cache_key_hex: "0".repeat(64),
        cached_bytes: None,
    };
    roundtrip(&miss);

    let hit = Response::ExecProbeResult {
        cache_key_hex: "f".repeat(64),
        cached_bytes: Some(Arc::new(b"cached-ast-bytes".to_vec())),
    };
    roundtrip(&hit);
}

#[test]
fn exec_store_ack_roundtrip() {
    let ack = Response::ExecStoreAck { stored: true };
    roundtrip(&ack);
}
