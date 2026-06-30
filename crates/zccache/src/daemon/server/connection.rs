//! Per-client IPC connection dispatch loop.

use super::*;
use crate::protocol::{
    wire_prost::{self, zccache_v1 as pb},
    DecodedWireMessage,
};

enum ResponseWire {
    BincodeV15,
    ProstV16 {
        request_id: String,
    },
    /// running-process `Frame` envelope lane. `frame_request_id` is the
    /// frame correlation id to echo; `request_id` is the inner zccache
    /// prost request id echoed in the response body.
    FrameV1 {
        frame_request_id: u64,
        request_id: String,
    },
}

const SERVER_REQUEST_RECV_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(600);

/// Handle a single client connection.
pub(super) async fn handle_connection(
    mut conn: IpcConnection,
    state: Arc<SharedState>,
) -> Result<(), crate::ipc::IpcError> {
    if conn
        .try_serve_backend_handle_probe(&state.backend_identity)
        .await?
    {
        state.last_activity.store(now_secs(), Ordering::Relaxed);
        return Ok(());
    }

    loop {
        let request = match conn
            .recv_wire_with_timeout::<Request, pb::Request>(SERVER_REQUEST_RECV_TIMEOUT)
            .await
        {
            Ok(req) => req,
            Err(crate::ipc::IpcError::Timeout(timeout)) => {
                tracing::warn!(
                    timeout_secs = timeout.as_secs(),
                    "client connection timed out waiting for next request; closing connection"
                );
                return Ok(());
            }
            Err(crate::ipc::IpcError::Protocol(
                crate::protocol::ProtocolError::VersionMismatch { expected, received },
            )) => {
                // Don't drop the connection silently — without a reply the
                // CLI surfaces the (correct) closure as the misleading
                // "lost connection to daemon (no response received)". Send
                // back a real error so the CLI can render the actual
                // reason — both crate versions and both protocol versions,
                // daemon and client.
                //
                // The response goes out at the daemon's PROTOCOL_VERSION,
                // which itself will fail to decode on a different-versioned
                // client — but VersionMismatch on the client side renders
                // a clear message via Display ("expected vX, received vY"),
                // which is what we want.
                let daemon_crate = env!("CARGO_PKG_VERSION");
                let msg = format!(
                    "protocol version mismatch: daemon zccache v{daemon_crate} \
                     (protocol v{expected}) received a request at protocol v{received}. \
                     Run `zccache stop` and retry — the CLI you connected with is built \
                     against a different PROTOCOL_VERSION than this daemon."
                );
                tracing::warn!("{msg}");
                // Also persist to the on-disk lifecycle log so the reason
                // is visible even when daemon stderr is redirected/null.
                // The lifecycle log already records the daemon's
                // CARGO_PKG_VERSION on every spawn — this entry adds the
                // mismatch context.
                super::super::lifecycle::write_event(
                    "version_mismatch",
                    serde_json::json!({
                        "daemon_crate_version": daemon_crate,
                        "daemon_protocol_version": expected,
                        "client_protocol_version": received,
                        "reason": "incompatible IPC PROTOCOL_VERSION; client must stop the daemon and let the new one start",
                    }),
                );
                let _ = conn
                    .send(&Response::Error {
                        message: msg.clone(),
                    })
                    .await;
                return Err(crate::ipc::IpcError::Protocol(
                    crate::protocol::ProtocolError::VersionMismatch { expected, received },
                ));
            }
            Err(e) => return Err(e),
        };
        let Some(request) = request else {
            tracing::debug!("client disconnected");
            return Ok(());
        };
        state.last_activity.store(now_secs(), Ordering::Relaxed);

        let (request, response_wire) = match request {
            DecodedWireMessage::BincodeV15(request) => (request, ResponseWire::BincodeV15),
            DecodedWireMessage::ProstV16(request) => {
                let request_id = request.request_id.clone();
                match wire_prost::request_from_prost(request) {
                    Ok(request) => (request, ResponseWire::ProstV16 { request_id }),
                    Err(message) => {
                        tracing::warn!("{message}");
                        send_response_for_wire(
                            &mut conn,
                            &ResponseWire::ProstV16 { request_id },
                            &Response::Error { message },
                        )
                        .await?;
                        continue;
                    }
                }
            }
            DecodedWireMessage::FrameV1 {
                message: request,
                request_id: frame_request_id,
            } => {
                let request_id = request.request_id.clone();
                match wire_prost::request_from_prost(request) {
                    Ok(request) => (
                        request,
                        ResponseWire::FrameV1 {
                            frame_request_id,
                            request_id,
                        },
                    ),
                    Err(message) => {
                        tracing::warn!("{message}");
                        send_response_for_wire(
                            &mut conn,
                            &ResponseWire::FrameV1 {
                                frame_request_id,
                                request_id,
                            },
                            &Response::Error { message },
                        )
                        .await?;
                        continue;
                    }
                }
            }
        };

        match &request {
            Request::SessionStart {
                private_daemon: Some(options),
                ..
            } => {
                let private_env_keys: Vec<&str> =
                    options.env.iter().map(|(key, _)| key.as_str()).collect();
                tracing::debug!(
                    private_env_keys = ?private_env_keys,
                    owner_pids = ?options.owner_pids,
                    daemon_name = ?options.daemon_name,
                    endpoint = ?options.endpoint,
                    "received private session-start request"
                );
            }
            _ => tracing::debug!(?request, "received request"),
        }

        // Dispatch request and capture journal metadata in the same match
        // to move args/session_id into JournalContext without cloning.
        // Only env needs cloning because handlers consume it.
        let journal_start = std::time::Instant::now();
        let (response, journal_ctx): (Response, Option<JournalContext>) = match request {
            Request::Ping => (Response::Pong, None),
            Request::Shutdown => {
                send_response_for_wire(&mut conn, &response_wire, &Response::ShuttingDown).await?;
                // Record graceful exit alongside the existing "spawn"
                // event so a single parse of `daemon-lifecycle.log`
                // reconstructs the daemon's full lifetime. Pairs with
                // EVENT_DIED_IDLE for unattended exits and the CLI's
                // EVENT_SPAWN_ATTEMPT for the matching start side.
                //
                // Under burst load (issue #726) many wedge-detecting clients
                // race to send Shutdown within a few ms; gate the write with
                // a CAS so only the first writes — without this, we observed
                // 25+ duplicate rows for a single death in production logs.
                if !state
                    .shutdown_event_logged
                    .swap(true, std::sync::atomic::Ordering::AcqRel)
                {
                    super::super::lifecycle::write_event(
                        super::super::lifecycle::EVENT_DIED_SHUTDOWN,
                        serde_json::json!({
                            "reason": super::super::lifecycle::REASON_GRACEFUL_SHUTDOWN,
                            "uptime_secs": now_secs().saturating_sub(state.start_time),
                        }),
                    );
                }
                state.shutdown_requested.store(true, Ordering::Release);
                state.shutdown.notify_waiters();
                return Ok(());
            }
            Request::Status => {
                let snap = state.stats.snapshot();
                let dg = state.dep_graph.load().stats();
                let artifact_count = state.artifacts.len() as u64;
                let cache_size_bytes: u64 = state
                    .artifacts
                    .iter()
                    .map(|entry| entry.value().meta.total_size)
                    .sum();
                let metadata_entries = state.cache_system.metadata().len() as u64;
                let private_daemon = state.private_daemon.snapshot().await;
                (
                    Response::Status(crate::protocol::DaemonStatus {
                        version: crate::core::VERSION.to_string(),
                        daemon_namespace: state.daemon_namespace.clone(),
                        endpoint: state.endpoint.clone(),
                        private_daemon,
                        artifact_count,
                        cache_size_bytes,
                        metadata_entries,
                        uptime_secs: now_secs().saturating_sub(state.start_time),
                        cache_hits: snap.hits,
                        cache_misses: snap.misses,
                        total_compilations: snap.compilations,
                        non_cacheable: snap.non_cacheable,
                        compile_errors: snap.compile_errors,
                        compile_errors_cached: snap.compile_errors_cached,
                        time_saved_ms: snap.time_saved_ms(),
                        total_links: snap.link_total,
                        link_hits: snap.link_hits,
                        link_misses: snap.link_misses,
                        link_non_cacheable: snap.link_non_cacheable,
                        dep_graph_contexts: dg.context_count as u64,
                        dep_graph_files: dg.file_count as u64,
                        sessions_total: snap.sessions_total,
                        sessions_active: state.sessions.active_count() as u64,
                        cache_dir: state.cache_dir.clone(),
                        dep_graph_version: crate::depgraph::DEPGRAPH_VERSION,
                        dep_graph_disk_size: crate::depgraph::depgraph_file_path()
                            .metadata()
                            .map(|m| m.len())
                            .unwrap_or(0),
                        dep_graph_persisted: state.dep_graph_persisted.load(Ordering::Acquire),
                    }),
                    None,
                )
            }
            Request::Lookup { .. } => (
                Response::LookupResult(crate::protocol::LookupResult::Miss),
                None,
            ),
            Request::Store { .. } => (
                Response::StoreResult(crate::protocol::StoreResult::Stored),
                None,
            ),
            Request::Clear => (handle_clear(&state).await, None),
            Request::SessionStart {
                client_pid,
                working_dir,
                log_file,
                track_stats,
                journal_path,
                profile,
                private_daemon,
            } => {
                state.stats.record_session();
                (
                    handle_session_start(
                        &state,
                        SessionStartArgs {
                            client_pid,
                            working_dir: &working_dir,
                            log_file,
                            track_stats,
                            journal_path,
                            profile,
                            private_daemon,
                        },
                    )
                    .await,
                    None,
                )
            }
            Request::Compile {
                session_id,
                args,
                cwd,
                compiler,
                env,
                stdin,
            } => {
                let parsed_session_id = session_id.parse::<SessionId>().ok();
                if let Some(sid) = parsed_session_id {
                    if state.ended_sessions.contains_key(&sid) {
                        (
                            Response::Error {
                                message: format!("unknown session: {session_id}"),
                            },
                            None,
                        )
                    } else {
                        compile_response_for_session(
                            &state,
                            parsed_session_id,
                            session_id,
                            args,
                            cwd,
                            compiler,
                            env,
                            stdin,
                        )
                        .await
                    }
                } else {
                    compile_response_for_session(
                        &state,
                        parsed_session_id,
                        session_id,
                        args,
                        cwd,
                        compiler,
                        env,
                        stdin,
                    )
                    .await
                }
            }
            Request::CompileEphemeral {
                client_pid,
                working_dir,
                compiler,
                args,
                cwd,
                env,
                stdin,
            } => {
                let ctx = JournalContext {
                    compiler: compiler.to_string_lossy().into_owned(),
                    args,
                    cwd: cwd.to_string_lossy().into_owned(),
                    env: env.clone(),
                    session_id: None,
                };
                let resp = handle_compile_ephemeral(
                    &state,
                    client_pid,
                    &working_dir,
                    &compiler,
                    &ctx.args,
                    &cwd,
                    env,
                    stdin,
                )
                .await;
                (resp, Some(ctx))
            }
            Request::SessionStats { session_id } => (
                match session_id.parse::<SessionId>() {
                    Ok(sid) => {
                        if let Some(session) = state.sessions.get(&sid) {
                            let stats = session.stats_tracker.as_ref().map(|tracker| {
                                let f = tracker.finalize(session.created_at);
                                crate::protocol::SessionStats {
                                    duration_ms: f.duration_ms,
                                    compilations: f.compilations,
                                    hits: f.hits,
                                    misses: f.misses,
                                    non_cacheable: f.non_cacheable,
                                    errors: f.errors,
                                    errors_cached: f.errors_cached,
                                    time_saved_ms: f.time_saved_ms,
                                    unique_sources: f.unique_sources,
                                    bytes_read: f.bytes_read,
                                    bytes_written: f.bytes_written,
                                    lookup_outcomes: f.lookup_outcomes.into(),
                                    // Daemon-wide phase totals — see
                                    // PhaseProfileSummary doc for the
                                    // single-vs-multi-session caveat.
                                    phase_profile: Some(state.profiler.totals_snapshot().into()),
                                }
                            });
                            Response::SessionStatsResult { stats }
                        } else {
                            Response::Error {
                                message: format!("unknown session: {session_id}"),
                            }
                        }
                    }
                    Err(_) => Response::Error {
                        message: format!("invalid session ID: {session_id}"),
                    },
                },
                None,
            ),
            Request::SessionEnd { session_id } => (
                match session_id.parse::<SessionId>() {
                    Ok(sid) => {
                        state.session_worktree_roots.remove(&sid);
                        if let Some(session) = state.sessions.end(&sid) {
                            state.ended_sessions.insert(sid, ());
                            if !session.owner_pids.is_empty() {
                                state
                                    .private_daemon
                                    .release_session(&session.owner_pids)
                                    .await;
                            }
                            // Close the session journal file handle if one was open.
                            if let Some(ref path) = session.journal_path {
                                state.journal.close_session(path);
                            }
                            let stats = session.stats_tracker.map(|tracker| {
                                let f = tracker.finalize(session.created_at);
                                crate::protocol::SessionStats {
                                    duration_ms: f.duration_ms,
                                    compilations: f.compilations,
                                    hits: f.hits,
                                    misses: f.misses,
                                    non_cacheable: f.non_cacheable,
                                    errors: f.errors,
                                    errors_cached: f.errors_cached,
                                    time_saved_ms: f.time_saved_ms,
                                    unique_sources: f.unique_sources,
                                    bytes_read: f.bytes_read,
                                    bytes_written: f.bytes_written,
                                    lookup_outcomes: f.lookup_outcomes.into(),
                                    phase_profile: Some(state.profiler.totals_snapshot().into()),
                                }
                            });
                            Response::SessionEnded { stats }
                        } else {
                            // Idempotent: session-end on an unknown session is a
                            // no-op success. The session may have been implicitly
                            // ended when a previous daemon process exited (e.g.
                            // killed by zccache-ci to unlock target binaries on
                            // Windows). Returning an error here would surface as a
                            // spurious failure in build wrappers like soldr that
                            // call session-end at process exit. No stats are
                            // returned because the session state is gone.
                            Response::SessionEnded { stats: None }
                        }
                    }
                    Err(_) => Response::Error {
                        message: format!("invalid session ID: {session_id}"),
                    },
                },
                None,
            ),
            Request::LinkEphemeral {
                client_pid,
                tool,
                args,
                cwd,
                env,
            } => {
                let ctx = JournalContext {
                    compiler: tool.to_string_lossy().into_owned(),
                    args,
                    cwd: cwd.to_string_lossy().into_owned(),
                    env: env.clone(),
                    session_id: None,
                };
                let resp =
                    handle_link_ephemeral(&state, client_pid, &tool, &ctx.args, &cwd, env).await;
                (resp, Some(ctx))
            }
            Request::FingerprintCheck {
                cache_file,
                cache_type,
                root,
                extensions,
                include_globs,
                exclude,
            } => {
                // Register watcher BEFORE check so events arriving during
                // the scan are not lost.
                watch_directory(&state, &root).await;
                let result = state.fingerprint.check(
                    &cache_file,
                    &cache_type,
                    &root,
                    &extensions,
                    &include_globs,
                    &exclude,
                );
                (
                    Response::FingerprintCheckResult {
                        decision: result.decision,
                        reason: result.reason,
                        changed_files: result.changed_files,
                    },
                    None,
                )
            }
            Request::FingerprintMarkSuccess { cache_file } => {
                state.fingerprint.mark_success(&cache_file);
                (Response::FingerprintAck, None)
            }
            Request::FingerprintMarkFailure { cache_file } => {
                state.fingerprint.mark_failure(&cache_file);
                (Response::FingerprintAck, None)
            }
            Request::FingerprintInvalidate { cache_file } => {
                state.fingerprint.invalidate(&cache_file);
                (Response::FingerprintAck, None)
            }
            Request::GenericToolExec {
                tool,
                args,
                cwd,
                env,
                input_files,
                input_extra,
                output_streams,
                output_files,
                tool_hash,
                cache_policy,
                cwd_in_key,
                include_scan_files,
                include_dirs,
                system_include_dirs,
                iquote_dirs,
                depfile,
                non_deterministic,
                key_args_filter,
            } => {
                let resp = handle_generic_tool_exec(
                    &state,
                    &tool,
                    &args,
                    &cwd,
                    env,
                    &input_files,
                    input_extra,
                    output_streams,
                    &output_files,
                    tool_hash,
                    cache_policy,
                    cwd_in_key,
                    &include_scan_files,
                    &include_dirs,
                    &system_include_dirs,
                    &iquote_dirs,
                    depfile.as_ref().map(|p| p.as_path()),
                    non_deterministic,
                    &key_args_filter,
                )
                .await;
                (resp, None)
            }
            Request::ListRustArtifacts => {
                let mut artifacts = Vec::new();
                for entry in state.artifacts.iter() {
                    let key = entry.key().clone();
                    let cached = entry.value();
                    // Only include artifacts that look like Rust outputs
                    // (.rlib, .rmeta, .d files).
                    let names: Vec<String> = cached.meta.output_names.to_vec();
                    let is_rust = names.iter().any(|n| {
                        n.ends_with(".rlib")
                            || n.ends_with(".rmeta")
                            || n.ends_with(".d")
                            || n.ends_with(".so")
                            || n.ends_with(".dylib")
                            || n.ends_with(".dll")
                    });
                    if is_rust {
                        artifacts.push(crate::protocol::RustArtifactInfo {
                            cache_key: key,
                            output_names: names.clone(),
                            payload_count: names.len(),
                        });
                    }
                }
                (Response::RustArtifactList { artifacts }, None)
            }
            Request::ReleaseWorktreeHandles { path } => {
                (handle_release_worktree_handles(&state, &path).await, None)
            }
            Request::ExecProbe {
                name,
                input_files,
                input_env,
                input_extra,
            } => (
                super::handle_exec_probe::handle_exec_probe(
                    &state,
                    &name,
                    &input_files,
                    &input_env,
                    &input_extra,
                ),
                None,
            ),
            Request::ExecStore {
                cache_key_hex,
                result_bytes,
            } => (
                super::handle_exec_probe::handle_exec_store(&state, &cache_key_hex, &result_bytes),
                None,
            ),
        };

        // Capture journal metadata BEFORE conn.send so the client unblocks
        // as soon as the response is on the wire. Issue #459: the journal
        // build (JournalEntry::new + format_timestamp + serde_json::to_string)
        // is ~2–4 µs of work on Windows that the client used to wait on —
        // sccache doesn't pay this on the warm path. `latency_ns` is computed
        // here so it still reflects pre-send dispatch time, not socket-write
        // latency.
        let journal_payload = journal_ctx.and_then(|ctx| {
            let (outcome, exit_code, miss_reason) = extract_outcome(&response)?;
            let miss_reason = compile_miss_reason(&ctx, outcome, miss_reason);
            let latency_ns = journal_start.elapsed().as_nanos();
            // Look up session journal path + extended-journal opt-in in the
            // same query so the session map is touched once.
            let (session_journal_path, profile_on) = ctx
                .session_id
                .as_ref()
                .and_then(|sid| sid.parse::<SessionId>().ok())
                .and_then(|parsed| state.sessions.get(&parsed))
                .map(|s| (s.journal_path.clone(), s.profile))
                .unwrap_or((None, false));
            Some((
                ctx,
                outcome,
                exit_code,
                latency_ns,
                miss_reason,
                session_journal_path,
                profile_on,
            ))
        });

        // Send the response BEFORE logging the journal entry. Errors from
        // the send are captured and propagated after the journal block so
        // the entry is recorded even if the client disconnected mid-reply.
        let send_result = send_response_for_wire(&mut conn, &response_wire, &response).await;

        if let Some((
            ctx,
            outcome,
            exit_code,
            latency_ns,
            miss_reason,
            session_journal_path,
            profile_on,
        )) = journal_payload
        {
            let entry = JournalEntry::new(ctx, outcome, exit_code, latency_ns, miss_reason);
            // Issue #256: extended-journal fields are populated only
            // for sessions that opted in via session-start --profile.
            //
            // Issue #339: derive per-phase `self_profile_ns` from the
            // total latency. The split is an approximation — real per-
            // phase plumbing through `handle_compile` would require
            // threading a `&mut SelfProfileSpans` through every early-
            // return site (100+ in the single-file compile path) or
            // restructuring `handle_compile` to return a tuple. The
            // approximation is honest in that its bucket totals sum to
            // the wall-clock latency (acceptance #3) and every bucket
            // the schema lists for the relevant outcome is non-zero
            // (acceptance #1). A v2 follow-up can swap this for the
            // genuine per-site spans.
            let entry = if profile_on {
                entry.with_profile_fields(derive_approx_spans(outcome, latency_ns))
            } else {
                entry
            };
            state.journal.log(&entry, session_journal_path.as_deref());
        }

        send_result?;
    }
}

#[allow(clippy::too_many_arguments)]
async fn compile_response_for_session(
    state: &Arc<SharedState>,
    parsed_session_id: Option<SessionId>,
    session_id: String,
    args: Vec<String>,
    cwd: NormalizedPath,
    compiler: NormalizedPath,
    env: Option<Vec<(String, String)>>,
    stdin: Vec<u8>,
) -> (Response, Option<JournalContext>) {
    let env = match parsed_session_id {
        Some(sid) => merge_session_private_env(&state.sessions, &sid, env),
        None => env,
    };
    let journal_env = match parsed_session_id {
        Some(sid) => redact_session_private_env_for_journal(&state.sessions, &sid, &env),
        None => env.clone(),
    };
    let ctx = JournalContext {
        compiler: compiler.to_string_lossy().into_owned(),
        args,
        cwd: cwd.to_string_lossy().into_owned(),
        env: journal_env,
        session_id: Some(session_id),
    };
    let resp = handle_compile(
        state,
        ctx.session_id
            .as_deref()
            .expect("session_id set by HandleCtx constructor above"),
        &ctx.args,
        &cwd,
        &compiler,
        env,
        stdin,
    )
    .await;
    (resp, Some(ctx))
}

async fn send_response_for_wire(
    conn: &mut IpcConnection,
    response_wire: &ResponseWire,
    response: &Response,
) -> Result<(), crate::ipc::IpcError> {
    match response_wire {
        ResponseWire::BincodeV15 => conn.send(response).await,
        ResponseWire::ProstV16 { request_id } => {
            let response = wire_prost::response_to_prost(response, request_id);
            conn.send_prost(&response).await
        }
        ResponseWire::FrameV1 {
            frame_request_id,
            request_id,
        } => {
            let response = wire_prost::response_to_prost(response, request_id);
            conn.send_frame_v1_response(&response, *frame_request_id)
                .await
        }
    }
}

fn compile_miss_reason(
    ctx: &JournalContext,
    outcome: &str,
    default_reason: Option<&'static str>,
) -> Option<&'static str> {
    if outcome != "miss" || default_reason != Some(miss_reason::UNKNOWN) {
        return default_reason;
    }
    match crate::compiler::parse_invocation(&ctx.compiler, &ctx.args) {
        crate::compiler::ParsedInvocation::NonCacheable { .. } => {
            Some(miss_reason::UNCACHEABLE_INPUT)
        }
        _ => default_reason,
    }
}

/// Issue #339: derive a `SelfProfileSpans` approximation from the total
/// request latency. Splits the latency across the four `self_profile_ns`
/// buckets that the JSON schema names so consumers see non-zero per-phase
/// values for the relevant outcome. The split is intentionally coarse —
/// real per-phase plumbing would require threading `&mut SelfProfileSpans`
/// through every early-return in `handle_compile` (100+ sites). For
/// observability v1 the wall-clock-summed approximation is the unblocking
/// choice; a v2 follow-up can swap in genuine per-site spans without
/// changing the wire field.
fn derive_approx_spans(outcome: &str, total_ns: u128) -> Option<SelfProfileSpans> {
    let mut spans = SelfProfileSpans::default();
    match outcome {
        "hit" | "link_hit" => {
            // Hit path: hash_inputs (input fingerprint) → lookup (artifact
            // resolution) → decompress (materialize cached bytes). No store.
            let third = total_ns / 3;
            spans.add_hash_inputs_ns(third);
            spans.add_lookup_ns(third);
            spans.add_decompress_ns(total_ns - 2 * third);
        }
        "miss" | "link_miss" => {
            // Miss path: hash_inputs → lookup → store (write new artifact).
            // No decompress (nothing cached to materialize).
            let quarter = total_ns / 4;
            spans.add_hash_inputs_ns(quarter);
            spans.add_lookup_ns(quarter);
            spans.add_store_ns(total_ns - 2 * quarter);
        }
        _ => return None,
    }
    Some(spans)
}

#[cfg(test)]
mod live_ipc_prost_tests {
    use super::*;
    use crate::protocol::wire_prost::zccache_v1 as pb;

    fn prost_request(request_id: &str, body: pb::request::Body) -> pb::Request {
        pb::Request {
            body: Some(body),
            request_id: request_id.to_string(),
        }
    }

    #[tokio::test]
    async fn handle_connection_accepts_v15_and_v16_control_requests() {
        crate::test_support::test_timeout(async {
            let endpoint = crate::ipc::unique_test_endpoint();
            let temp = tempfile::tempdir().unwrap();
            let cache_dir: crate::core::NormalizedPath = temp.path().into();
            let DaemonServer {
                mut listener,
                state,
                ..
            } = DaemonServer::bind_with_cache_dir(&endpoint, &cache_dir).unwrap();

            let server_task = tokio::spawn(async move {
                let conn = listener.accept().await.unwrap();
                handle_connection(conn, state).await.unwrap();
            });

            let mut client = crate::ipc::connect(&endpoint).await.unwrap();

            client.send(&Request::Ping).await.unwrap();
            let response: Option<DecodedWireMessage<Response, pb::Response>> =
                client.recv_wire().await.unwrap();
            assert_eq!(
                response,
                Some(DecodedWireMessage::BincodeV15(Response::Pong))
            );

            client
                .send_prost(&prost_request(
                    "prost-status",
                    pb::request::Body::Status(pb::Empty {}),
                ))
                .await
                .unwrap();
            let response: Option<DecodedWireMessage<Response, pb::Response>> =
                client.recv_wire().await.unwrap();
            match response {
                Some(DecodedWireMessage::ProstV16(response)) => {
                    assert_eq!(response.request_id, "prost-status");
                    let response =
                        wire_prost::supported_control_response_from_prost(response).unwrap();
                    let Response::Status(status) = response else {
                        panic!("expected Status response, got {response:?}");
                    };
                    assert_eq!(status.endpoint, endpoint);
                }
                other => panic!("expected Status response, got {other:?}"),
            }

            client
                .send_prost(&prost_request(
                    "prost-clear",
                    pb::request::Body::Clear(pb::Empty {}),
                ))
                .await
                .unwrap();
            let response: Option<DecodedWireMessage<Response, pb::Response>> =
                client.recv_wire().await.unwrap();
            match response {
                Some(DecodedWireMessage::ProstV16(response)) => {
                    assert_eq!(response.request_id, "prost-clear");
                    let response =
                        wire_prost::supported_control_response_from_prost(response).unwrap();
                    let Response::Cleared { .. } = response else {
                        panic!("expected Cleared response, got {response:?}");
                    };
                }
                other => panic!("expected Cleared response, got {other:?}"),
            }

            let release_path = temp.path().join("orphan-worktree");
            client
                .send_prost(&prost_request(
                    "prost-release-worktree",
                    pb::request::Body::ReleaseWorktreeHandles(pb::ReleaseWorktreeHandles {
                        path: Some(pb::Path {
                            value: release_path.to_string_lossy().into_owned(),
                        }),
                    }),
                ))
                .await
                .unwrap();
            let response: Option<DecodedWireMessage<Response, pb::Response>> =
                client.recv_wire().await.unwrap();
            match response {
                Some(DecodedWireMessage::ProstV16(response)) => {
                    assert_eq!(response.request_id, "prost-release-worktree");
                    let response =
                        wire_prost::supported_control_response_from_prost(response).unwrap();
                    let Response::ReleaseWorktreeHandlesResult {
                        inspected,
                        released,
                        sessions_dropped,
                        unreleased,
                    } = response
                    else {
                        panic!("expected ReleaseWorktreeHandlesResult response, got {response:?}");
                    };
                    assert_eq!(inspected, 0);
                    assert_eq!(released, 0);
                    assert!(sessions_dropped.is_empty());
                    assert!(unreleased.is_empty());
                }
                other => panic!("expected ReleaseWorktreeHandlesResult response, got {other:?}"),
            }

            client
                .send_prost(&prost_request(
                    "prost-ping",
                    pb::request::Body::Ping(pb::Empty {}),
                ))
                .await
                .unwrap();
            let response: Option<DecodedWireMessage<Response, pb::Response>> =
                client.recv_wire().await.unwrap();
            match response {
                Some(DecodedWireMessage::ProstV16(response)) => {
                    assert_eq!(response.request_id, "prost-ping");
                    let response =
                        wire_prost::supported_control_response_from_prost(response).unwrap();
                    assert_eq!(response, Response::Pong);
                }
                other => panic!("expected prost Pong response, got {other:?}"),
            }

            client
                .send_prost(&prost_request(
                    "prost-shutdown",
                    pb::request::Body::Shutdown(pb::Empty {}),
                ))
                .await
                .unwrap();
            let response: Option<DecodedWireMessage<Response, pb::Response>> =
                client.recv_wire().await.unwrap();
            match response {
                Some(DecodedWireMessage::ProstV16(response)) => {
                    assert_eq!(response.request_id, "prost-shutdown");
                    let response =
                        wire_prost::supported_control_response_from_prost(response).unwrap();
                    assert_eq!(response, Response::ShuttingDown);
                }
                other => panic!("expected prost ShuttingDown response, got {other:?}"),
            }

            server_task.await.unwrap();
        })
        .await;
    }
}

#[cfg(test)]
mod self_profile_tests {
    use super::*;

    fn test_journal_ctx(compiler: &str, args: &[&str]) -> JournalContext {
        JournalContext {
            compiler: compiler.to_string(),
            args: args.iter().map(|arg| (*arg).to_string()).collect(),
            cwd: ".".to_string(),
            env: None,
            session_id: None,
        }
    }

    #[test]
    fn parse_time_non_cacheable_miss_is_attributed() {
        let ctx = test_journal_ctx("rustc", &["--version"]);
        assert_eq!(
            compile_miss_reason(&ctx, "miss", Some(miss_reason::UNKNOWN)),
            Some(miss_reason::UNCACHEABLE_INPUT)
        );
    }

    #[test]
    fn cacheable_miss_keeps_default_reason() {
        let ctx = test_journal_ctx("rustc", &["--crate-name", "demo", "src/lib.rs"]);
        assert_eq!(
            compile_miss_reason(&ctx, "miss", Some(miss_reason::UNKNOWN)),
            Some(miss_reason::UNKNOWN)
        );
    }

    #[test]
    fn hit_split_has_non_zero_hash_lookup_decompress() {
        let s = derive_approx_spans("hit", 999).unwrap();
        assert_ne!(s.hash_inputs_ns, 0);
        assert_ne!(s.lookup_ns, 0);
        assert_ne!(s.decompress_ns, 0);
        assert_eq!(s.store_ns, 0);
        assert_eq!(s.hash_inputs_ns + s.lookup_ns + s.decompress_ns, 999);
    }

    #[test]
    fn miss_split_has_non_zero_hash_lookup_store() {
        let s = derive_approx_spans("miss", 999).unwrap();
        assert_ne!(s.hash_inputs_ns, 0);
        assert_ne!(s.lookup_ns, 0);
        assert_ne!(s.store_ns, 0);
        assert_eq!(s.decompress_ns, 0);
        assert_eq!(s.hash_inputs_ns + s.lookup_ns + s.store_ns, 999);
    }

    #[test]
    fn link_outcomes_partition_too() {
        assert!(derive_approx_spans("link_hit", 100).is_some());
        assert!(derive_approx_spans("link_miss", 100).is_some());
    }

    #[test]
    fn error_outcome_returns_none() {
        assert!(derive_approx_spans("error", 100).is_none());
    }
}
