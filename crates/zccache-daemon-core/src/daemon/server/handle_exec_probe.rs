//! `Request::ExecProbe` / `Request::ExecStore` handlers (issue #1433).
//!
//! Caller-owned tool caching: the *caller* runs the tool, the daemon only
//! computes a stable cache key from declared inputs and stores opaque
//! result bytes. This complements [`handle_generic_tool_exec`](super::handle_exec)
//! (which spawns the tool inside the daemon) and powers the upcoming PyO3
//! `zccache.exec_cached` binding for Python build orchestrators that already own
//! their subprocess lifecycle.
//!
//! Cache-key composition (domain tag `zccache-exec-probe-v1`):
//!   - caller-supplied `name` (typically tool identity or AST schema id)
//!   - sorted `(path, content-hash)` declared input file pairs
//!   - sorted `(name, value)` declared env pairs
//!   - opaque `input_extra` bytes
//!
//! Results live in the daemon's crash-safe namespaced
//! [`KvStore`](crate::artifact::KvStore), so a successful store is durable
//! before acknowledgement and survives daemon restarts.

use std::sync::Arc;

use super::util::hash_file_via_cache;
use super::SharedState;
use crate::artifact::Key;
use crate::core::NormalizedPath;
use crate::protocol::Response;

/// Domain separation tag for ExecProbe/ExecStore cache keys.
const EXEC_PROBE_KEY_DOMAIN: &[u8] = b"zccache-exec-probe-v1";
pub(super) const EXEC_PROBE_NAMESPACE: &str = "exec-probe-v1";

/// Compute the cache key for an ExecProbe / ExecStore call and look it up
/// in the persistent caller-owned exec cache.
pub(super) async fn handle_exec_probe(
    state: &Arc<SharedState>,
    name: &str,
    input_files: &[NormalizedPath],
    input_env: &[(String, String)],
    input_extra: &Arc<Vec<u8>>,
) -> Response {
    let _active_request = state.begin_cache_request();
    let _publication_guard = state.artifact_publication.read().await;
    let cache_key_hex =
        match compose_exec_probe_key(state, name, input_files, input_env, input_extra) {
            Ok(hex) => hex,
            Err(message) => return Response::Error { message },
        };
    let key = match Key::from_hex(&cache_key_hex) {
        Ok(key) => key,
        Err(error) => {
            return Response::Error {
                message: format!("invalid derived exec cache key: {error}"),
            };
        }
    };
    let cached_bytes = match state.exec_store.get_async(EXEC_PROBE_NAMESPACE, &key).await {
        Ok(bytes) => bytes.map(Arc::new),
        Err(error) => {
            return Response::Error {
                message: format!("exec cache read failed for {cache_key_hex}: {error}"),
            };
        }
    };
    Response::ExecProbeResult {
        cache_key_hex,
        cached_bytes,
        persistent: true,
    }
}

/// Write opaque result bytes under the caller-supplied cache key. Last-writer-wins.
pub(super) async fn handle_exec_store(
    state: &Arc<SharedState>,
    cache_key_hex: &str,
    result_bytes: &Arc<Vec<u8>>,
) -> Response {
    if !is_valid_cache_key_hex(cache_key_hex) {
        return Response::Error {
            message: format!(
                "invalid cache_key_hex: expected 64 lowercase hex chars, got {} chars",
                cache_key_hex.len()
            ),
        };
    }
    let key = match Key::from_hex(cache_key_hex) {
        Ok(key) => key,
        Err(error) => {
            return Response::Error {
                message: format!("invalid cache_key_hex: {error}"),
            };
        }
    };
    let _publication_guard = state.artifact_publication.read().await;
    match state
        .exec_store
        .put_async(EXEC_PROBE_NAMESPACE, &key, result_bytes.as_slice())
        .await
    {
        Ok(_) => Response::ExecStoreAck {
            stored: true,
            persistent: true,
        },
        Err(error) => Response::Error {
            message: format!("exec cache store failed for {cache_key_hex}: {error}"),
        },
    }
}

/// Compose the deterministic blake3 cache key from declared inputs.
fn compose_exec_probe_key(
    state: &Arc<SharedState>,
    name: &str,
    input_files: &[NormalizedPath],
    input_env: &[(String, String)],
    input_extra: &Arc<Vec<u8>>,
) -> Result<String, String> {
    let mut input_pairs: Vec<(String, [u8; 32])> = Vec::with_capacity(input_files.len());
    for input in input_files {
        let hash = hash_file_via_cache(state, input.as_path())
            .ok_or_else(|| format!("cannot hash input file {}", input.as_path().display()))?;
        input_pairs.push((
            input.as_path().to_string_lossy().into_owned(),
            *hash.as_bytes(),
        ));
    }
    input_pairs.sort_by(|a, b| a.0.cmp(&b.0));

    let mut env_pairs: Vec<(String, String)> = input_env.to_vec();
    env_pairs.sort();

    let mut hasher = blake3::Hasher::new();
    hasher.update(EXEC_PROBE_KEY_DOMAIN);

    hasher.update(b"name=");
    hasher.update(name.as_bytes());
    hasher.update(b"\0");

    hasher.update(b"input_files=");
    hasher.update(&(input_pairs.len() as u64).to_le_bytes());
    for (path, hash) in &input_pairs {
        hasher.update(&(path.len() as u64).to_le_bytes());
        hasher.update(path.as_bytes());
        hasher.update(hash);
    }

    hasher.update(b"input_env=");
    hasher.update(&(env_pairs.len() as u64).to_le_bytes());
    for (k, v) in &env_pairs {
        hasher.update(&(k.len() as u64).to_le_bytes());
        hasher.update(k.as_bytes());
        hasher.update(&(v.len() as u64).to_le_bytes());
        hasher.update(v.as_bytes());
    }

    hasher.update(b"input_extra=");
    hasher.update(&(input_extra.len() as u64).to_le_bytes());
    hasher.update(input_extra.as_slice());

    Ok(hasher.finalize().to_hex().to_string())
}

fn is_valid_cache_key_hex(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_cache_key_hex_is_rejected() {
        assert!(!is_valid_cache_key_hex(""));
        assert!(!is_valid_cache_key_hex("ABCDEF"));
        assert!(!is_valid_cache_key_hex(&"g".repeat(64)));
        assert!(!is_valid_cache_key_hex(&"A".repeat(64)));
        assert!(is_valid_cache_key_hex(&"a".repeat(64)));
        assert!(is_valid_cache_key_hex(&"0123456789abcdef".repeat(4)));
    }
}
