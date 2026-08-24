//! Request preparation helpers shared by the compile-pipeline orchestrator.

use super::super::super::*;
use super::super::error_cache::compile_failure_stderr;
use super::system_includes::{discover_system_includes, SystemIncludesOutcome};
use super::time_macros::{find_time_macro_use, warn_time_macro_uncacheable};

const DEPGRAPH_STARTUP_WAIT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

pub(super) fn begin_prepared_request(
    state: &SharedState,
    sid: &SessionId,
    session_id: &str,
    compiler_path: &Path,
) -> (NormalizedPath, crate::daemon::lineage::Lineage, bool) {
    // Sessions can disappear across a daemon restart while wrappers retain
    // their UUID. The stat/touch helpers no-op for unknown sessions, so the
    // compile remains valid and only per-session accounting is lost.
    state.stats.record_compilation();
    let compiler = compiler_path.into();
    let lineage = crate::daemon::lineage::Lineage::current(
        session_client_pid(state, sid),
        Some(session_id.into()),
    );
    // Miss profiling alone needs the early phase clock reads. Decide once to
    // avoid paying them on every warm request.
    let want_rust_miss_profile = std::env::var_os(RUST_MISS_PROFILE_ENV).is_some();
    (compiler, lineage, want_rust_miss_profile)
}

pub(super) struct ParsedRequestArguments {
    pub(super) sid: SessionId,
    pub(super) client_env: Option<Vec<(String, String)>>,
    pub(super) dependency_mode: DependencyDiscoveryMode,
    pub(super) worktree_root: Option<NormalizedPath>,
    pub(super) effective_args: Vec<String>,
    pub(super) request_cache_key_root: Option<NormalizedPath>,
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn prepare_request_arguments(
    state: &SharedState,
    session_id: &str,
    compiler_path: &Path,
    raw_args: &[String],
    cwd: &Path,
    mut client_env: Option<Vec<(String, String)>>,
    stdin: &[u8],
) -> Result<ParsedRequestArguments, Response> {
    let sid = session_id
        .parse::<SessionId>()
        .map_err(|_| Response::Error {
            message: format!("invalid session ID: {session_id}"),
        })?;

    // Expand before request caching so mutations to an @file cannot reuse a
    // fast-hit entry keyed only by the raw argument list.
    let expanded_args = expand_args_cached(state, raw_args, cwd);
    let strict_paths_mode = strict_paths_mode_from_client_env(client_env.as_deref())
        .map_err(|err| compile_failure_stderr(format!("zccache: {err}")))?;
    crate::compiler::strict_paths::validate_args(&expanded_args, strict_paths_mode).map_err(
        |err| {
            let compiler = compiler_path.display().to_string();
            compile_failure_stderr(err.diagnostic(&compiler, &expanded_args))
        },
    )?;
    let dependency_mode = DependencyDiscoveryMode::from_client_env(client_env.as_deref())
        .map_err(|err| compile_failure_stderr(format!("zccache: {err}")))?;
    let worktree_root = compile_worktree_root(state, &sid, cwd, client_env.as_deref());
    let effective_args = effective_compile_args(
        expanded_args,
        compiler_path,
        cwd,
        worktree_root.as_ref(),
        client_env.as_deref(),
    );

    if let Some(response) = prepare_dylint_request(
        state,
        &sid,
        compiler_path,
        &effective_args,
        raw_args,
        cwd,
        &mut client_env,
        stdin,
    )
    .await
    {
        return Err(response);
    }
    let request_cache_key_root =
        request_key_root(compiler_path, &effective_args, worktree_root.as_ref());
    if let Some(response) = bypass_time_macro_request(
        state,
        &sid,
        compiler_path,
        &effective_args,
        raw_args,
        cwd,
        &client_env,
        stdin,
    )
    .await
    {
        return Err(response);
    }

    Ok(ParsedRequestArguments {
        sid,
        client_env,
        dependency_mode,
        worktree_root,
        effective_args,
        request_cache_key_root,
    })
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn prepare_dylint_request(
    state: &SharedState,
    sid: &SessionId,
    compiler_path: &Path,
    effective_args: &[String],
    raw_args: &[String],
    cwd: &Path,
    client_env: &mut Option<Vec<(String, String)>>,
    stdin: &[u8],
) -> Option<Response> {
    if !crate::compiler::is_dylint_driver(&compiler_path.to_string_lossy()) {
        return None;
    }

    let dylint_result = async {
        let (inner_rustc, _) = crate::compiler::dylint_inner_rustc_args(
            &compiler_path.to_string_lossy(),
            effective_args,
        )
        .map_err(str::to_string)?
        .ok_or_else(|| "Dylint nested invocation was not recognized".to_string())?;
        let inner_rustc = {
            let path = Path::new(inner_rustc);
            if path.is_absolute() {
                path.to_path_buf()
            } else {
                cwd.join(path)
            }
        };
        let driver_identity = state
            .compiler_hash_cache
            .get_or_hash_with_async(compiler_path, {
                let probe_env = client_env.clone().unwrap_or_default();
                |path| hash_dylint_driver_identity_async(path, probe_env)
            })
            .await
            .ok_or_else(|| format!("cannot identify Dylint driver {}", compiler_path.display()))?;
        let inner_rustc_identity = state
            .compiler_hash_cache
            .get_or_hash_rustc_identity_async(&inner_rustc)
            .await
            .ok_or_else(|| {
                format!(
                    "cannot identify Dylint inner rustc {}",
                    inner_rustc.display()
                )
            })?;
        let env = client_env.get_or_insert_with(Vec::new);
        crate::compiler::prepare_dylint_cache_env_with_identities(
            &NormalizedPath::new(compiler_path),
            effective_args,
            cwd,
            env,
            driver_identity,
            inner_rustc_identity,
            |path| {
                state
                    .compiler_hash_cache
                    .get_or_hash_with(path, |path| crate::hash::hash_file(path).ok())
                    .ok_or_else(|| {
                        format!(
                            "cannot hash Dylint library {}; running uncached",
                            path.display()
                        )
                    })
            },
        )
    }
    .await;

    let reason = match dylint_result {
        Ok(_) => {
            if let Some(env) = client_env.as_deref() {
                for (name, value) in env.iter().filter(|(name, _)| {
                    crate::compiler::dylint_env_affects_output(name)
                        || matches!(name.as_str(), "DYLINT_LIBS" | "ZCCACHE_WORKTREE_ROOT")
                }) {
                    record_dylint_input_hash(name, value.as_bytes());
                }
            }
            return None;
        }
        Err(reason) => reason,
    };
    let diagnostic = format!("zccache: Dylint cache disabled: {reason}\n");
    let bypass_compiler: NormalizedPath = compiler_path.into();
    state.stats.record_compilation();
    state.stats.record_non_cacheable();
    record_session_stat(&state.sessions, sid, |tracker| {
        tracker.record_non_cacheable()
    });
    write_session_log(
        &state.sessions,
        sid,
        &format!("non-cacheable: Dylint cache disabled: {reason}"),
    );
    let response = run_compiler_direct(
        state,
        &bypass_compiler,
        raw_args,
        cwd,
        &state.sessions,
        sid,
        client_env,
        stdin,
        state.depfile_tmpdir.as_path(),
    )
    .await;
    Some(prepend_compile_stderr(response, diagnostic.as_bytes()))
}

/// Bypass every cache layer when a C/C++ source uses a time-dependent macro.
#[allow(clippy::too_many_arguments)]
pub(super) async fn bypass_time_macro_request(
    state: &SharedState,
    sid: &SessionId,
    compiler_path: &Path,
    effective_args: &[String],
    raw_args: &[String],
    cwd: &Path,
    client_env: &Option<Vec<(String, String)>>,
    stdin: &[u8],
) -> Option<Response> {
    let parsed =
        crate::compiler::parse_invocation(compiler_path.to_str().unwrap_or(""), effective_args);
    let found = match &parsed {
        crate::compiler::ParsedInvocation::Cacheable(compilation) => {
            find_time_macro_use(compilation, cwd)
        }
        crate::compiler::ParsedInvocation::MultiFile { compilations, .. } => compilations
            .iter()
            .find_map(|compilation| find_time_macro_use(compilation, cwd)),
        crate::compiler::ParsedInvocation::NonCacheable { .. } => None,
    }?;

    let bypass_compiler: NormalizedPath = compiler_path.into();
    state.stats.record_compilation();
    state.stats.record_non_cacheable();
    record_session_stat(&state.sessions, sid, |tracker| {
        tracker.record_non_cacheable()
    });
    write_session_log(
        &state.sessions,
        sid,
        &format!(
            "non-cacheable: {} in {}",
            found.macro_name,
            found.input_file.display()
        ),
    );
    warn_time_macro_uncacheable(&found);
    Some(
        run_compiler_direct(
            state,
            &bypass_compiler,
            raw_args,
            cwd,
            &state.sessions,
            sid,
            client_env,
            stdin,
            state.depfile_tmpdir.as_path(),
        )
        .await,
    )
}

/// Discover system include roots, converting an ambiguous empty C/C++ probe
/// into an uncached compile response.
#[allow(clippy::too_many_arguments)]
pub(super) async fn discover_request_system_includes(
    state: &SharedState,
    sid: &SessionId,
    compiler: &NormalizedPath,
    lineage: &crate::daemon::lineage::Lineage,
    want_rust_miss_profile: bool,
    raw_args: &[String],
    cwd: &Path,
    client_env: &Option<Vec<(String, String)>>,
    stdin: &[u8],
) -> Result<SystemIncludesOutcome, Response> {
    let compiler_priority = CompilePriority::from_client_env(client_env.as_deref());
    let outcome = discover_system_includes(
        state,
        compiler,
        lineage,
        compiler_priority,
        want_rust_miss_profile,
    )
    .await;
    if !outcome.empty_discovery {
        return Ok(outcome);
    }

    // Issue #1167: an empty successful C/C++ probe is ambiguous, not proof
    // that the compiler has no default includes. Run directly and re-probe.
    state.stats.record_compilation();
    state.stats.record_non_cacheable();
    record_session_stat(&state.sessions, sid, |tracker| {
        tracker.record_non_cacheable()
    });
    write_session_log(
        &state.sessions,
        sid,
        "non-cacheable: system include discovery returned zero paths",
    );
    Err(run_compiler_direct(
        state,
        compiler,
        raw_args,
        cwd,
        &state.sessions,
        sid,
        client_env,
        stdin,
        state.depfile_tmpdir.as_path(),
    )
    .await)
}

pub(super) struct ParsedSingleRequest {
    pub(super) compilation: crate::compiler::CacheableCompilation,
    pub(super) cwd_path: NormalizedPath,
    pub(super) source_path: NormalizedPath,
    pub(super) output_path: NormalizedPath,
    pub(super) system_includes: Vec<NormalizedPath>,
    pub(super) client_env: Option<Vec<(String, String)>>,
    pub(super) stdin: Vec<u8>,
    pub(super) parse_args_ns: u64,
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn parse_single_compile_request(
    state_arc: &Arc<SharedState>,
    state: &SharedState,
    sid: &SessionId,
    compiler: &NormalizedPath,
    effective_args: &[String],
    raw_args: &[String],
    cwd: &Path,
    worktree_root: Option<&NormalizedPath>,
    system_includes: Vec<NormalizedPath>,
    client_env: Option<Vec<(String, String)>>,
    stdin: Vec<u8>,
    compile_start: std::time::Instant,
    dependency_mode: DependencyDiscoveryMode,
) -> Result<ParsedSingleRequest, Response> {
    let t0 = std::time::Instant::now();
    let compiler_str = compiler.to_str().unwrap_or("");
    let parsed = crate::compiler::parse_invocation(compiler_str, effective_args);
    let compilation = match parsed {
        crate::compiler::ParsedInvocation::Cacheable(compilation) => compilation,
        crate::compiler::ParsedInvocation::NonCacheable { reason } => {
            state.stats.record_non_cacheable();
            record_session_stat(&state.sessions, sid, |tracker| {
                tracker.record_non_cacheable()
            });
            write_session_log(&state.sessions, sid, &format!("non-cacheable: {reason}"));
            return Err(run_compiler_direct(
                state,
                compiler,
                raw_args,
                cwd,
                &state.sessions,
                sid,
                &client_env,
                &stdin,
                state.depfile_tmpdir.as_path(),
            )
            .await);
        }
        crate::compiler::ParsedInvocation::MultiFile {
            compilations,
            original_args,
            source_indices,
        } => {
            wait_for_startup_depgraph_load(state, sid).await;
            return Err(handle_compile_multi(
                Arc::clone(state_arc),
                *sid,
                compiler.clone(),
                compilations,
                original_args,
                source_indices,
                cwd.into(),
                worktree_root.cloned(),
                system_includes,
                client_env,
                stdin,
                compile_start,
                dependency_mode,
            )
            .await);
        }
    };
    let parse_args_ns = t0.elapsed().as_nanos() as u64;

    let cwd_path: NormalizedPath = cwd.into();
    let source_path = if compilation.source_file.is_absolute() {
        compilation.source_file.clone()
    } else {
        cwd_path.join(&compilation.source_file)
    };
    let output_path = if compilation.output_file.is_absolute() {
        compilation.output_file.clone()
    } else {
        cwd_path.join(&compilation.output_file)
    };
    if compilation.family == crate::compiler::CompilerFamily::Rustc {
        let rustc_args = crate::depgraph::parse_rustc_args(
            crate::compiler::dylint_inner_rustc_args(compiler_str, effective_args)
                .ok()
                .flatten()
                .map_or(effective_args, |(_, inner)| inner),
            cwd,
        );
        if rustc_args.crate_types == ["cdylib"]
            && !dylint_cdylib_has_complete_output_identity(
                &rustc_args,
                output_path.as_path(),
                cwd,
                client_env.as_deref(),
            )
        {
            state.stats.record_non_cacheable();
            record_session_stat(&state.sessions, sid, |tracker| {
                tracker.record_non_cacheable()
            });
            write_session_log(
                &state.sessions,
                sid,
                "non-cacheable: Dylint cdylib output identity is incomplete",
            );
            return Err(run_compiler_direct(
                state,
                compiler,
                raw_args,
                cwd,
                &state.sessions,
                sid,
                &client_env,
                &stdin,
                state.depfile_tmpdir.as_path(),
            )
            .await);
        }
    }

    Ok(ParsedSingleRequest {
        compilation,
        cwd_path,
        source_path,
        output_path,
        system_includes,
        client_env,
        stdin,
        parse_args_ns,
    })
}

pub(super) fn record_dylint_input_hash(name: &str, value: &[u8]) {
    let value_hash = crate::hash::hash_bytes(value).to_hex();
    super::super::inner_trace::record_ns(
        &format!(
            "dylint_input_{}_{}",
            name.to_ascii_lowercase(),
            &value_hash[..12]
        ),
        0,
    );
}

fn prepend_compile_stderr(mut response: Response, diagnostic: &[u8]) -> Response {
    if let Response::CompileResult { stderr, .. } = &mut response {
        let existing = std::mem::take(Arc::make_mut(stderr));
        let mut combined = Vec::with_capacity(diagnostic.len() + existing.len());
        combined.extend_from_slice(diagnostic);
        combined.extend(existing);
        *Arc::make_mut(stderr) = combined;
    }
    response
}

pub(super) async fn wait_for_startup_depgraph_load(state: &SharedState, sid: &SessionId) {
    if state.dep_graph_load_complete.load(Ordering::Acquire) {
        return;
    }

    write_session_log(
        &state.sessions,
        sid,
        "[DIAG] depgraph_load_pending: waiting before compile context registration",
    );

    let deadline = tokio::time::sleep(DEPGRAPH_STARTUP_WAIT_TIMEOUT);
    tokio::pin!(deadline);
    loop {
        let notified = state.dep_graph_load_notify.notified();
        if state.dep_graph_load_complete.load(Ordering::Acquire) {
            return;
        }
        tokio::select! {
            () = notified => {
                if state.dep_graph_load_complete.load(Ordering::Acquire) {
                    return;
                }
            }
            () = &mut deadline => {
                write_session_log(
                    &state.sessions,
                    sid,
                    "[WARN] depgraph_load_pending: timed out; continuing with current graph",
                );
                return;
            }
        }
    }
}

pub(super) fn invalidate_missing_depgraph_artifact(
    state: &SharedState,
    sid: &SessionId,
    artifact_key_hex: &str,
    _evidence: &CacheBlobMissing,
) {
    let mut stale_keys = std::collections::HashSet::with_capacity(1);
    stale_keys.insert(artifact_key_hex.to_string());
    let cleared = state.dep_graph.load().invalidate_artifact_keys(&stale_keys);
    write_session_log(
        &state.sessions,
        sid,
        &format!("[DIAG] depgraph_invalidate_artifact: key={artifact_key_hex} cleared={cleared}"),
    );
}
