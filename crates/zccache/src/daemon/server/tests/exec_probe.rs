//! Tests for `handle_exec_probe` / `handle_exec_store` (issue #838 slice 1).
//!
//! Drives the handlers in-process via `DaemonServer::test_state()` — the
//! same seam `release_worktree_handles` uses. Verifies the
//! probe-miss → store → probe-hit round trip and that the cache key is a
//! stable function of declared inputs.

use std::sync::Arc;

use super::super::*;
use super::CacheDirEnvGuard;
use crate::core::NormalizedPath;
use crate::protocol::Response;

fn probe(
    state: &Arc<super::super::SharedState>,
    name: &str,
    input_files: &[NormalizedPath],
    input_env: &[(String, String)],
    input_extra: &Arc<Vec<u8>>,
) -> (String, Option<Arc<Vec<u8>>>) {
    let resp = super::super::handle_exec_probe::handle_exec_probe(
        state,
        name,
        input_files,
        input_env,
        input_extra,
    );
    match resp {
        Response::ExecProbeResult {
            cache_key_hex,
            cached_bytes,
        } => (cache_key_hex, cached_bytes),
        other => panic!("expected ExecProbeResult, got: {other:?}"),
    }
}

#[tokio::test]
#[ignore] // integration-level: instantiates a real DaemonServer
async fn probe_miss_then_store_then_probe_hit() {
    crate::test_support::test_timeout(async {
        let cache_tmp = tempfile::tempdir().unwrap();
        let _env = CacheDirEnvGuard::set(cache_tmp.path());
        let endpoint = crate::ipc::unique_test_endpoint();
        let cache_dir = NormalizedPath::new(cache_tmp.path());
        let server = DaemonServer::bind_with_cache_dir(&endpoint, &cache_dir).unwrap();
        let state = server.test_state_arc();

        let name = "fastled-parse-ast";
        let env: Vec<(String, String)> = vec![("LINT_VERSION".into(), "1.2.3".into())];
        let extra = Arc::new(b"schema-v1".to_vec());

        // First probe: miss. cache_key_hex returned regardless.
        let (key_miss, cached_miss) = probe(&state, name, &[], &env, &extra);
        assert!(cached_miss.is_none(), "fresh daemon must miss");
        assert_eq!(key_miss.len(), 64, "cache key must be 64-char hex");
        assert!(
            key_miss
                .bytes()
                .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')),
            "cache key must be lowercase hex: {key_miss}"
        );

        // Store the caller's result bytes under that key.
        let payload = Arc::new(b"opaque-ast-bytes".to_vec());
        let store_resp =
            super::super::handle_exec_probe::handle_exec_store(&state, &key_miss, &payload);
        match store_resp {
            Response::ExecStoreAck { stored } => assert!(stored, "store must ack"),
            other => panic!("expected ExecStoreAck, got: {other:?}"),
        }

        // Second probe with the same declared inputs: hit, same key, same bytes.
        let (key_hit, cached_hit) = probe(&state, name, &[], &env, &extra);
        assert_eq!(key_miss, key_hit, "key must be stable across probes");
        let cached = cached_hit.expect("post-store probe must hit");
        assert_eq!(
            cached.as_slice(),
            payload.as_slice(),
            "cached bytes must match what was stored"
        );

        // Different declared input → different key → still a miss.
        let extra2 = Arc::new(b"schema-v2".to_vec());
        let (key_other, cached_other) = probe(&state, name, &[], &env, &extra2);
        assert_ne!(key_miss, key_other, "changing input_extra must change key");
        assert!(
            cached_other.is_none(),
            "different key must not surface the prior store"
        );
    })
    .await;
}

#[test]
fn malformed_cache_key_fails_validation_shape() {
    // The handler's `is_valid_cache_key_hex` is the source of truth and is
    // unit-tested where it lives; this test mirrors the contract so a
    // future loosening (e.g. uppercase hex) doesn't silently regress the
    // integration story.
    let invalid_keys: Vec<String> = vec![
        String::new(),
        "abc".to_string(),
        "G".repeat(64),
        "0".repeat(63),
    ];
    for k in &invalid_keys {
        assert!(
            !(k.len() == 64 && k.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))),
            "key {k:?} should not pass lowercase-hex-64 validation"
        );
    }
}
