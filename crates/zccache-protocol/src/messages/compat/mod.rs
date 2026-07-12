//! Bincode compatibility and protocol roundtrip tests.
//!
//! Split out of the original single-file `compat.rs` to stay under the
//! 1,000 LOC per-file cap. Submodules group related roundtrip tests by
//! domain (variant indices, session lifecycle, fingerprint, etc.); shared
//! helpers live here so each submodule can reach them via `super::*`.

use super::*;
use serde::Serialize;
use std::sync::Arc;
use zccache_core::NormalizedPath;

mod artifact_payload;
mod clear;
mod daemon_status;
mod ephemeral;
mod exec_probe;
mod fingerprint;
mod generic_exec;
mod rust_artifacts;
mod session_lifecycle;
mod session_stats;
mod variant_indices;

/// Helper: roundtrip a value through bincode.
pub(super) fn roundtrip<
    T: Serialize + serde::de::DeserializeOwned + PartialEq + std::fmt::Debug,
>(
    val: &T,
) {
    let bytes = bincode::serialize(val).unwrap();
    let decoded: T = bincode::deserialize(&bytes).unwrap();
    assert_eq!(*val, decoded);
}

pub(super) fn variant_index<T: Serialize>(val: &T) -> u32 {
    let bytes = bincode::serialize(val).unwrap();
    u32::from_le_bytes(bytes[0..4].try_into().unwrap())
}

pub(super) fn sample_session_stats() -> SessionStats {
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
        lookup_outcomes: LookupOutcomes::default(),
        phase_profile: None,
    }
}

pub(super) fn sample_daemon_status() -> DaemonStatus {
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

pub(super) fn sample_artifact() -> ArtifactData {
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

// Compile-time check: PROTOCOL_VERSION must be positive.
const _: () = assert!(super::super::PROTOCOL_VERSION > 0);
// Compile-time check: PROTOCOL_VERSION == 18 after staged telemetry was added.
// v17 added ExecProbe/ExecStore (issue #838 slice 1) for caller-owned tool caching (e.g. the
// PyO3 `zccache.exec` binding for Python build orchestrators). v15 was
// the pin after `ReleaseWorktreeHandles` was added for soldr Tier 3
// worktree teardown (issue #690). v14 was the pin after private daemon
// SessionStart/status diagnostics were added. v13 was the pin after daemon
// namespace diagnostics were added to DaemonStatus. v12 was the pin after
// cached-error counters were added for rustc negative-result caching. v11
// was the pin after `GenericToolExec` gained Path A (include scan) + Path B
// (depfile) + non_deterministic + key_args_filter, fully implementing issue
// #272. v10 was the prior pin when `GenericToolExec` was added. v9 was the
// pin after SessionStats gained `phase_profile`. v8 was the pin after
// Compile/CompileEphemeral gained `stdin` and ArtifactPayload replaced
// ArtifactOutput.data: Arc<Vec<u8>> (issue #296 Option B).
const _FINGERPRINT_VERSION: () = assert!(super::super::PROTOCOL_VERSION == 18);
