//! Full conversion between the internal `Response` enum and the v16 prost
//! `zccache_v1::Response` schema.

use super::convert::{
    artifact_output_from_prost, artifact_output_to_prost, daemon_status_from_prost,
    daemon_status_to_prost, lookup_result_from_prost, lookup_result_to_prost, path_from_prost,
    path_to_prost, rust_artifact_info_from_prost, rust_artifact_info_to_prost,
    session_stats_from_prost, session_stats_to_prost, store_result_kind_from_prost,
    store_result_kind_to_prost,
};
use super::zccache_v1;

/// Convert any internal daemon response to the v16 prost schema.
#[must_use]
pub fn response_to_prost(
    response: &crate::protocol::Response,
    request_id: &str,
) -> zccache_v1::Response {
    use zccache_v1::response::Body;

    let body = match response {
        crate::protocol::Response::Pong => Body::Pong(zccache_v1::Empty {}),
        crate::protocol::Response::ShuttingDown => Body::ShuttingDown(zccache_v1::Empty {}),
        crate::protocol::Response::Status(status) => Body::Status(daemon_status_to_prost(status)),
        crate::protocol::Response::LookupResult(result) => {
            Body::LookupResult(lookup_result_to_prost(result))
        }
        crate::protocol::Response::StoreResult(result) => {
            Body::StoreResult(zccache_v1::StoreResult {
                kind: store_result_kind_to_prost(result).into(),
            })
        }
        crate::protocol::Response::SessionStarted {
            session_id,
            journal_path,
        } => Body::SessionStarted(zccache_v1::SessionStarted {
            session_id: session_id.clone(),
            journal_path: journal_path.as_ref().map(path_to_prost),
        }),
        crate::protocol::Response::CompileResult {
            exit_code,
            stdout,
            stderr,
            cached,
        } => Body::CompileResult(zccache_v1::CompileResult {
            exit_code: *exit_code,
            stdout: stdout.as_ref().clone(),
            stderr: stderr.as_ref().clone(),
            cached: *cached,
        }),
        crate::protocol::Response::SessionEnded { stats } => {
            Body::SessionEnded(zccache_v1::SessionEnded {
                stats: stats.as_ref().map(session_stats_to_prost),
            })
        }
        crate::protocol::Response::LinkResult {
            exit_code,
            stdout,
            stderr,
            cached,
            warning,
        } => Body::LinkResult(zccache_v1::LinkResult {
            exit_code: *exit_code,
            stdout: stdout.as_ref().clone(),
            stderr: stderr.as_ref().clone(),
            cached: *cached,
            warning: warning.clone(),
        }),
        crate::protocol::Response::Error { message } => Body::Error(zccache_v1::Error {
            message: message.clone(),
        }),
        crate::protocol::Response::Cleared {
            artifacts_removed,
            metadata_cleared,
            dep_graph_contexts_cleared,
            on_disk_bytes_freed,
        } => Body::Cleared(zccache_v1::Cleared {
            artifacts_removed: *artifacts_removed,
            metadata_cleared: *metadata_cleared,
            dep_graph_contexts_cleared: *dep_graph_contexts_cleared,
            on_disk_bytes_freed: *on_disk_bytes_freed,
        }),
        crate::protocol::Response::SessionStatsResult { stats } => {
            Body::SessionStatsResult(zccache_v1::SessionStatsResult {
                stats: stats.as_ref().map(session_stats_to_prost),
            })
        }
        crate::protocol::Response::FingerprintCheckResult {
            decision,
            reason,
            changed_files,
        } => Body::FingerprintCheckResult(zccache_v1::FingerprintCheckResult {
            decision: decision.clone(),
            reason: reason.clone(),
            changed_files: changed_files.clone(),
        }),
        crate::protocol::Response::FingerprintAck => Body::FingerprintAck(zccache_v1::Empty {}),
        crate::protocol::Response::RustArtifactList { artifacts } => {
            Body::RustArtifactList(zccache_v1::RustArtifactList {
                artifacts: artifacts.iter().map(rust_artifact_info_to_prost).collect(),
            })
        }
        crate::protocol::Response::GenericToolExecResult {
            exit_code,
            stdout,
            stderr,
            output_files,
            cached,
            cache_key_hex,
        } => Body::GenericToolExecResult(zccache_v1::GenericToolExecResult {
            exit_code: *exit_code,
            stdout: stdout.as_ref().clone(),
            stderr: stderr.as_ref().clone(),
            output_files: output_files.iter().map(artifact_output_to_prost).collect(),
            cached: *cached,
            cache_key_hex: cache_key_hex.clone(),
        }),
        crate::protocol::Response::Backpressure {
            queue_depth,
            retry_after_ms,
            reason,
        } => Body::Backpressure(zccache_v1::Backpressure {
            queue_depth: *queue_depth,
            retry_after_ms: *retry_after_ms,
            reason: reason.clone(),
        }),
        crate::protocol::Response::ReleaseWorktreeHandlesResult {
            inspected,
            released,
            sessions_dropped,
            unreleased,
        } => Body::ReleaseWorktreeHandlesResult(zccache_v1::ReleaseWorktreeHandlesResult {
            inspected: *inspected,
            released: *released,
            sessions_dropped: sessions_dropped.clone(),
            unreleased: unreleased.iter().map(path_to_prost).collect(),
        }),
        // Issue #838: ExecProbeResult / ExecStoreAck are bincode-only in
        // slice 1. The prost wire lane will gain proto definitions once a
        // wheel consumer needs cross-protocol routing.
        crate::protocol::Response::ExecProbeResult { .. }
        | crate::protocol::Response::ExecStoreAck { .. } => Body::Pong(zccache_v1::Empty {}),
    };

    zccache_v1::Response {
        body: Some(body),
        request_id: request_id.to_string(),
    }
}

/// Convert any v16 prost response to the internal daemon response enum.
///
/// # Errors
///
/// Returns a clear diagnostic for a missing response body, missing required
/// nested fields, or out-of-range enum values.
pub fn response_from_prost(
    response: zccache_v1::Response,
) -> Result<crate::protocol::Response, String> {
    use zccache_v1::response::Body;

    match response.body {
        Some(Body::Pong(_)) => Ok(crate::protocol::Response::Pong),
        Some(Body::ShuttingDown(_)) => Ok(crate::protocol::Response::ShuttingDown),
        Some(Body::Status(status)) => {
            daemon_status_from_prost(status).map(crate::protocol::Response::Status)
        }
        Some(Body::LookupResult(result)) => {
            lookup_result_from_prost(result).map(crate::protocol::Response::LookupResult)
        }
        Some(Body::StoreResult(result)) => {
            store_result_kind_from_prost(result.kind).map(crate::protocol::Response::StoreResult)
        }
        Some(Body::SessionStarted(started)) => Ok(crate::protocol::Response::SessionStarted {
            session_id: started.session_id,
            journal_path: started.journal_path.map(path_from_prost),
        }),
        Some(Body::CompileResult(result)) => Ok(crate::protocol::Response::CompileResult {
            exit_code: result.exit_code,
            stdout: std::sync::Arc::new(result.stdout),
            stderr: std::sync::Arc::new(result.stderr),
            cached: result.cached,
        }),
        Some(Body::SessionEnded(ended)) => Ok(crate::protocol::Response::SessionEnded {
            stats: ended.stats.map(session_stats_from_prost),
        }),
        Some(Body::LinkResult(result)) => Ok(crate::protocol::Response::LinkResult {
            exit_code: result.exit_code,
            stdout: std::sync::Arc::new(result.stdout),
            stderr: std::sync::Arc::new(result.stderr),
            cached: result.cached,
            warning: result.warning,
        }),
        Some(Body::Error(error)) => Ok(crate::protocol::Response::Error {
            message: error.message,
        }),
        Some(Body::Cleared(cleared)) => Ok(crate::protocol::Response::Cleared {
            artifacts_removed: cleared.artifacts_removed,
            metadata_cleared: cleared.metadata_cleared,
            dep_graph_contexts_cleared: cleared.dep_graph_contexts_cleared,
            on_disk_bytes_freed: cleared.on_disk_bytes_freed,
        }),
        Some(Body::SessionStatsResult(result)) => {
            Ok(crate::protocol::Response::SessionStatsResult {
                stats: result.stats.map(session_stats_from_prost),
            })
        }
        Some(Body::FingerprintCheckResult(result)) => {
            Ok(crate::protocol::Response::FingerprintCheckResult {
                decision: result.decision,
                reason: result.reason,
                changed_files: result.changed_files,
            })
        }
        Some(Body::FingerprintAck(_)) => Ok(crate::protocol::Response::FingerprintAck),
        Some(Body::RustArtifactList(list)) => Ok(crate::protocol::Response::RustArtifactList {
            artifacts: list
                .artifacts
                .into_iter()
                .map(rust_artifact_info_from_prost)
                .collect::<Result<Vec<_>, _>>()?,
        }),
        Some(Body::GenericToolExecResult(result)) => {
            Ok(crate::protocol::Response::GenericToolExecResult {
                exit_code: result.exit_code,
                stdout: std::sync::Arc::new(result.stdout),
                stderr: std::sync::Arc::new(result.stderr),
                output_files: result
                    .output_files
                    .into_iter()
                    .map(artifact_output_from_prost)
                    .collect::<Result<Vec<_>, _>>()?,
                cached: result.cached,
                cache_key_hex: result.cache_key_hex,
            })
        }
        Some(Body::Backpressure(backpressure)) => Ok(crate::protocol::Response::Backpressure {
            queue_depth: backpressure.queue_depth,
            retry_after_ms: backpressure.retry_after_ms,
            reason: backpressure.reason,
        }),
        Some(Body::ReleaseWorktreeHandlesResult(result)) => {
            Ok(crate::protocol::Response::ReleaseWorktreeHandlesResult {
                inspected: result.inspected,
                released: result.released,
                sessions_dropped: result.sessions_dropped,
                unreleased: result.unreleased.into_iter().map(path_from_prost).collect(),
            })
        }
        None => Err("v16 prost response is missing its response body".to_string()),
    }
}
