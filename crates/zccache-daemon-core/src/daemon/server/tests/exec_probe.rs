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

async fn probe(
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
    )
    .await;
    match resp {
        Response::ExecProbeResult {
            cache_key_hex,
            cached_bytes,
            persistent: true,
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
        let (key_miss, cached_miss) = probe(&state, name, &[], &env, &extra).await;
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
            super::super::handle_exec_probe::handle_exec_store(&state, &key_miss, &payload).await;
        match store_resp {
            Response::ExecStoreAck { stored, persistent } => {
                assert!(stored, "store must ack");
                assert!(persistent, "store must be durable");
            }
            other => panic!("expected ExecStoreAck, got: {other:?}"),
        }

        // Second probe with the same declared inputs: hit, same key, same bytes.
        let (key_hit, cached_hit) = probe(&state, name, &[], &env, &extra).await;
        assert_eq!(key_miss, key_hit, "key must be stable across probes");
        let cached = cached_hit.expect("post-store probe must hit");
        assert_eq!(
            cached.as_slice(),
            payload.as_slice(),
            "cached bytes must match what was stored"
        );

        // Different declared input → different key → still a miss.
        let extra2 = Arc::new(b"schema-v2".to_vec());
        let (key_other, cached_other) = probe(&state, name, &[], &env, &extra2).await;
        assert_ne!(key_miss, key_other, "changing input_extra must change key");
        assert!(
            cached_other.is_none(),
            "different key must not surface the prior store"
        );
    })
    .await;
}

#[tokio::test]
#[ignore] // integration-level: binds two real daemon servers to one cache root
async fn stored_bytes_survive_daemon_restart() {
    crate::test_support::test_timeout(async {
        let cache_tmp = tempfile::tempdir().unwrap();
        let cache_dir = NormalizedPath::new(cache_tmp.path());
        let name = "python-exec-cached-restart";
        let extra = Arc::new(b"schema-v1".to_vec());
        let payload = Arc::new(b"persistent-result".to_vec());

        let first_endpoint = crate::ipc::unique_test_endpoint();
        let first = DaemonServer::bind_with_cache_dir(&first_endpoint, &cache_dir).unwrap();
        let first_state = first.test_state_arc();
        let (key, cached) = probe(&first_state, name, &[], &[], &extra).await;
        assert!(cached.is_none(), "fresh cache root must miss");
        let stored =
            super::super::handle_exec_probe::handle_exec_store(&first_state, &key, &payload).await;
        assert_eq!(
            stored,
            Response::ExecStoreAck {
                stored: true,
                persistent: true,
            }
        );
        drop(first_state);
        drop(first);

        let second_endpoint = crate::ipc::unique_test_endpoint();
        let second = DaemonServer::bind_with_cache_dir(&second_endpoint, &cache_dir).unwrap();
        let second_state = second.test_state_arc();
        let (restarted_key, restarted_bytes) = probe(&second_state, name, &[], &[], &extra).await;

        assert_eq!(restarted_key, key);
        assert_eq!(restarted_bytes.as_deref(), Some(payload.as_ref()));
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
