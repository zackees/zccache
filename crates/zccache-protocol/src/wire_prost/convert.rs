//! Shared field-level conversion helpers between internal protocol types and
//! the generated v16 prost `zccache_v1` schema.
//!
//! These helpers stay `pub(super)` so the request and response converters in
//! sibling modules can share them, but no helper escapes the `wire_prost`
//! module — callers go through the higher-level entry points.

use super::zccache_v1;

pub(super) fn env_pairs_to_prost(env: &[(String, String)]) -> Vec<zccache_v1::EnvVar> {
    env.iter()
        .map(|(name, value)| zccache_v1::EnvVar {
            name: name.clone(),
            value: value.clone(),
        })
        .collect()
}

pub(super) fn env_pairs_from_prost(env: Vec<zccache_v1::EnvVar>) -> Vec<(String, String)> {
    env.into_iter().map(|var| (var.name, var.value)).collect()
}

pub(super) fn optional_env_to_prost(
    env: Option<&[(String, String)]>,
) -> (Vec<zccache_v1::EnvVar>, bool) {
    match env {
        Some(env) => (env_pairs_to_prost(env), true),
        None => (Vec::new(), false),
    }
}

pub(super) fn optional_env_from_prost(
    env: Vec<zccache_v1::EnvVar>,
    env_is_set: bool,
) -> Option<Vec<(String, String)>> {
    env_is_set.then(|| env_pairs_from_prost(env))
}

pub(super) fn paths_to_prost(paths: &[zccache_core::NormalizedPath]) -> Vec<zccache_v1::Path> {
    paths.iter().map(path_to_prost).collect()
}

pub(super) fn paths_from_prost(paths: Vec<zccache_v1::Path>) -> Vec<zccache_core::NormalizedPath> {
    paths.into_iter().map(path_from_prost).collect()
}

pub(super) fn private_daemon_session_options_to_prost(
    options: &crate::PrivateDaemonSessionOptions,
) -> zccache_v1::PrivateDaemonSessionOptions {
    zccache_v1::PrivateDaemonSessionOptions {
        daemon_name: options.daemon_name.clone(),
        endpoint: options.endpoint.clone(),
        cache_dir: options.cache_dir.as_ref().map(path_to_prost),
        owner_pids: options.owner_pids.clone(),
        env: env_pairs_to_prost(&options.env),
    }
}

pub(super) fn private_daemon_session_options_from_prost(
    options: zccache_v1::PrivateDaemonSessionOptions,
) -> crate::PrivateDaemonSessionOptions {
    crate::PrivateDaemonSessionOptions {
        daemon_name: options.daemon_name,
        endpoint: options.endpoint,
        cache_dir: options.cache_dir.map(path_from_prost),
        owner_pids: options.owner_pids,
        env: env_pairs_from_prost(options.env),
    }
}

pub(super) fn artifact_data_to_prost(artifact: &crate::ArtifactData) -> zccache_v1::ArtifactData {
    zccache_v1::ArtifactData {
        outputs: artifact
            .outputs
            .iter()
            .map(artifact_output_to_prost)
            .collect(),
        stdout: artifact.stdout.as_ref().clone(),
        stderr: artifact.stderr.as_ref().clone(),
        exit_code: artifact.exit_code,
    }
}

pub(super) fn artifact_data_from_prost(
    artifact: zccache_v1::ArtifactData,
) -> Result<crate::ArtifactData, String> {
    Ok(crate::ArtifactData {
        outputs: artifact
            .outputs
            .into_iter()
            .map(artifact_output_from_prost)
            .collect::<Result<Vec<_>, _>>()?,
        stdout: std::sync::Arc::new(artifact.stdout),
        stderr: std::sync::Arc::new(artifact.stderr),
        exit_code: artifact.exit_code,
    })
}

pub(super) fn artifact_output_to_prost(
    output: &crate::ArtifactOutput,
) -> zccache_v1::ArtifactOutput {
    zccache_v1::ArtifactOutput {
        name: output.name.clone(),
        payload: Some(artifact_payload_to_prost(&output.payload)),
    }
}

pub(super) fn artifact_output_from_prost(
    output: zccache_v1::ArtifactOutput,
) -> Result<crate::ArtifactOutput, String> {
    Ok(crate::ArtifactOutput {
        payload: artifact_payload_from_prost(required_prost_field(
            output.payload,
            "ArtifactOutput.payload",
        )?)?,
        name: output.name,
    })
}

fn artifact_payload_to_prost(payload: &crate::ArtifactPayload) -> zccache_v1::ArtifactPayload {
    use zccache_v1::artifact_payload::Body;

    zccache_v1::ArtifactPayload {
        body: Some(match payload {
            crate::ArtifactPayload::Bytes(bytes) => Body::Bytes(bytes.as_ref().clone()),
            crate::ArtifactPayload::Path(path) => Body::Path(path_to_prost(path)),
        }),
    }
}

fn artifact_payload_from_prost(
    payload: zccache_v1::ArtifactPayload,
) -> Result<crate::ArtifactPayload, String> {
    use zccache_v1::artifact_payload::Body;

    match payload.body {
        Some(Body::Bytes(bytes)) => Ok(crate::ArtifactPayload::Bytes(std::sync::Arc::new(bytes))),
        Some(Body::Path(path)) => Ok(crate::ArtifactPayload::Path(path_from_prost(path))),
        None => Err("missing required v16 prost field ArtifactPayload.body".to_string()),
    }
}

pub(super) fn lookup_result_to_prost(result: &crate::LookupResult) -> zccache_v1::LookupResult {
    use zccache_v1::lookup_result::Body;

    zccache_v1::LookupResult {
        body: Some(match result {
            crate::LookupResult::Hit { artifact } => Body::Hit(artifact_data_to_prost(artifact)),
            crate::LookupResult::Miss => Body::Miss(zccache_v1::Empty {}),
        }),
    }
}

pub(super) fn lookup_result_from_prost(
    result: zccache_v1::LookupResult,
) -> Result<crate::LookupResult, String> {
    use zccache_v1::lookup_result::Body;

    match result.body {
        Some(Body::Hit(artifact)) => Ok(crate::LookupResult::Hit {
            artifact: artifact_data_from_prost(artifact)?,
        }),
        Some(Body::Miss(_)) => Ok(crate::LookupResult::Miss),
        None => Err("missing required v16 prost field LookupResult.body".to_string()),
    }
}

pub(super) fn store_result_kind_to_prost(
    result: &crate::StoreResult,
) -> zccache_v1::StoreResultKind {
    match result {
        crate::StoreResult::Stored => zccache_v1::StoreResultKind::Stored,
        crate::StoreResult::AlreadyExists => zccache_v1::StoreResultKind::AlreadyExists,
    }
}

pub(super) fn store_result_kind_from_prost(kind: i32) -> Result<crate::StoreResult, String> {
    match zccache_v1::StoreResultKind::try_from(kind) {
        Ok(zccache_v1::StoreResultKind::Stored) => Ok(crate::StoreResult::Stored),
        Ok(zccache_v1::StoreResultKind::AlreadyExists) => Ok(crate::StoreResult::AlreadyExists),
        Ok(zccache_v1::StoreResultKind::Unspecified) | Err(_) => Err(format!(
            "invalid v16 prost StoreResult.kind value {kind}; expected Stored or AlreadyExists"
        )),
    }
}

pub(super) fn exec_output_streams_to_prost(
    streams: crate::ExecOutputStreams,
) -> zccache_v1::ExecOutputStreams {
    zccache_v1::ExecOutputStreams {
        stdout: streams.stdout,
        stderr: streams.stderr,
    }
}

pub(super) fn exec_output_streams_from_prost(
    streams: zccache_v1::ExecOutputStreams,
) -> crate::ExecOutputStreams {
    crate::ExecOutputStreams {
        stdout: streams.stdout,
        stderr: streams.stderr,
    }
}

pub(super) fn exec_cache_policy_to_prost(
    policy: crate::ExecCachePolicy,
) -> zccache_v1::ExecCachePolicy {
    match policy {
        crate::ExecCachePolicy::Normal => zccache_v1::ExecCachePolicy::Normal,
        crate::ExecCachePolicy::Bypass => zccache_v1::ExecCachePolicy::Bypass,
        crate::ExecCachePolicy::ReadOnly => zccache_v1::ExecCachePolicy::ReadOnly,
    }
}

pub(super) fn exec_cache_policy_from_prost(policy: i32) -> Result<crate::ExecCachePolicy, String> {
    match zccache_v1::ExecCachePolicy::try_from(policy) {
        Ok(zccache_v1::ExecCachePolicy::Normal) => Ok(crate::ExecCachePolicy::Normal),
        Ok(zccache_v1::ExecCachePolicy::Bypass) => Ok(crate::ExecCachePolicy::Bypass),
        Ok(zccache_v1::ExecCachePolicy::ReadOnly) => Ok(crate::ExecCachePolicy::ReadOnly),
        Ok(zccache_v1::ExecCachePolicy::Unspecified) | Err(_) => Err(format!(
            "invalid v16 prost GenericToolExec.cache_policy value {policy}; \
             expected Normal, Bypass, or ReadOnly"
        )),
    }
}

pub(super) fn tool_hash_from_prost(hash: Option<Vec<u8>>) -> Result<Option<[u8; 32]>, String> {
    match hash {
        None => Ok(None),
        Some(bytes) => <[u8; 32]>::try_from(bytes.as_slice())
            .map(Some)
            .map_err(|_| {
                format!(
                    "invalid v16 prost GenericToolExec.tool_hash length {}; expected 32 bytes",
                    bytes.len()
                )
            }),
    }
}

pub(super) fn rust_artifact_info_to_prost(
    info: &crate::RustArtifactInfo,
) -> zccache_v1::RustArtifactInfo {
    zccache_v1::RustArtifactInfo {
        cache_key: info.cache_key.clone(),
        output_names: info.output_names.clone(),
        payload_count: info.payload_count as u64,
    }
}

pub(super) fn rust_artifact_info_from_prost(
    info: zccache_v1::RustArtifactInfo,
) -> Result<crate::RustArtifactInfo, String> {
    Ok(crate::RustArtifactInfo {
        payload_count: usize::try_from(info.payload_count).map_err(|_| {
            format!(
                "invalid v16 prost RustArtifactInfo.payload_count value {}; exceeds usize",
                info.payload_count
            )
        })?,
        cache_key: info.cache_key,
        output_names: info.output_names,
    })
}

pub(super) fn session_stats_to_prost(stats: &crate::SessionStats) -> zccache_v1::SessionStats {
    zccache_v1::SessionStats {
        duration_ms: stats.duration_ms,
        compilations: stats.compilations,
        hits: stats.hits,
        misses: stats.misses,
        non_cacheable: stats.non_cacheable,
        errors: stats.errors,
        errors_cached: stats.errors_cached,
        time_saved_ms: stats.time_saved_ms,
        unique_sources: stats.unique_sources,
        bytes_read: stats.bytes_read,
        bytes_written: stats.bytes_written,
        phase_profile: stats.phase_profile.as_ref().map(phase_profile_to_prost),
        lookup_outcomes: Some(lookup_outcomes_to_prost(&stats.lookup_outcomes)),
    }
}

pub(super) fn session_stats_from_prost(stats: zccache_v1::SessionStats) -> crate::SessionStats {
    crate::SessionStats {
        duration_ms: stats.duration_ms,
        compilations: stats.compilations,
        hits: stats.hits,
        misses: stats.misses,
        non_cacheable: stats.non_cacheable,
        errors: stats.errors,
        errors_cached: stats.errors_cached,
        time_saved_ms: stats.time_saved_ms,
        unique_sources: stats.unique_sources,
        bytes_read: stats.bytes_read,
        bytes_written: stats.bytes_written,
        phase_profile: stats.phase_profile.map(phase_profile_from_prost),
        lookup_outcomes: stats
            .lookup_outcomes
            .map(lookup_outcomes_from_prost)
            .unwrap_or_default(),
    }
}

fn lookup_outcomes_to_prost(outcomes: &crate::LookupOutcomes) -> zccache_v1::LookupOutcomes {
    zccache_v1::LookupOutcomes {
        depgraph_hit_artifact_hit: outcomes.depgraph_hit_artifact_hit,
        depgraph_hit_artifact_miss: outcomes.depgraph_hit_artifact_miss,
        depgraph_cold_skip: outcomes.depgraph_cold_skip,
        depgraph_other_miss: outcomes.depgraph_other_miss.clone().into_iter().collect(),
    }
}

fn lookup_outcomes_from_prost(outcomes: zccache_v1::LookupOutcomes) -> crate::LookupOutcomes {
    crate::LookupOutcomes {
        depgraph_hit_artifact_hit: outcomes.depgraph_hit_artifact_hit,
        depgraph_hit_artifact_miss: outcomes.depgraph_hit_artifact_miss,
        depgraph_cold_skip: outcomes.depgraph_cold_skip,
        depgraph_other_miss: outcomes.depgraph_other_miss.into_iter().collect(),
    }
}

fn phase_profile_to_prost(profile: &crate::PhaseProfileSummary) -> zccache_v1::PhaseProfileSummary {
    zccache_v1::PhaseProfileSummary {
        hit_count: profile.hit_count,
        miss_count: profile.miss_count,
        parse_args_ns: profile.parse_args_ns,
        build_context_ns: profile.build_context_ns,
        hash_source_ns: profile.hash_source_ns,
        hash_headers_ns: profile.hash_headers_ns,
        depgraph_check_ns: profile.depgraph_check_ns,
        request_cache_lookup_ns: profile.request_cache_lookup_ns,
        cross_root_validate_ns: profile.cross_root_validate_ns,
        artifact_lookup_ns: profile.artifact_lookup_ns,
        write_output_ns: profile.write_output_ns,
        bookkeeping_ns: profile.bookkeeping_ns,
        total_hit_ns: profile.total_hit_ns,
        compiler_exec_ns: profile.compiler_exec_ns,
        include_scan_ns: profile.include_scan_ns,
        hash_all_ns: profile.hash_all_ns,
        artifact_store_ns: profile.artifact_store_ns,
        total_miss_ns: profile.total_miss_ns,
        staged: Some(staged_profile_to_prost(&profile.staged)),
    }
}

fn staged_profile_to_prost(
    profile: &crate::StagedProfileSummary,
) -> zccache_v1::StagedProfileSummary {
    zccache_v1::StagedProfileSummary {
        counters: profile.counters.clone().into_iter().collect(),
        timings_ns: profile.timings_ns.clone().into_iter().collect(),
        bytes: profile.bytes.clone().into_iter().collect(),
        failures: profile.failures.clone().into_iter().collect(),
    }
}

fn staged_profile_from_prost(
    profile: zccache_v1::StagedProfileSummary,
) -> crate::StagedProfileSummary {
    crate::StagedProfileSummary {
        counters: profile.counters.into_iter().collect(),
        timings_ns: profile.timings_ns.into_iter().collect(),
        bytes: profile.bytes.into_iter().collect(),
        failures: profile.failures.into_iter().collect(),
    }
}

fn phase_profile_from_prost(
    profile: zccache_v1::PhaseProfileSummary,
) -> crate::PhaseProfileSummary {
    crate::PhaseProfileSummary {
        hit_count: profile.hit_count,
        miss_count: profile.miss_count,
        parse_args_ns: profile.parse_args_ns,
        build_context_ns: profile.build_context_ns,
        hash_source_ns: profile.hash_source_ns,
        hash_headers_ns: profile.hash_headers_ns,
        depgraph_check_ns: profile.depgraph_check_ns,
        request_cache_lookup_ns: profile.request_cache_lookup_ns,
        cross_root_validate_ns: profile.cross_root_validate_ns,
        artifact_lookup_ns: profile.artifact_lookup_ns,
        write_output_ns: profile.write_output_ns,
        bookkeeping_ns: profile.bookkeeping_ns,
        total_hit_ns: profile.total_hit_ns,
        compiler_exec_ns: profile.compiler_exec_ns,
        include_scan_ns: profile.include_scan_ns,
        hash_all_ns: profile.hash_all_ns,
        artifact_store_ns: profile.artifact_store_ns,
        total_miss_ns: profile.total_miss_ns,
        staged: profile
            .staged
            .map(staged_profile_from_prost)
            .unwrap_or_default(),
    }
}

pub(super) fn daemon_status_to_prost(status: &crate::DaemonStatus) -> zccache_v1::DaemonStatus {
    zccache_v1::DaemonStatus {
        version: status.version.clone(),
        daemon_namespace: status.daemon_namespace.clone(),
        endpoint: status.endpoint.clone(),
        private_daemon: Some(private_daemon_status_to_prost(&status.private_daemon)),
        artifact_count: status.artifact_count,
        cache_size_bytes: status.cache_size_bytes,
        metadata_entries: status.metadata_entries,
        uptime_secs: status.uptime_secs,
        cache_hits: status.cache_hits,
        cache_misses: status.cache_misses,
        total_compilations: status.total_compilations,
        non_cacheable: status.non_cacheable,
        compile_errors: status.compile_errors,
        compile_errors_cached: status.compile_errors_cached,
        time_saved_ms: status.time_saved_ms,
        total_links: status.total_links,
        link_hits: status.link_hits,
        link_misses: status.link_misses,
        link_non_cacheable: status.link_non_cacheable,
        dep_graph_contexts: status.dep_graph_contexts,
        dep_graph_files: status.dep_graph_files,
        sessions_total: status.sessions_total,
        sessions_active: status.sessions_active,
        cache_dir: Some(path_to_prost(&status.cache_dir)),
        dep_graph_version: status.dep_graph_version,
        dep_graph_disk_size: status.dep_graph_disk_size,
        dep_graph_persisted: status.dep_graph_persisted,
    }
}

pub(super) fn daemon_status_from_prost(
    status: zccache_v1::DaemonStatus,
) -> Result<crate::DaemonStatus, String> {
    Ok(crate::DaemonStatus {
        version: status.version,
        daemon_namespace: status.daemon_namespace,
        endpoint: status.endpoint,
        private_daemon: private_daemon_status_from_prost(required_prost_field(
            status.private_daemon,
            "DaemonStatus.private_daemon",
        )?),
        artifact_count: status.artifact_count,
        cache_size_bytes: status.cache_size_bytes,
        metadata_entries: status.metadata_entries,
        uptime_secs: status.uptime_secs,
        cache_hits: status.cache_hits,
        cache_misses: status.cache_misses,
        total_compilations: status.total_compilations,
        non_cacheable: status.non_cacheable,
        compile_errors: status.compile_errors,
        compile_errors_cached: status.compile_errors_cached,
        time_saved_ms: status.time_saved_ms,
        total_links: status.total_links,
        link_hits: status.link_hits,
        link_misses: status.link_misses,
        link_non_cacheable: status.link_non_cacheable,
        dep_graph_contexts: status.dep_graph_contexts,
        dep_graph_files: status.dep_graph_files,
        sessions_total: status.sessions_total,
        sessions_active: status.sessions_active,
        cache_dir: path_from_prost(required_prost_field(
            status.cache_dir,
            "DaemonStatus.cache_dir",
        )?),
        dep_graph_version: status.dep_graph_version,
        dep_graph_disk_size: status.dep_graph_disk_size,
        dep_graph_persisted: status.dep_graph_persisted,
    })
}

fn private_daemon_status_to_prost(
    status: &crate::PrivateDaemonStatus,
) -> zccache_v1::PrivateDaemonStatus {
    zccache_v1::PrivateDaemonStatus {
        enabled: status.enabled,
        owners: status
            .owners
            .iter()
            .map(|owner| zccache_v1::PrivateDaemonOwnerStatus {
                pid: owner.pid,
                ref_count: owner.ref_count,
            })
            .collect(),
        private_env_keys: status.private_env_keys.clone(),
    }
}

fn private_daemon_status_from_prost(
    status: zccache_v1::PrivateDaemonStatus,
) -> crate::PrivateDaemonStatus {
    crate::PrivateDaemonStatus {
        enabled: status.enabled,
        owners: status
            .owners
            .into_iter()
            .map(|owner| crate::PrivateDaemonOwnerStatus {
                pid: owner.pid,
                ref_count: owner.ref_count,
            })
            .collect(),
        private_env_keys: status.private_env_keys,
    }
}

pub(super) fn path_to_prost(path: &zccache_core::NormalizedPath) -> zccache_v1::Path {
    zccache_v1::Path {
        value: path.as_path().to_string_lossy().into_owned(),
    }
}

pub(super) fn path_from_prost(path: zccache_v1::Path) -> zccache_core::NormalizedPath {
    zccache_core::NormalizedPath::from(path.value)
}

pub(super) fn required_prost_field<T>(value: Option<T>, field: &str) -> Result<T, String> {
    value.ok_or_else(|| format!("missing required v16 prost field {field}"))
}
