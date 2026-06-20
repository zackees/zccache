//! Session-start, session-end, session-stats subcommands and their JSON helpers.

use crate::core::NormalizedPath;
use std::ffi::OsString;
use std::path::Path;
use std::process::ExitCode;

use super::super::session_end_idempotent;
use super::daemon::ensure_daemon;
use super::util::{
    connect, format_duration_ms, print_json_value, resolve_endpoint, LOST_CONNECTION_MSG,
};

#[derive(Debug, Clone, Default)]
pub(crate) struct SessionStartPrivateOptions {
    pub(crate) cache_dir: Option<NormalizedPath>,
    pub(crate) private_daemon: bool,
    pub(crate) daemon_name: Option<String>,
    pub(crate) owner_pids: Vec<u32>,
    pub(crate) private_env: Vec<(String, String)>,
}

impl SessionStartPrivateOptions {
    fn enabled(&self) -> bool {
        self.private_daemon
            || self.daemon_name.is_some()
            || !self.owner_pids.is_empty()
            || !self.private_env.is_empty()
    }

    pub(crate) fn ensure_private_identity(&mut self, explicit_endpoint: Option<&str>) {
        if !self.enabled() || self.daemon_name.is_some() {
            return;
        }

        let raw_name = explicit_endpoint
            .map(|endpoint| format!("endpoint-{endpoint}"))
            .unwrap_or_else(|| format!("private-{}", std::process::id()));
        self.daemon_name = crate::core::config::sanitize_daemon_namespace(&raw_name)
            .or_else(|| Some(format!("private-{}", std::process::id())));
    }
}

struct ScopedDaemonEnv {
    cache_dir: Option<OsString>,
    daemon_namespace: Option<OsString>,
}

impl ScopedDaemonEnv {
    fn apply(options: &SessionStartPrivateOptions) -> Self {
        let previous = Self {
            cache_dir: std::env::var_os(crate::core::config::CACHE_DIR_ENV),
            daemon_namespace: std::env::var_os(crate::core::config::DAEMON_NAMESPACE_ENV),
        };
        if let Some(cache_dir) = options.cache_dir.as_ref() {
            std::env::set_var(crate::core::config::CACHE_DIR_ENV, cache_dir.as_os_str());
        }
        if let Some(name) = options.daemon_name.as_deref() {
            if let Some(namespace) = crate::core::config::sanitize_daemon_namespace(name) {
                std::env::set_var(crate::core::config::DAEMON_NAMESPACE_ENV, namespace);
            }
        }
        previous
    }
}

impl Drop for ScopedDaemonEnv {
    fn drop(&mut self) {
        match &self.cache_dir {
            Some(value) => std::env::set_var(crate::core::config::CACHE_DIR_ENV, value),
            None => std::env::remove_var(crate::core::config::CACHE_DIR_ENV),
        }
        match &self.daemon_namespace {
            Some(value) => std::env::set_var(crate::core::config::DAEMON_NAMESPACE_ENV, value),
            None => std::env::remove_var(crate::core::config::DAEMON_NAMESPACE_ENV),
        }
    }
}

pub(crate) fn resolve_session_start_endpoint(
    explicit: Option<&str>,
    options: &SessionStartPrivateOptions,
) -> String {
    if let Some(endpoint) = explicit {
        return endpoint.to_string();
    }
    if let Some(name) = options.daemon_name.as_deref() {
        return crate::ipc::endpoint_for_private_daemon_name(
            options.cache_dir.as_ref().map(|p| p.as_path()),
            name,
        );
    }
    if let Some(cache_dir) = options.cache_dir.as_ref() {
        return crate::ipc::endpoint_for_cache_dir(cache_dir.as_path(), None);
    }
    resolve_endpoint(None)
}

pub(crate) fn parse_private_env_assignments(
    raw: &[String],
) -> Result<Vec<(String, String)>, String> {
    let mut parsed = Vec::with_capacity(raw.len());
    for assignment in raw {
        let Some((key, value)) = assignment.split_once('=') else {
            return Err(format!(
                "--private-env must be KEY=VALUE, got `{assignment}`"
            ));
        };
        if key.is_empty() {
            return Err("--private-env key must not be empty".to_string());
        }
        if key.contains('\0') || value.contains('\0') {
            return Err("--private-env must not contain NUL bytes".to_string());
        }
        parsed.push((key.to_string(), value.to_string()));
    }
    Ok(parsed)
}

pub(crate) async fn cmd_session_start(
    endpoint: &str,
    cwd: &Path,
    log: Option<&Path>,
    track_stats: bool,
    journal: Option<NormalizedPath>,
    profile: bool,
    private_options: SessionStartPrivateOptions,
) -> ExitCode {
    let _daemon_env = ScopedDaemonEnv::apply(&private_options);
    if let Err(e) = ensure_daemon(endpoint).await {
        eprintln!("zccache[err][D]: cannot start daemon at {endpoint}: {e}");
        return ExitCode::FAILURE;
    }

    let mut conn = match connect(endpoint).await {
        Ok(c) => c,
        Err(e) => {
            eprintln!("zccache[err][C]: cannot connect to daemon at {endpoint}: {e}");
            return ExitCode::FAILURE;
        }
    };

    let wire = crate::protocol::wire_prost::full_family_wire_format_from_env();
    let request = crate::protocol::Request::SessionStart {
        client_pid: std::process::id(),
        working_dir: cwd.into(),
        log_file: log.map(NormalizedPath::from),
        track_stats,
        journal_path: journal,
        profile,
        private_daemon: private_options.enabled().then(|| {
            crate::protocol::PrivateDaemonSessionOptions {
                daemon_name: private_options
                    .daemon_name
                    .as_deref()
                    .and_then(crate::core::config::sanitize_daemon_namespace),
                endpoint: Some(endpoint.to_string()),
                cache_dir: private_options.cache_dir.clone(),
                owner_pids: private_options.owner_pids.clone(),
                env: private_options.private_env.clone(),
            }
        }),
    };
    if let Err(e) = conn.send_request(&request, wire).await {
        eprintln!("zccache[err][S]: failed to send to daemon: {e}");
        return ExitCode::FAILURE;
    }

    let recv_result = match conn.recv_response().await {
        Ok(r) => r,
        Err(e) => {
            eprintln!("zccache[err][R]: broken connection to daemon: {e}");
            return ExitCode::FAILURE;
        }
    };
    match recv_result {
        Some(crate::protocol::Response::SessionStarted {
            session_id,
            journal_path,
        }) => {
            // One-line JSON so scripts can parse both the session ID and start time:
            //   result=$(zccache session-start)
            let started_at = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            if let Some(ref jp) = journal_path {
                // Escape backslashes for valid JSON (Windows paths contain `\`)
                let jp_escaped = jp.display().to_string().replace('\\', "\\\\");
                println!(
                    r#"{{"session_id":"{}","started_at":{},"journal_path":"{}"}}"#,
                    session_id, started_at, jp_escaped
                );
            } else {
                println!(
                    r#"{{"session_id":"{}","started_at":{}}}"#,
                    session_id, started_at
                );
            }
            ExitCode::SUCCESS
        }
        Some(crate::protocol::Response::Error { message }) => {
            eprintln!("session-start failed: {message}");
            ExitCode::FAILURE
        }
        None => {
            eprintln!("{LOST_CONNECTION_MSG}");
            ExitCode::FAILURE
        }
        Some(other) => {
            eprintln!("zccache[err][U]: unexpected response from daemon: {other:?}");
            ExitCode::FAILURE
        }
    }
}

pub(crate) fn cmd_session_end(endpoint: &str, session_id: String, json: bool) -> ExitCode {
    // Thin wrapper around the shared library entry point. All daemon
    // callers (CLI, soldr, future tools) must agree on what "the daemon
    // is gone" means — see `session_end_idempotent` for the contract
    // and issue #159 for why this lives in the library.
    match session_end_idempotent(endpoint, &session_id) {
        Ok(Some(s)) => {
            if json {
                print_session_stats_json(&session_id, &s);
            } else {
                print_session_stats_human(&session_id, &s, "complete");
            }
            ExitCode::SUCCESS
        }
        // `Ok(None)` covers both:
        //   - daemon was unreachable (already logged by the library), and
        //   - daemon was reached but had no stats for this session.
        // Both are no-op successes.
        Ok(None) => {
            if json {
                print_session_stats_unavailable_json(&session_id, "stats_unavailable");
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            if json {
                print_session_stats_error_json(&session_id, &e.to_string());
            } else {
                eprintln!("zccache: session-end failed: {e}");
            }
            ExitCode::FAILURE
        }
    }
}

pub(crate) async fn cmd_session_stats(endpoint: &str, session_id: String, json: bool) -> ExitCode {
    let mut conn = match connect(endpoint).await {
        Ok(c) => c,
        Err(e) => {
            let message = format!("cannot connect to daemon at {endpoint}: {e}");
            if json {
                print_session_stats_error_json(&session_id, &message);
            } else {
                eprintln!("{message}");
            }
            return ExitCode::FAILURE;
        }
    };

    let wire = crate::protocol::wire_prost::full_family_wire_format_from_env();
    let request = crate::protocol::Request::SessionStats {
        session_id: session_id.clone(),
    };
    if let Err(e) = conn.send_request(&request, wire).await {
        let message = format!("zccache: failed to send to daemon: {e}");
        if json {
            print_session_stats_error_json(&session_id, &message);
        } else {
            eprintln!("{message}");
        }
        return ExitCode::FAILURE;
    }

    let recv_result = match conn.recv_response().await {
        Ok(r) => r,
        Err(e) => {
            let message = format!("zccache: broken connection to daemon: {e}");
            if json {
                print_session_stats_error_json(&session_id, &message);
            } else {
                eprintln!("{message}");
            }
            return ExitCode::FAILURE;
        }
    };
    match recv_result {
        Some(crate::protocol::Response::SessionStatsResult { stats }) => {
            if let Some(s) = stats {
                if json {
                    print_session_stats_json(&session_id, &s);
                } else {
                    print_session_stats_human(&session_id, &s, "active");
                }
            } else if json {
                print_session_stats_unavailable_json(&session_id, "stats_not_enabled");
            } else {
                eprintln!("Session {session_id}: stats tracking not enabled");
            }
            ExitCode::SUCCESS
        }
        Some(crate::protocol::Response::Error { message }) => {
            if json {
                print_session_stats_error_json(&session_id, &message);
            } else {
                eprintln!("session-stats failed: {message}");
            }
            ExitCode::FAILURE
        }
        None => {
            let message = LOST_CONNECTION_MSG;
            if json {
                print_session_stats_error_json(&session_id, message);
            } else {
                eprintln!("{message}");
            }
            ExitCode::FAILURE
        }
        Some(other) => {
            let message = format!("zccache: unexpected response from daemon: {other:?}");
            if json {
                print_session_stats_error_json(&session_id, &message);
            } else {
                eprintln!("{message}");
            }
            ExitCode::FAILURE
        }
    }
}

pub(crate) fn print_session_stats_human(
    session_id: &str,
    stats: &crate::protocol::SessionStats,
    state: &str,
) {
    let total = stats.hits + stats.misses;
    let hit_rate = if total > 0 {
        format!("{:.1}%", stats.hits as f64 / total as f64 * 100.0)
    } else {
        "n/a".to_string()
    };
    let label = if state == "active" {
        format!(
            "Session {session_id} (active, {})",
            format_duration_ms(stats.duration_ms)
        )
    } else {
        format!(
            "Session {session_id} {state} ({})",
            format_duration_ms(stats.duration_ms)
        )
    };
    eprintln!("{label}");
    eprintln!(
        "  {} compilations: {} hits, {} misses, {} non-cacheable, {} cached errors",
        stats.compilations, stats.hits, stats.misses, stats.non_cacheable, stats.errors_cached
    );
    eprintln!("  Hit rate: {hit_rate}");
    if stats.time_saved_ms > 0 {
        eprintln!("  Time saved: ~{}", format_duration_ms(stats.time_saved_ms));
    }
}

pub(crate) fn print_session_stats_json(session_id: &str, stats: &crate::protocol::SessionStats) {
    print_json_value(&session_stats_json(session_id, stats));
}

pub(crate) fn print_session_stats_unavailable_json(session_id: &str, reason: &str) {
    print_json_value(&session_stats_unavailable_json(session_id, reason));
}

pub(crate) fn print_session_stats_error_json(session_id: &str, error: &str) {
    print_json_value(&session_stats_error_json(session_id, error));
}

pub(crate) fn session_stats_unavailable_json(session_id: &str, reason: &str) -> serde_json::Value {
    serde_json::json!({
        "status": "unavailable",
        "session_id": session_id,
        "reason": reason,
    })
}

pub(crate) fn session_stats_error_json(session_id: &str, error: &str) -> serde_json::Value {
    serde_json::json!({
        "status": "error",
        "session_id": session_id,
        "error": error,
    })
}

pub(crate) fn session_stats_json(
    session_id: &str,
    stats: &crate::protocol::SessionStats,
) -> serde_json::Value {
    let total = stats.hits + stats.misses;
    let hit_rate = if total > 0 {
        Some(stats.hits as f64 / total as f64)
    } else {
        None
    };
    // `phase_profile` reaches downstream consumers (soldr's
    // `last-session-stats.json`, perf-harness `render_summary`) through
    // this JSON. Emit the full struct when populated so each consumer
    // can pick fields without a separate IPC roundtrip.
    let phase_profile = stats
        .phase_profile
        .as_ref()
        .map(phase_profile_summary_json)
        .unwrap_or(serde_json::Value::Null);
    serde_json::json!({
        "status": "ok",
        "session_id": session_id,
        "duration_ms": stats.duration_ms,
        "compilations": stats.compilations,
        "hits": stats.hits,
        "misses": stats.misses,
        "non_cacheable": stats.non_cacheable,
        "errors": stats.errors,
        "errors_cached": stats.errors_cached,
        "time_saved_ms": stats.time_saved_ms,
        "unique_sources": stats.unique_sources,
        "bytes_read": stats.bytes_read,
        "bytes_written": stats.bytes_written,
        "hit_rate": hit_rate,
        "lookup_outcomes": &stats.lookup_outcomes,
        "phase_profile": phase_profile,
    })
}

pub(crate) fn phase_profile_summary_json(
    p: &crate::protocol::PhaseProfileSummary,
) -> serde_json::Value {
    serde_json::json!({
        "hit_count": p.hit_count,
        "miss_count": p.miss_count,
        "parse_args_ns": p.parse_args_ns,
        "build_context_ns": p.build_context_ns,
        "hash_source_ns": p.hash_source_ns,
        "hash_headers_ns": p.hash_headers_ns,
        "depgraph_check_ns": p.depgraph_check_ns,
        "request_cache_lookup_ns": p.request_cache_lookup_ns,
        "cross_root_validate_ns": p.cross_root_validate_ns,
        "artifact_lookup_ns": p.artifact_lookup_ns,
        "write_output_ns": p.write_output_ns,
        "bookkeeping_ns": p.bookkeeping_ns,
        "total_hit_ns": p.total_hit_ns,
        "compiler_exec_ns": p.compiler_exec_ns,
        "include_scan_ns": p.include_scan_ns,
        "hash_all_ns": p.hash_all_ns,
        "artifact_store_ns": p.artifact_store_ns,
        "total_miss_ns": p.total_miss_ns,
    })
}

pub(crate) async fn query_session_stats_json(
    endpoint: &str,
    session_id: &str,
) -> serde_json::Value {
    match query_session_stats(endpoint, session_id).await {
        Ok(Some(stats)) => session_stats_json(session_id, &stats),
        Ok(None) => serde_json::json!({
            "status": "not_tracked",
            "session_id": session_id,
            "message": "session exists but stats tracking is not enabled"
        }),
        Err(err) => serde_json::json!({
            "status": "error",
            "session_id": session_id,
            "error": err
        }),
    }
}

pub(crate) async fn query_session_stats(
    endpoint: &str,
    session_id: &str,
) -> Result<Option<crate::protocol::SessionStats>, String> {
    let mut conn = connect(endpoint)
        .await
        .map_err(|err| format!("cannot connect to daemon at {endpoint}: {err}"))?;

    let wire = crate::protocol::wire_prost::full_family_wire_format_from_env();
    let request = crate::protocol::Request::SessionStats {
        session_id: session_id.to_string(),
    };
    conn.send_request(&request, wire)
        .await
        .map_err(|err| format!("failed to send session stats request: {err}"))?;

    let recv_result = conn
        .recv_response()
        .await
        .map_err(|err| format!("broken daemon connection: {err}"))?;
    match recv_result {
        Some(crate::protocol::Response::SessionStatsResult { stats }) => Ok(stats),
        Some(crate::protocol::Response::Error { message }) => Err(message),
        Some(other) => Err(format!("unexpected daemon response: {other:?}")),
        None => Err(LOST_CONNECTION_MSG.to_string()),
    }
}
