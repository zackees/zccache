//! Per-`Request` dispatch: the match arm that used to be inlined in
//! `handle_connection` (`connection.rs` before the #1154 phase-0 split).

use super::*;

/// Dispatch a single decoded [`Request`].
///
/// Returns `Ok(Some((response, journal_ctx)))` with a response (and optional
/// journal context) ready for the caller to send + log, or `Ok(None)` when
/// the handler already sent its own response (or none was needed) and the
/// connection loop must return immediately — mirrors the early
/// `return Ok(());` sites in the pre-split `handle_connection` (`Shutdown`,
/// and every `guarded_dispatch` client-cancellation path).
///
/// (`Option` rather than a two-variant enum so the ~528-byte completed
/// payload does not sit next to a zero-size "done" variant —
/// `clippy::large_enum_variant`; same rationale as `guarded_dispatch`'s
/// return type.)
pub(super) async fn dispatch_request(
    request: Request,
    conn: &mut IpcConnection,
    response_wire: &ResponseWire,
    state: &Arc<SharedState>,
) -> Result<Option<(Response, Option<PendingJournalContext>)>, crate::ipc::IpcError> {
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

    let (response, journal_ctx): (Response, Option<PendingJournalContext>) = match request {
        Request::Ping => (Response::Pong, None),
        Request::Shutdown => {
            send_response_for_wire(conn, response_wire, &Response::ShuttingDown).await?;
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
                crate::daemon::lifecycle::write_event(
                    crate::daemon::lifecycle::EVENT_DIED_SHUTDOWN,
                    serde_json::json!({
                        "reason": crate::daemon::lifecycle::REASON_GRACEFUL_SHUTDOWN,
                        "uptime_secs": now_secs().saturating_sub(state.start_time),
                    }),
                );
            }
            state.shutdown_requested.store(true, Ordering::Release);
            state.shutdown.notify_waiters();
            return Ok(None);
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
        Request::Clear => (handle_clear(state).await, None),
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
                    state,
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
            let handler = async {
                let parsed_session_id = session_id.parse::<SessionId>().ok();
                if let Some(sid) = parsed_session_id {
                    if state.ended_sessions.contains_key(&sid) {
                        return (
                            Response::Error {
                                message: format!("unknown session: {session_id}"),
                            },
                            None,
                        );
                    }
                }
                let request_profile = parsed_session_id.as_ref().and_then(|sid| {
                    state
                        .session_staged_profiles
                        .get(sid)
                        .map(|profile| Arc::clone(profile.value()))
                });
                let request = compile_response_for_session(
                    state,
                    parsed_session_id,
                    session_id,
                    args,
                    cwd,
                    compiler,
                    env,
                    stdin,
                );
                match request_profile {
                    Some(profile) => {
                        crate::daemon::staged_stats::scope_request_profile(profile, request).await
                    }
                    None => request.await,
                }
            };
            match guarded_dispatch(conn, handler).await {
                Some((response, ctx)) => (response, ctx),
                None => {
                    log_client_cancelled("compile");
                    return Ok(None);
                }
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
            let handler = async {
                let ctx = JournalContext::new(
                    compiler.to_string_lossy().into_owned(),
                    args,
                    cwd.to_string_lossy().into_owned(),
                    env.clone(),
                    None,
                );
                let (resp, attributed_miss_reason) =
                    capture_miss_reason(Box::pin(handle_compile_ephemeral(
                        state,
                        client_pid,
                        &working_dir,
                        &compiler,
                        &ctx.args,
                        &cwd,
                        env,
                        stdin,
                    )))
                    .await;
                (
                    resp,
                    Some(PendingJournalContext::new(ctx, attributed_miss_reason)),
                )
            };
            match guarded_dispatch(conn, handler).await {
                Some((response, ctx)) => (response, ctx),
                None => {
                    log_client_cancelled("compile_ephemeral");
                    return Ok(None);
                }
            }
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
                                // Legacy phase fields remain daemon-wide;
                                // staged telemetry is request/session-owned.
                                phase_profile: Some(session_phase_profile(state, &sid)),
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
                    let ended_phase_profile = session_phase_profile(state, &sid);
                    state.session_staged_profiles.remove(&sid);
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
                                phase_profile: Some(ended_phase_profile),
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
            let handler = async {
                let ctx = JournalContext::new(
                    tool.to_string_lossy().into_owned(),
                    args,
                    cwd.to_string_lossy().into_owned(),
                    env.clone(),
                    None,
                );
                let resp =
                    handle_link_ephemeral(state, client_pid, &tool, &ctx.args, &cwd, env).await;
                (resp, Some(PendingJournalContext::new(ctx, None)))
            };
            match guarded_dispatch(conn, handler).await {
                Some((response, ctx)) => (response, ctx),
                None => {
                    log_client_cancelled("link_ephemeral");
                    return Ok(None);
                }
            }
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
            watch_directory(state, &root).await;
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
            let handler = async {
                let resp = handle_generic_tool_exec(
                    state,
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
            };
            match guarded_dispatch(conn, handler).await {
                Some((response, ctx)) => (response, ctx),
                None => {
                    log_client_cancelled("generic_tool_exec");
                    return Ok(None);
                }
            }
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
            (handle_release_worktree_handles(state, &path).await, None)
        }
        Request::ExecProbe {
            name,
            input_files,
            input_env,
            input_extra,
        } => (
            super::super::handle_exec_probe::handle_exec_probe(
                state,
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
            super::super::handle_exec_probe::handle_exec_store(
                state,
                &cache_key_hex,
                &result_bytes,
            ),
            None,
        ),
    };

    Ok(Some((response, journal_ctx)))
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
) -> (Response, Option<PendingJournalContext>) {
    let env = match parsed_session_id {
        Some(sid) => merge_session_private_env(&state.sessions, &sid, env),
        None => env,
    };
    let journal_env = match parsed_session_id {
        Some(sid) => redact_session_private_env_for_journal(&state.sessions, &sid, &env),
        None => env.clone(),
    };
    let ctx = JournalContext::new(
        compiler.to_string_lossy().into_owned(),
        args,
        cwd.to_string_lossy().into_owned(),
        journal_env,
        Some(session_id),
    );
    #[expect(
        clippy::expect_used,
        reason = "ctx.session_id is set to Some(session_id) immediately above (line 704); the Option wrap is purely for the JournalContext return field"
    )]
    let (resp, attributed_miss_reason) = capture_miss_reason(Box::pin(handle_compile(
        state,
        ctx.session_id
            .as_deref()
            .expect("session_id set by JournalContext constructor above"),
        &ctx.args,
        &cwd,
        &compiler,
        env,
        stdin,
    )))
    .await;
    (
        resp,
        Some(PendingJournalContext::new(ctx, attributed_miss_reason)),
    )
}
