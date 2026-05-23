//! Single-file compile handler: handle_compile + compile_failure_stderr.

use super::*;

pub(super) fn compile_failure_stderr(message: String) -> Response {
    let mut stderr = message.into_bytes();
    stderr.push(b'\n');
    Response::CompileResult {
        exit_code: 1,
        stdout: Arc::new(Vec::new()),
        stderr: Arc::new(stderr),
        cached: false,
    }
}

/// Handle a Compile request: parse args, check depgraph, run compiler or return cached.
#[allow(clippy::too_many_arguments)] // Hot dispatch path; refactor parked.
pub(super) async fn handle_compile(
    state_arc: &Arc<SharedState>,
    session_id: &str,
    args: &[String],
    cwd: &Path,
    compiler_path: &Path,
    client_env: Option<Vec<(String, String)>>,
    stdin: Vec<u8>,
) -> Response {
    let state = state_arc.as_ref();
    let compile_start = std::time::Instant::now();
    let sid = match session_id.parse::<SessionId>() {
        Ok(id) => id,
        Err(_) => {
            return Response::Error {
                message: format!("invalid session ID: {session_id}"),
            };
        }
    };
    // Expand response files before request-level caching so `@file` mutations
    // can't reuse stale fast-hit entries keyed only by raw argv.
    let expanded_args = expand_args_cached(state, args, cwd);

    let strict_paths_mode = match strict_paths_mode_from_client_env(client_env.as_deref()) {
        Ok(mode) => mode,
        Err(err) => return compile_failure_stderr(format!("zccache: {err}")),
    };
    if let Err(err) =
        crate::compiler::strict_paths::validate_args(&expanded_args, strict_paths_mode)
    {
        let compiler = compiler_path.display().to_string();
        return compile_failure_stderr(err.diagnostic(&compiler, &expanded_args));
    }

    let worktree_root = compile_worktree_root(state, &sid, cwd, client_env.as_deref());
    let effective_args = effective_compile_args(
        &expanded_args,
        compiler_path,
        cwd,
        worktree_root.as_ref(),
        client_env.as_deref(),
    );
    let request_cache_key_root =
        request_key_root(compiler_path, &effective_args, worktree_root.as_ref());

    // Snap the journal clock once so all file hashes in this request see a
    // consistent view (avoids per-file current_clock() syscalls).
    let snap_clock = state.cache_system.current_clock();

    // ── Ultra-fast request-level cache ────────────────────────────────
    // If we've seen this exact (compiler, args, cwd) before AND the fast-hit
    // cache still holds a valid entry, skip ALL heavy work: system include
    // discovery, watch_directories, response file expansion, arg parsing,
    // context building, and dep_graph registration.
    if state.watcher_active.load(Ordering::Acquire) {
        let t_request_cache_lookup = std::time::Instant::now();
        let request_fp = request_fingerprint(
            compiler_path,
            &effective_args,
            cwd,
            request_cache_key_root.as_deref(),
            client_env.as_deref(),
        );
        if let Some(req_entry) = state.request_cache.get(&request_fp) {
            let request_cache_lookup_ns = t_request_cache_lookup.elapsed().as_nanos() as u64;
            if request_cache_entry_matches_root(&req_entry, request_cache_key_root.as_ref()) {
                if let Some(fh_entry) = state.fast_hit_cache.get(&req_entry.context_key) {
                    let artifact_key_hex = &fh_entry.artifact_key_hex;
                    let source_path = req_entry
                        .source_path
                        .resolve(request_cache_key_root.as_deref());
                    let output_path = req_entry
                        .output_path
                        .resolve(request_cache_key_root.as_deref());
                    let same_root = req_entry.root.as_ref() == request_cache_key_root.as_ref();
                    let t_cross_root_validate = std::time::Instant::now();
                    let inputs_match = if same_root {
                        context_files_fresh(
                            state,
                            &req_entry.context_key,
                            &source_path,
                            fh_entry.clock,
                        )
                    } else {
                        request_cache_artifact_matches(
                            state,
                            &req_entry,
                            request_fp,
                            request_cache_key_root.as_ref(),
                            artifact_key_hex,
                            compile_start,
                            snap_clock,
                        )
                    };
                    let cross_root_validate_ns = if same_root {
                        0
                    } else {
                        t_cross_root_validate.elapsed().as_nanos() as u64
                    };
                    if cache_entry_fresh_at(compile_start, fh_entry.cached_at, FAST_HIT_MAX_AGE)
                        && cache_entry_fresh_at(
                            compile_start,
                            req_entry.cached_at,
                            EPHEMERAL_CACHE_MAX_AGE,
                        )
                        && inputs_match
                    {
                        let t_artifact_lookup = std::time::Instant::now();
                        if let Some(mut cached_ref) =
                            lookup_artifact_with_disk_fallback(state, artifact_key_hex)
                        {
                            cached_ref.last_used = std::time::Instant::now();
                            let loaded = ensure_payloads(
                                &mut cached_ref,
                                &state.artifact_dir,
                                artifact_key_hex,
                            )
                            .is_some();
                            if loaded {
                                let artifact_lookup_ns =
                                    t_artifact_lookup.elapsed().as_nanos() as u64;
                                let payloads = Arc::clone(cached_ref.payloads.as_ref().unwrap());
                                let names = Arc::clone(&cached_ref.meta.output_names);
                                let exit_code = cached_ref.meta.exit_code;
                                let stdout = cached_ref.stdout.clone();
                                let stderr = cached_ref.stderr.clone();
                                let artifact_bytes: u64 = cached_ref.meta.total_size;
                                // Drop the DashMap reference before doing more work
                                drop(cached_ref);

                                // Write output
                                let t_write_output = std::time::Instant::now();
                                let secondary_dir =
                                    output_path.parent().unwrap_or(cwd).to_path_buf();
                                let targets: Vec<(NormalizedPath, NormalizedPath)> = (0..payloads
                                    .len())
                                    .map(|i| {
                                        let out: NormalizedPath = if i == 0 {
                                            output_path.clone()
                                        } else {
                                            secondary_dir.join(&names[i]).into()
                                        };
                                        let cache_file = state
                                            .artifact_dir
                                            .join(format!("{artifact_key_hex}_{i}"));
                                        (out, cache_file)
                                    })
                                    .collect();
                                let write_ok = write_payloads_par(&targets, &payloads);
                                if write_ok {
                                    let write_output_ns =
                                        t_write_output.elapsed().as_nanos() as u64;
                                    let t_bookkeeping = std::time::Instant::now();
                                    state.stats.record_compilation();
                                    let latency_ns = compile_start.elapsed().as_nanos() as u64;
                                    state.stats.record_hit(latency_ns, artifact_bytes);
                                    let src = source_path.clone();
                                    record_session_stat(&state.sessions, &sid, move |t| {
                                        t.record_hit(src, latency_ns, artifact_bytes);
                                    });
                                    write_session_log(
                                        &state.sessions,
                                        &sid,
                                        &format!(
                                            "[{}] {} -> {}",
                                            if same_root {
                                                "HIT_REQUEST"
                                            } else {
                                                "HIT_WORKTREE_REQUEST"
                                            },
                                            source_path.display(),
                                            output_path.display()
                                        ),
                                    );
                                    let bookkeeping_ns = t_bookkeeping.elapsed().as_nanos() as u64;
                                    let total_ns = compile_start.elapsed().as_nanos() as u64;
                                    state.profiler.record_hit(&HitPhases {
                                        parse_args_ns: 0,
                                        build_context_ns: 0,
                                        hash_source_ns: 0,
                                        hash_headers_ns: 0,
                                        depgraph_check_ns: 0,
                                        request_cache_lookup_ns,
                                        cross_root_validate_ns,
                                        artifact_lookup_ns,
                                        write_output_ns,
                                        bookkeeping_ns,
                                        total_ns,
                                    });

                                    return Response::CompileResult {
                                        exit_code,
                                        stdout,
                                        stderr,
                                        cached: true,
                                    };
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    state.stats.record_compilation();

    // Note: we do not require `state.sessions.exists(&sid)` here. A daemon
    // restart (e.g. zccache-ci killing the daemon to unlock target binaries
    // on Windows) drops the session map but client wrappers keep using the
    // session UUID they were issued. The session-stat and touch helpers
    // below already no-op for unknown sessions, so the compile itself
    // proceeds; only per-session stats are lost. Mirrors PR #137's
    // idempotent SessionEnd fix. See issues #166 and #167.

    let compiler: NormalizedPath = compiler_path.into();

    // Lineage carried into every child spawned for this compile request —
    // compiler, depfile probe, etc. See `super::super::lineage` and issue #7.
    let lineage =
        super::super::lineage::Lineage::current(session_client_pid(state, &sid), Some(session_id.into()));

    // Discover system includes for this compiler (cached per compiler path)
    let t_system_includes = std::time::Instant::now();
    let compiler_priority = CompilePriority::from_client_env(client_env.as_deref());
    let system_includes = {
        let mut cache = state.system_includes.lock().await;
        let lineage_for_probe = lineage.clone();
        cache
            .get_or_discover(&compiler, |c| {
                let disc_args = crate::depgraph::discovery_args();
                let output = {
                    let mut cmd = std::process::Command::new(c);
                    cmd.args(&disc_args);
                    lineage_for_probe.apply_to_sync(&mut cmd, None);
                    super::super::process::command_output_with_priority(&mut cmd, compiler_priority)
                };
                match output {
                    Ok(out) => {
                        let stderr = String::from_utf8_lossy(&out.stderr);
                        crate::depgraph::parse_system_include_output(&stderr)
                    }
                    Err(e) => {
                        tracing::warn!("failed to run compiler for include discovery: {e}");
                        Vec::new()
                    }
                }
            })
            .to_vec()
    };
    let system_includes_ns = t_system_includes.elapsed().as_nanos() as u64;

    // Watch system include directories
    let t_system_watch = std::time::Instant::now();
    watch_directories(state, &system_includes).await;
    let system_watch_ns = t_system_watch.elapsed().as_nanos() as u64;

    state.sessions.touch(&sid);

    // ── Phase: expand response files + parse args ─────────────────────
    let t0 = std::time::Instant::now();
    let compiler_str = compiler.to_str().unwrap_or("");
    let parsed = crate::compiler::parse_invocation(compiler_str, &effective_args);
    let compilation = match parsed {
        crate::compiler::ParsedInvocation::Cacheable(c) => c,
        crate::compiler::ParsedInvocation::NonCacheable { reason } => {
            state.stats.record_non_cacheable();
            record_session_stat(&state.sessions, &sid, |t| t.record_non_cacheable());
            write_session_log(&state.sessions, &sid, &format!("non-cacheable: {reason}"));
            // Use raw args — compiler handles @file natively
            return run_compiler_direct(
                &compiler,
                args,
                cwd,
                &state.sessions,
                &sid,
                &client_env,
                &stdin,
                state.depfile_tmpdir.as_path(),
            )
            .await;
        }
        crate::compiler::ParsedInvocation::MultiFile {
            compilations,
            original_args,
            source_indices,
        } => {
            return handle_compile_multi(
                Arc::clone(state_arc),
                sid,
                compiler,
                compilations,
                original_args,
                source_indices,
                cwd.into(),
                worktree_root.clone(),
                system_includes,
                client_env,
                compile_start,
            )
            .await;
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

    // ── Phase: build context + register ──────────────────────────────
    let t1 = std::time::Instant::now();
    let env_slice = client_env.as_deref().unwrap_or(&[]);
    let build_result = build_compile_context(
        &compilation,
        &cwd_path,
        &system_includes,
        env_slice,
        &state.compiler_hash_cache,
    );
    let default_key_root = worktree_root.clone().unwrap_or_else(|| cwd_path.clone());
    let (ctx, dep_flags, rustc_args_opt, context_key, worktree_equivalent_context) =
        match build_result {
            BuildContextResult::Cc { ctx, dep_flags } => {
                let registration = state
                    .dep_graph
                    .register_with_root_result(ctx.clone(), Some(default_key_root.clone()));
                (
                    ctx,
                    dep_flags,
                    None,
                    registration.key,
                    registration.rebased_from_equivalent_root,
                )
            }
            BuildContextResult::Rustc {
                rustc_ctx,
                compat_ctx,
                rustc_args,
            } => {
                let remap_gate =
                    rust_remap_gate(&rustc_args.remap_path_prefixes, worktree_root.as_ref());
                write_session_log(
                    &state.sessions,
                    &sid,
                    &format!("[DIAG] {}", remap_gate.as_str()),
                );
                let rustc_key_root =
                    rustc_context_key_root(&rustc_args.remap_path_prefixes, worktree_root.as_ref());
                let key = rustc_ctx.context_key_with_root(rustc_key_root.as_deref());
                let registration = state.dep_graph.register_with_key_and_root_result(
                    key,
                    compat_ctx.clone(),
                    rustc_key_root.clone(),
                );
                (
                    compat_ctx,
                    UserDepFlags::default(),
                    Some(rustc_args),
                    registration.key,
                    registration.rebased_from_equivalent_root,
                )
            }
        };
    let is_rustc = rustc_args_opt.is_some();
    let rust_profile_enabled = is_rustc && std::env::var_os(RUST_MISS_PROFILE_ENV).is_some();
    let rust_profile_mode = rustc_args_opt
        .as_ref()
        .map(|rustc_args| {
            if rustc_args.emit_types.iter().any(|emit| emit == "link") {
                "build"
            } else {
                "check"
            }
        })
        .unwrap_or("other");
    let build_context_ns = t1.elapsed().as_nanos() as u64;

    // ── Ultra-fast path: per-file freshness skip ────────────────────
    // If the watcher is active and none of the source/header files have
    // changed since the last verified hit, skip ALL hash/depgraph work.
    // Uses per-file journal checks instead of global clock comparison so
    // output file writes don't invalidate unrelated fast-hit entries.
    if state.watcher_active.load(Ordering::Acquire) {
        if let Some(entry) = state.fast_hit_cache.get(&context_key) {
            if cache_entry_fresh_at(compile_start, entry.cached_at, FAST_HIT_MAX_AGE)
                && context_files_fresh(state, &context_key, &source_path, entry.clock)
            {
                let artifact_key_hex = &entry.artifact_key_hex;
                let t5 = std::time::Instant::now();
                // Write directly from DashMap reference — avoids cloning the
                // entire CachedArtifact (including all .o data, ~50-200KB).
                if let Some(mut cached_ref) =
                    lookup_artifact_with_disk_fallback(state, artifact_key_hex)
                {
                    cached_ref.last_used = std::time::Instant::now();
                    let artifact_lookup_ns = t5.elapsed().as_nanos() as u64;
                    let t6 = std::time::Instant::now();
                    let loaded =
                        ensure_payloads(&mut cached_ref, &state.artifact_dir, artifact_key_hex)
                            .is_some();
                    if !loaded {
                        // Fall through to slow path on payload load failure
                    } else {
                        let payloads = Arc::clone(cached_ref.payloads.as_ref().unwrap());
                        let names = Arc::clone(&cached_ref.meta.output_names);
                        let exit_code = cached_ref.meta.exit_code;
                        let stdout = cached_ref.stdout.clone();
                        let stderr = cached_ref.stderr.clone();
                        let artifact_bytes: u64 = cached_ref.meta.total_size;
                        // Drop the DashMap reference before doing more work
                        drop(cached_ref);

                        let secondary_dir = if is_rustc {
                            output_path.parent().unwrap_or(&cwd_path).to_path_buf()
                        } else {
                            cwd_path.clone().to_path_buf()
                        };
                        let targets: Vec<(NormalizedPath, NormalizedPath)> = (0..payloads.len())
                            .map(|i| {
                                let out: NormalizedPath = if i == 0 {
                                    output_path.clone()
                                } else {
                                    secondary_dir.join(&names[i]).into()
                                };
                                let cache_file =
                                    state.artifact_dir.join(format!("{artifact_key_hex}_{i}"));
                                (out, cache_file)
                            })
                            .collect();
                        let write_ok = write_payloads_par(&targets, &payloads);
                        if !write_ok {
                            // Fall through to slow path on write failure
                        } else {
                            let write_output_ns = t6.elapsed().as_nanos() as u64;

                            // Downgrade output metadata (file was re-written) but
                            // DON'T advance the journal clock — the output content is
                            // the same cached artifact, and advancing the global clock
                            // would invalidate fast-hit entries for unrelated source
                            // files in the same batch.
                            state.cache_system.metadata().downgrade(&output_path);

                            let t7 = std::time::Instant::now();
                            let latency_ns = compile_start.elapsed().as_nanos() as u64;
                            state.stats.record_hit(latency_ns, artifact_bytes);
                            let src = source_path.clone();
                            record_session_stat(&state.sessions, &sid, move |t| {
                                t.record_hit(src, latency_ns, artifact_bytes);
                            });
                            write_session_log(
                                &state.sessions,
                                &sid,
                                &format!(
                                    "[{}] {} -> {}",
                                    if worktree_equivalent_context {
                                        "HIT_WORKTREE_FAST"
                                    } else {
                                        "HIT_FAST"
                                    },
                                    source_path.display(),
                                    output_path.display()
                                ),
                            );
                            let bookkeeping_ns = t7.elapsed().as_nanos() as u64;

                            let rfp = request_fingerprint(
                                compiler_path,
                                &effective_args,
                                cwd,
                                request_cache_key_root.as_deref(),
                                client_env.as_deref(),
                            );
                            let input_paths =
                                request_cache_input_paths(state, &context_key, &source_path, &ctx);
                            state.request_cache.insert(
                                rfp,
                                request_cache_entry(
                                    context_key,
                                    &source_path,
                                    &output_path,
                                    input_paths,
                                    request_cache_key_root.as_ref(),
                                ),
                            );

                            let total_ns = compile_start.elapsed().as_nanos() as u64;
                            state.profiler.record_hit(&HitPhases {
                                parse_args_ns,
                                build_context_ns,
                                hash_source_ns: 0,
                                hash_headers_ns: 0,
                                depgraph_check_ns: 0,
                                request_cache_lookup_ns: 0,
                                cross_root_validate_ns: 0,
                                artifact_lookup_ns,
                                write_output_ns,
                                bookkeeping_ns,
                                total_ns,
                            });

                            return Response::CompileResult {
                                exit_code,
                                stdout,
                                stderr,
                                cached: true,
                            };
                        }
                    }
                }
            }
        }
    }

    // ── Slow path: hash + depgraph verify ────────────────────────────

    // Skip pre-compile hashing for cold contexts — the depgraph would
    // return Cold without examining any hashes, so the work is wasted.
    // Jump straight to compiler exec.
    let context_is_cold = state.dep_graph.is_cold(&context_key);

    // ── Phase: hash source ───────────────────────────────────────────
    let t2 = std::time::Instant::now();
    let mut hash_map: HashMap<NormalizedPath, ContentHash> = HashMap::new();
    if !context_is_cold {
        match hash_file(&state.cache_system, &source_path, snap_clock) {
            Ok(h) => {
                hash_map.insert(source_path.clone(), h);
            }
            Err(e) => {
                write_session_log(
                    &state.sessions,
                    &sid,
                    &format!("cache key error: {e}, falling back to direct compile"),
                );
                return run_compiler_direct(
                    &compiler,
                    args,
                    cwd,
                    &state.sessions,
                    &sid,
                    &client_env,
                    &stdin,
                    state.depfile_tmpdir.as_path(),
                )
                .await;
            }
        }
    }
    let hash_source_ns = t2.elapsed().as_nanos() as u64;

    // ── Phase: hash headers + depgraph check ────────────────────────
    let t3 = std::time::Instant::now();
    let hash_headers_ns;
    let depgraph_check_ns;
    let verdict;
    let diag_reason;

    if context_is_cold {
        // Cold context — skip hashing and depgraph check entirely.
        hash_headers_ns = 0;
        depgraph_check_ns = 0;
        verdict = crate::depgraph::CacheVerdict::Cold;
        diag_reason = "cold_skip".to_string();
    } else {
        // Hash includes + force-includes in parallel (PCH-aware).
        {
            use rayon::prelude::*;
            let includes = state.dep_graph.get_includes(&context_key);
            let include_iter = includes
                .iter()
                .flat_map(|v| v.iter().map(|h| (h, "header_hash_fail")));
            let force_iter = ctx
                .force_includes
                .iter()
                .map(|h| (h, "force_include_hash_fail"));
            let all_paths: Vec<_> = include_iter.chain(force_iter).collect();

            let results: Vec<_> = all_paths
                .par_iter()
                .map(|(header, label)| {
                    let hash_path = resolve_pch_source(header, &state.pch_source_map)
                        .unwrap_or_else(|| (*header).clone());
                    let result = hash_file(&state.cache_system, &hash_path, snap_clock);
                    ((*header).clone(), hash_path, result, *label)
                })
                .collect();

            for (header, hash_path, result, label) in results {
                match result {
                    Ok(h) => {
                        hash_map.insert(header, h);
                    }
                    Err(e) => {
                        write_session_log(
                            &state.sessions,
                            &sid,
                            &format!("[DIAG] {label}: {} error={e}", hash_path.display()),
                        );
                    }
                }
            }
        }
        hash_headers_ns = t3.elapsed().as_nanos() as u64;

        // ── Phase: depgraph check ────────────────────────────────────
        // Fast path: recompute artifact key from fresh hashes and compare
        // with the stored key.  Skips redundant journal freshness checks
        // and path clones that check_diagnostic performs.
        if let Some(artifact_key) = state.dep_graph.try_fast_hit(&context_key, |p| {
            let path = NormalizedPath::new(p);
            hash_map.get(&path).copied()
        }) {
            depgraph_check_ns = 0;
            verdict = crate::depgraph::CacheVerdict::Hit { artifact_key };
            diag_reason = "fast_key_match".to_string();
        } else {
            let t4 = std::time::Instant::now();
            let result = {
                let is_fresh = |p: &Path| {
                    let path = NormalizedPath::new(p);
                    !state
                        .cache_system
                        .journal()
                        .changed_since(&path, snap_clock)
                };
                let get_hash = |p: &Path| {
                    let path = NormalizedPath::new(p);
                    hash_map.get(&path).copied()
                };
                state
                    .dep_graph
                    .check_diagnostic(&context_key, is_fresh, get_hash)
            };
            depgraph_check_ns = t4.elapsed().as_nanos() as u64;
            verdict = result.0;
            diag_reason = result.1;
        }
    }

    write_session_log(
        &state.sessions,
        &sid,
        &format!(
            "[DIAG] depgraph_check: {} -> {} ctx={} verdict={} reason={}",
            source_path.display(),
            output_path.display(),
            &context_key.hash().to_hex()[..8],
            match &verdict {
                crate::depgraph::CacheVerdict::Hit { .. } => "Hit",
                crate::depgraph::CacheVerdict::SourceChanged { .. } => "SourceChanged",
                crate::depgraph::CacheVerdict::HeadersChanged { .. } => "HeadersChanged",
                crate::depgraph::CacheVerdict::Cold => "Cold",
                crate::depgraph::CacheVerdict::NeedsPreprocessor => "NeedsPreprocessor",
            },
            diag_reason,
        ),
    );
    match verdict {
        crate::depgraph::CacheVerdict::Hit { artifact_key }
        | crate::depgraph::CacheVerdict::SourceChanged { artifact_key } => {
            // ── Phase: artifact lookup + write ─────────────────────────
            let t5 = std::time::Instant::now();
            let artifact_key_hex = artifact_key.hash().to_hex();
            if let Some(mut cached_ref) =
                lookup_artifact_with_disk_fallback(state, &artifact_key_hex)
            {
                cached_ref.last_used = std::time::Instant::now();
                let artifact_lookup_ns = t5.elapsed().as_nanos() as u64;

                let t6 = std::time::Instant::now();
                let loaded =
                    ensure_payloads(&mut cached_ref, &state.artifact_dir, &artifact_key_hex)
                        .is_some();
                if !loaded {
                    // Fall through to compile on payload load failure
                } else {
                    let payloads = Arc::clone(cached_ref.payloads.as_ref().unwrap());
                    let names = Arc::clone(&cached_ref.meta.output_names);
                    let exit_code = cached_ref.meta.exit_code;
                    let stdout = cached_ref.stdout.clone();
                    let stderr = cached_ref.stderr.clone();
                    let artifact_bytes: u64 = cached_ref.meta.total_size;
                    drop(cached_ref);

                    let secondary_dir = if is_rustc {
                        output_path.parent().unwrap_or(&cwd_path).to_path_buf()
                    } else {
                        cwd_path.clone().to_path_buf()
                    };
                    let targets: Vec<(NormalizedPath, NormalizedPath)> = (0..payloads.len())
                        .map(|i| {
                            let out: NormalizedPath = if i == 0 {
                                output_path.clone()
                            } else {
                                secondary_dir.join(&names[i]).into()
                            };
                            let cache_file =
                                state.artifact_dir.join(format!("{artifact_key_hex}_{i}"));
                            (out, cache_file)
                        })
                        .collect();
                    let write_ok = write_payloads_par(&targets, &payloads);
                    if !write_ok {
                        // Fall through to compile on write failure
                    } else {
                        let write_output_ns = t6.elapsed().as_nanos() as u64;

                        // Downgrade output metadata but don't advance journal
                        // clock — same cached artifact content, advancing would
                        // invalidate fast-hit entries for other source files.
                        state.cache_system.metadata().downgrade(&output_path);

                        // ── Phase: bookkeeping ───────────────────────────────
                        let t7 = std::time::Instant::now();
                        let latency_ns = compile_start.elapsed().as_nanos() as u64;
                        state.stats.record_hit(latency_ns, artifact_bytes);
                        let src = source_path.clone();
                        record_session_stat(&state.sessions, &sid, move |t| {
                            t.record_hit(src, latency_ns, artifact_bytes);
                        });
                        write_session_log(
                            &state.sessions,
                            &sid,
                            &format!(
                                "[{}] {} -> {}",
                                if worktree_equivalent_context {
                                    "HIT_WORKTREE"
                                } else {
                                    "HIT"
                                },
                                source_path.display(),
                                output_path.display()
                            ),
                        );
                        let bookkeeping_ns = t7.elapsed().as_nanos() as u64;

                        // Populate fast-hit cache for future requests
                        let input_paths =
                            request_cache_input_paths(state, &context_key, &source_path, &ctx);
                        state.cache_system.register_tracked(&input_paths);
                        let current_clock = state.cache_system.current_clock();
                        state.fast_hit_cache.insert(
                            context_key,
                            FastHitEntry {
                                clock: current_clock,
                                artifact_key_hex: artifact_key_hex.clone(),
                                cached_at: std::time::Instant::now(),
                            },
                        );

                        let rfp = request_fingerprint(
                            compiler_path,
                            &effective_args,
                            cwd,
                            request_cache_key_root.as_deref(),
                            client_env.as_deref(),
                        );
                        state.request_cache.insert(
                            rfp,
                            request_cache_entry(
                                context_key,
                                &source_path,
                                &output_path,
                                input_paths,
                                request_cache_key_root.as_ref(),
                            ),
                        );

                        // Record phase profile
                        let total_ns = compile_start.elapsed().as_nanos() as u64;
                        state.profiler.record_hit(&HitPhases {
                            parse_args_ns,
                            build_context_ns,
                            hash_source_ns,
                            hash_headers_ns,
                            depgraph_check_ns,
                            request_cache_lookup_ns: 0,
                            cross_root_validate_ns: 0,
                            artifact_lookup_ns,
                            write_output_ns,
                            bookkeeping_ns,
                            total_ns,
                        });

                        return Response::CompileResult {
                            exit_code,
                            stdout,
                            stderr,
                            cached: true,
                        };
                    }
                }
            }
            // Artifact key computed but no artifact stored yet — fall through to compile
            write_session_log(
                &state.sessions,
                &sid,
                &format!("[DIAG] artifact_not_found: key={artifact_key_hex}"),
            );
        }
        crate::depgraph::CacheVerdict::Cold
        | crate::depgraph::CacheVerdict::HeadersChanged { .. }
        | crate::depgraph::CacheVerdict::NeedsPreprocessor => {
            // Need to compile and scan includes
        }
    }

    // Cache miss — invalidate fast-hit cache for this context
    state.fast_hit_cache.remove(&context_key);

    // Cache miss — run the compiler
    write_session_log(
        &state.sessions,
        &sid,
        &format!(
            "[MISS] {} -> {} (reason: {diag_reason})",
            source_path.display(),
            output_path.display()
        ),
    );

    // ── Phase: compiler exec (with depfile injection) ────────────────
    let pre_exec_ns = compile_start.elapsed().as_nanos() as u64;
    let t_exec = std::time::Instant::now();
    let supports_depfile = compilation.family.supports_depfile();
    let (mut extra_args, mut depfile_strategy) = crate::depgraph::depfile::prepare_depfile(
        supports_depfile,
        &dep_flags,
        &output_path,
        &state.depfile_tmpdir,
    );

    // For MSVC, use /showIncludes to get complete dependency info
    // (equivalent to depfiles for gcc/clang). This enables cache hits
    // for files with computed includes like `#include MACRO`.
    if compilation.family == crate::compiler::CompilerFamily::Msvc
        && depfile_strategy == DepfileStrategy::Unsupported
    {
        if !dep_flags.has_md {
            extra_args.push("/showIncludes".to_string());
        }
        depfile_strategy = DepfileStrategy::ShowIncludes;
    }

    // Combine expanded_args + extra_args for response-file length check.
    // Only allocates when extra_args is non-empty.
    let combined_args;
    let rsp_args: &[String] = if extra_args.is_empty() {
        &effective_args
    } else {
        combined_args = [effective_args.as_slice(), extra_args.as_slice()].concat();
        &combined_args
    };

    let _rsp_guard = match crate::compiler::response_file::write_response_file_if_needed(
        rsp_args,
        &state.depfile_tmpdir,
    ) {
        Ok(guard) => guard,
        Err(e) => {
            return Response::Error {
                message: format!("failed to write response file: {e}"),
            };
        }
    };

    let output_paths = if let Some(rustc_args) = rustc_args_opt.as_ref() {
        rustc_expected_output_paths(rustc_args, &output_path, &cwd_path)
    } else {
        vec![output_path.clone()]
    };
    let t_break_outputs = std::time::Instant::now();
    for path in &output_paths {
        if let Err(e) = break_output_hardlink_before_compile(path) {
            return Response::Error {
                message: format!(
                    "failed to detach hardlinked output before compile {}: {e}",
                    path.display()
                ),
            };
        }
    }
    let break_outputs_ns = t_break_outputs.elapsed().as_nanos() as u64;

    let mut cmd = tokio::process::Command::new(&compiler);
    if let Some(ref rsp) = _rsp_guard {
        cmd.arg(rsp.at_arg()).current_dir(cwd);
    } else {
        cmd.args(&effective_args).current_dir(cwd);
        if !extra_args.is_empty() {
            cmd.args(&extra_args);
        }
    }
    apply_client_env(&mut cmd, &client_env, &lineage);
    let t_compiler_process = std::time::Instant::now();
    let is_link_like = rustc_args_opt
        .as_ref()
        .is_some_and(|rustc_args| rustc_args.emit_types.iter().any(|emit| emit == "link"));
    let compiler_priority =
        CompilePriority::from_client_env_for_link_like(client_env.as_deref(), is_link_like);
    let compiler_priority_decision = compiler_priority.resolve_for_current_load();
    let result = super::super::process::tokio_command_output_with_priority(
        &mut cmd,
        compiler_priority_decision.effective,
    )
    .await;
    let compiler_process_ns = t_compiler_process.elapsed().as_nanos() as u64;

    let output = match result {
        Ok(o) => o,
        Err(e) => {
            return Response::Error {
                message: format!("failed to run compiler: {e}"),
            };
        }
    };
    let compiler_exec_ns = t_exec.elapsed().as_nanos() as u64;
    let compiler_prep_ns = compiler_exec_ns.saturating_sub(compiler_process_ns);

    let t_post_exec = std::time::Instant::now();
    let exit_code = output.status.code().unwrap_or(-1);
    let stdout = Arc::new(output.stdout);

    // For MSVC /showIncludes: parse dependency info from stderr and
    // filter out the /showIncludes lines before returning to the client.
    let (show_includes_scan, stderr_bytes) = if depfile_strategy == DepfileStrategy::ShowIncludes {
        let (scan, filtered) = crate::depgraph::show_includes::parse_show_includes(
            &output.stderr,
            &source_path,
            &cwd_path,
        );
        (Some(scan), filtered)
    } else {
        (None, output.stderr)
    };
    let stderr = Arc::new(stderr_bytes);
    let post_exec_ns = t_post_exec.elapsed().as_nanos() as u64;

    if exit_code != 0 {
        state.stats.record_error();
        record_session_stat(&state.sessions, &sid, |t| t.record_error());
    }

    // Only cache successful compilations
    if exit_code == 0 {
        // The compiler just wrote the output file. Invalidate it in the
        // cache system so any compilation that depends on this output
        // (e.g. via -include-pch) sees the change immediately — no need
        // to wait for a watcher event.
        let t_apply_changes = std::time::Instant::now();
        state.cache_system.apply_changes(vec![output_path.clone()]);
        let apply_changes_ns = t_apply_changes.elapsed().as_nanos() as u64;

        // Capture output metadata. Rust payload bytes are snapshotted into
        // cache files after the artifact key is known, avoiding foreground
        // reads of .rlib/.rmeta/.d on cold misses.
        let t_collect_outputs = std::time::Instant::now();
        let (output_data, rustc_all_outputs) = if is_rustc {
            let all = collect_rustc_output_files(
                rustc_args_opt.as_ref().unwrap(),
                &output_path,
                &cwd_path,
            );
            if all.is_empty() {
                tracing::warn!("failed to stat output file {}", output_path.display());
                return Response::CompileResult {
                    exit_code,
                    stdout: Arc::clone(&stdout),
                    stderr: Arc::clone(&stderr),
                    cached: false,
                };
            }
            (Vec::new(), Some(all))
        } else {
            match std::fs::read(&output_path) {
                Ok(data) => (data, None),
                Err(e) => {
                    tracing::warn!("failed to read output file {}: {e}", output_path.display());
                    return Response::CompileResult {
                        exit_code,
                        stdout: Arc::clone(&stdout),
                        stderr: Arc::clone(&stderr),
                        cached: false,
                    };
                }
            }
        };
        let collect_outputs_ns = t_collect_outputs.elapsed().as_nanos() as u64;
        let rust_output_count = rustc_all_outputs.as_ref().map_or(1, Vec::len);
        let rust_output_bytes: u64 = rustc_all_outputs
            .as_ref()
            .map_or(output_data.len() as u64, |all| {
                all.iter().map(|output| output.size).sum()
            });

        // ── Phase: include scan (depfile or fallback) ────────────────
        let t_scan = std::time::Instant::now();
        let scan_result = if is_rustc {
            // Rustc: try to parse the dep-info file if --emit included dep-info.
            // The dep-info file is in --out-dir with crate name and extra-filename.
            scan_rustc_deps(rustc_args_opt.as_ref().unwrap(), &source_path, &cwd_path)
        } else {
            match &depfile_strategy {
                DepfileStrategy::Injected { path }
                | DepfileStrategy::UserSpecified { path }
                | DepfileStrategy::UserDefault { path } => {
                    let cwd_path: NormalizedPath = cwd.into();
                    match crate::depgraph::depfile::parse_depfile_path(
                        path,
                        &source_path,
                        &cwd_path,
                    ) {
                        Ok(result) => {
                            if matches!(depfile_strategy, DepfileStrategy::Injected { .. }) {
                                let _ = std::fs::remove_file(path);
                            }
                            result
                        }
                        Err(e) => {
                            tracing::warn!("depfile parse failed, falling back to scanner: {e}");
                            write_session_log(
                                &state.sessions,
                                &sid,
                                &format!(
                                    "[DIAG] depfile_parse_fail: path={} error={e}",
                                    path.display()
                                ),
                            );
                            if matches!(depfile_strategy, DepfileStrategy::Injected { .. }) {
                                let _ = std::fs::remove_file(path);
                            }
                            crate::depgraph::scanner::scan_recursive(
                                &source_path,
                                &ctx.include_search,
                            )
                        }
                    }
                }
                DepfileStrategy::ShowIncludes => {
                    // Already parsed from stderr above.
                    show_includes_scan.unwrap_or_else(|| {
                        crate::depgraph::scanner::scan_recursive(&source_path, &ctx.include_search)
                    })
                }
                DepfileStrategy::Unsupported => {
                    crate::depgraph::scanner::scan_recursive(&source_path, &ctx.include_search)
                }
            }
        };
        let include_scan_ns = t_scan.elapsed().as_nanos() as u64;

        // Register scanned paths for zero-syscall fast path on future hits.
        let tracked_paths: Vec<NormalizedPath> = std::iter::once(source_path.clone())
            .chain(scan_result.resolved.iter().cloned())
            .chain(ctx.force_includes.iter().cloned())
            .collect();
        let t_register_tracked = std::time::Instant::now();
        state.cache_system.register_tracked(&tracked_paths);
        let register_tracked_ns = t_register_tracked.elapsed().as_nanos() as u64;

        // Collect directories to watch. The actual watch_directories call
        // (which involves expensive canonicalize() on Windows) is deferred
        // to a background task to avoid blocking the response.
        let t_dep_dirs = std::time::Instant::now();
        let dep_dirs: Vec<NormalizedPath> = {
            let mut dirs = HashSet::new();
            if let Some(parent) = source_path.parent() {
                dirs.insert(parent.into());
            }
            for header in &scan_result.resolved {
                if let Some(parent) = header.parent() {
                    dirs.insert(parent.into());
                }
            }
            // Also watch force-include parent dirs (PCH files, etc.).
            for fi in &ctx.force_includes {
                if let Some(parent) = fi.parent() {
                    dirs.insert(parent.into());
                }
            }
            dirs.into_iter().collect()
        };
        let dep_dirs_ns = t_dep_dirs.elapsed().as_nanos() as u64;

        // ── Phase: hash all files (parallel) ─────────────────────────
        // Hash source + resolved headers + force-includes using rayon
        // parallel iteration, matching the hit path's parallel strategy.
        let t_hash = std::time::Instant::now();
        let mut hash_map: HashMap<NormalizedPath, ContentHash> = HashMap::new();
        {
            use rayon::prelude::*;
            let header_iter = scan_result.resolved.iter().chain(ctx.force_includes.iter());
            let all_paths: Vec<&NormalizedPath> =
                std::iter::once(&source_path).chain(header_iter).collect();

            let results: Vec<_> = all_paths
                .par_iter()
                .map(|path| {
                    let hash_path = resolve_pch_source(path, &state.pch_source_map)
                        .unwrap_or_else(|| (*path).clone());
                    let result = hash_file(&state.cache_system, &hash_path, snap_clock);
                    ((*path).clone(), result)
                })
                .collect();

            let mut hash_failures: u32 = 0;
            for (path, result) in results {
                match result {
                    Ok(h) => {
                        hash_map.insert(path, h);
                    }
                    Err(_) => {
                        hash_failures += 1;
                    }
                }
            }
            if hash_failures > 0 {
                write_session_log(
                    &state.sessions,
                    &sid,
                    &format!(
                        "[DIAG] hash_failures: {} of {} files failed to hash for {}",
                        hash_failures,
                        1 + scan_result.resolved.len() + ctx.force_includes.len(),
                        source_path.display(),
                    ),
                );
            }
        }
        let hash_all_ns = t_hash.elapsed().as_nanos() as u64;

        // ── Phase: store artifact ────────────────────────────────────
        let t_store = std::time::Instant::now();
        let get_hash = |p: &Path| {
            let path = NormalizedPath::new(p);
            hash_map.get(&path).copied()
        };
        let include_count = scan_result.resolved.len();
        let t_depgraph_update = std::time::Instant::now();
        let artifact_key_result = state.dep_graph.update(&context_key, scan_result, get_hash);
        let depgraph_update_ns = t_depgraph_update.elapsed().as_nanos() as u64;
        let mut artifact_build_ns = 0;
        let mut persist_enqueue_ns = 0;
        let mut artifact_insert_stats_ns = 0;
        let mut artifact_meta_build_ns = 0;
        let mut rust_snapshot_ns = 0;
        let mut rust_snapshot_hardlink_count = 0;
        let mut rust_snapshot_copy_count = 0;
        let mut rust_snapshot_copy_bytes = 0;
        let mut rust_snapshot_error_count = 0;
        let mut artifact_index_build_ns = 0;
        let mut artifact_index_persist_ns = 0;
        let mut artifact_memory_insert_ns = 0;
        if let Some(artifact_key) = artifact_key_result {
            let artifact_key_hex = artifact_key.hash().to_hex();
            let ctx_hex = &context_key.hash().to_hex()[..8];
            write_session_log(
                &state.sessions,
                &sid,
                &format!(
                    "[DIAG] update: {} ctx={ctx_hex} artifact_key={} includes={include_count}",
                    source_path.display(),
                    &artifact_key_hex[..8],
                ),
            );

            // Record PCH source mapping so consuming compilations can hash
            // the source header instead of the non-deterministic PCH binary.
            if let Some(ext) = output_path.extension() {
                if ext == "pch" || ext == "gch" {
                    state
                        .pch_source_map
                        .insert(output_path.clone(), source_path.clone());
                }
            }

            // Build artifact — multi-output for Rustc, single output for C/C++.
            let t_artifact_build = std::time::Instant::now();
            if let Some(ref all_outputs) = rustc_all_outputs {
                let t_artifact_meta_build = std::time::Instant::now();
                let artifact_bytes: u64 = all_outputs.iter().map(|o| o.size).sum();
                let output_names: Vec<String> =
                    all_outputs.iter().map(|o| o.name.clone()).collect();
                let output_sizes: Vec<u64> = all_outputs.iter().map(|o| o.size).collect();
                let payload_paths: Vec<NormalizedPath> = (0..all_outputs.len())
                    .map(|i| state.artifact_dir.join(format!("{artifact_key_hex}_{i}")))
                    .collect();
                artifact_meta_build_ns = t_artifact_meta_build.elapsed().as_nanos() as u64;

                let mut snapshot_ok = true;
                let t_rust_snapshot = std::time::Instant::now();
                for (output, cache_path) in all_outputs.iter().zip(payload_paths.iter()) {
                    match persist_artifact_file(cache_path, &output.path) {
                        Ok(stats) => {
                            rust_snapshot_hardlink_count += stats.hardlink_count;
                            rust_snapshot_copy_count += stats.copy_count;
                            rust_snapshot_copy_bytes += stats.copy_bytes;
                        }
                        Err(e) => {
                            rust_snapshot_error_count += 1;
                            snapshot_ok = false;
                            tracing::warn!(
                                source = %output.path.display(),
                                cache = %cache_path.display(),
                                "failed to snapshot rustc output: {e}"
                            );
                            break;
                        }
                    }
                }
                rust_snapshot_ns = t_rust_snapshot.elapsed().as_nanos() as u64;
                artifact_build_ns = t_artifact_build.elapsed().as_nanos() as u64;

                let t_artifact_insert_stats = std::time::Instant::now();
                if snapshot_ok {
                    let t_artifact_index_build = std::time::Instant::now();
                    let meta = ArtifactIndex::new(
                        output_names,
                        output_sizes,
                        Arc::clone(&stdout),
                        Arc::clone(&stderr),
                        exit_code,
                    );
                    artifact_index_build_ns = t_artifact_index_build.elapsed().as_nanos() as u64;
                    let t_artifact_index_persist = std::time::Instant::now();
                    state.artifact_store.insert(&artifact_key_hex, &meta);
                    artifact_index_persist_ns =
                        t_artifact_index_persist.elapsed().as_nanos() as u64;
                    let t_artifact_memory_insert = std::time::Instant::now();
                    let cached = CachedArtifact::from_file_payloads(meta, payload_paths);
                    state.artifacts.insert(artifact_key_hex, cached);
                    artifact_memory_insert_ns =
                        t_artifact_memory_insert.elapsed().as_nanos() as u64;
                }

                let latency_ns = compile_start.elapsed().as_nanos() as u64;
                let recorded_bytes = if snapshot_ok { artifact_bytes } else { 0 };
                state.stats.record_miss(latency_ns, recorded_bytes);
                let src = source_path.clone();
                record_session_stat(&state.sessions, &sid, move |t| {
                    t.record_miss(src, recorded_bytes);
                });
                artifact_insert_stats_ns = t_artifact_insert_stats.elapsed().as_nanos() as u64;
            } else {
                let artifact = ArtifactData {
                    outputs: vec![ArtifactOutput {
                        name: output_path
                            .file_name()
                            .unwrap_or_default()
                            .to_string_lossy()
                            .into_owned(),
                        payload: ArtifactPayload::Bytes(Arc::new(output_data)),
                    }],
                    stdout: Arc::clone(&stdout),
                    stderr: Arc::clone(&stderr),
                    exit_code,
                };

                let artifact_bytes: u64 = artifact
                    .outputs
                    .iter()
                    .map(|o| o.payload.size_bytes())
                    .sum();

                // Build CachedArtifact once (no deep copies — all Arc clones).
                let cached = CachedArtifact::from_artifact_data(&artifact);
                artifact_build_ns = t_artifact_build.elapsed().as_nanos() as u64;
                let t_persist_enqueue = std::time::Instant::now();

                // Spawn disk persistence to background (meta.clone() is cheap — Arc fields only).
                {
                    let artifact_dir = state.artifact_dir.clone();
                    let key_hex = artifact_key_hex.clone();
                    let persist_meta = cached.meta.clone();
                    // Same Bytes-only-in-practice contract as the link-ephemeral
                    // site above; match-and-materialise keeps Path-variant
                    // forward-compatibility correct.
                    let payloads: Vec<Arc<Vec<u8>>> = artifact
                        .outputs
                        .iter()
                        .filter_map(|o| match &o.payload {
                            ArtifactPayload::Bytes(b) => Some(Arc::clone(b)),
                            ArtifactPayload::Path(p) => {
                                std::fs::read(p.as_path()).ok().map(Arc::new)
                            }
                        })
                        .collect();
                    let payload_size: usize = payloads.iter().map(|p| p.len()).sum();
                    state
                        .in_flight_bytes
                        .fetch_add(payload_size, Ordering::Relaxed);
                    let guard = InFlightGuard {
                        state: Arc::clone(state_arc),
                        size: payload_size,
                    };
                    let sem = Arc::clone(&state.persist_semaphore);
                    let state_ref = Arc::clone(state_arc);
                    tokio::spawn(async move {
                        let _permit = sem.acquire().await.unwrap();
                        let written = tokio::task::spawn_blocking(move || {
                            let _guard = guard;
                            if let Err(e) =
                                persist_artifact_payloads(&artifact_dir, &key_hex, &payloads)
                            {
                                tracing::warn!(
                                    key = %key_hex,
                                    "failed to persist artifact output: {e}"
                                );
                            }
                            (key_hex, persist_meta)
                        })
                        .await;
                        if let Ok((key_hex, meta)) = written {
                            let _ = state_ref.index_writer_tx.send((key_hex, meta));
                        }
                    });
                }
                persist_enqueue_ns = t_persist_enqueue.elapsed().as_nanos() as u64;

                let t_artifact_insert_stats = std::time::Instant::now();
                state.artifacts.insert(artifact_key_hex, cached);

                let latency_ns = compile_start.elapsed().as_nanos() as u64;
                state.stats.record_miss(latency_ns, artifact_bytes);
                let src = source_path.clone();
                record_session_stat(&state.sessions, &sid, move |t| {
                    t.record_miss(src, artifact_bytes);
                });
                artifact_insert_stats_ns = t_artifact_insert_stats.elapsed().as_nanos() as u64;
            }
        }
        let artifact_store_ns = t_store.elapsed().as_nanos() as u64;

        // Record miss phase profile
        let total_ns = compile_start.elapsed().as_nanos() as u64;
        state.profiler.record_miss(&MissPhases {
            compiler_exec_ns,
            include_scan_ns,
            hash_all_ns,
            artifact_store_ns,
            total_ns,
        });

        // Defer expensive watch_directories to background — canonicalize()
        // on Windows costs ~1-5ms per directory. This doesn't affect cache
        // correctness; it only delays watcher-based invalidation setup.
        if rust_profile_enabled {
            let pre_exec_measured_ns = system_includes_ns
                .saturating_add(system_watch_ns)
                .saturating_add(parse_args_ns)
                .saturating_add(build_context_ns)
                .saturating_add(hash_source_ns)
                .saturating_add(hash_headers_ns)
                .saturating_add(depgraph_check_ns);
            let pre_exec_other_ns = pre_exec_ns.saturating_sub(pre_exec_measured_ns);
            let artifact_store_measured_ns = depgraph_update_ns
                .saturating_add(artifact_build_ns)
                .saturating_add(persist_enqueue_ns)
                .saturating_add(artifact_insert_stats_ns);
            let artifact_store_other_ns =
                artifact_store_ns.saturating_sub(artifact_store_measured_ns);
            let accounted_ns = pre_exec_ns
                .saturating_add(compiler_prep_ns)
                .saturating_add(compiler_process_ns)
                .saturating_add(post_exec_ns)
                .saturating_add(apply_changes_ns)
                .saturating_add(collect_outputs_ns)
                .saturating_add(include_scan_ns)
                .saturating_add(register_tracked_ns)
                .saturating_add(dep_dirs_ns)
                .saturating_add(hash_all_ns)
                .saturating_add(artifact_store_ns);
            let unaccounted_ns = total_ns.saturating_sub(accounted_ns);
            let compiler_cpu_usage_percent = compiler_priority_decision
                .cpu_usage_percent
                .map(|usage| format!("{usage:.1}"))
                .unwrap_or_else(|| "n/a".to_string());
            eprintln!(
                concat!(
                    "zccache_rust_miss_profile ",
                    "mode={} compiler_priority={} compiler_effective_priority={} ",
                    "compiler_cpu_usage_percent={} total_ns={} pre_exec_ns={} system_includes_ns={} ",
                    "system_watch_ns={} parse_args_ns={} build_context_ns={} ",
                    "hash_source_ns={} hash_headers_ns={} depgraph_check_ns={} ",
                    "pre_exec_other_ns={} break_outputs_ns={} compiler_prep_ns={} compiler_process_ns={} ",
                    "post_exec_ns={} apply_changes_ns={} collect_outputs_ns={} ",
                    "outputs={} output_bytes={} include_scan_ns={} ",
                    "register_tracked_ns={} dep_dirs_ns={} hash_all_ns={} ",
                    "artifact_store_ns={} depgraph_update_ns={} artifact_build_ns={} ",
                    "artifact_meta_build_ns={} rust_snapshot_ns={} ",
                    "rust_snapshot_hardlink_count={} rust_snapshot_copy_count={} ",
                    "rust_snapshot_copy_bytes={} rust_snapshot_error_count={} ",
                    "persist_enqueue_ns={} artifact_insert_stats_ns={} ",
                    "artifact_index_build_ns={} artifact_index_persist_ns={} ",
                    "artifact_memory_insert_ns={} ",
                    "artifact_store_other_ns={} unaccounted_ns={}"
                ),
                rust_profile_mode,
                compiler_priority_decision.requested.as_str(),
                compiler_priority_decision.effective.as_str(),
                compiler_cpu_usage_percent,
                total_ns,
                pre_exec_ns,
                system_includes_ns,
                system_watch_ns,
                parse_args_ns,
                build_context_ns,
                hash_source_ns,
                hash_headers_ns,
                depgraph_check_ns,
                pre_exec_other_ns,
                break_outputs_ns,
                compiler_prep_ns,
                compiler_process_ns,
                post_exec_ns,
                apply_changes_ns,
                collect_outputs_ns,
                rust_output_count,
                rust_output_bytes,
                include_scan_ns,
                register_tracked_ns,
                dep_dirs_ns,
                hash_all_ns,
                artifact_store_ns,
                depgraph_update_ns,
                artifact_build_ns,
                artifact_meta_build_ns,
                rust_snapshot_ns,
                rust_snapshot_hardlink_count,
                rust_snapshot_copy_count,
                rust_snapshot_copy_bytes,
                rust_snapshot_error_count,
                persist_enqueue_ns,
                artifact_insert_stats_ns,
                artifact_index_build_ns,
                artifact_index_persist_ns,
                artifact_memory_insert_ns,
                artifact_store_other_ns,
                unaccounted_ns,
            );
        }

        {
            let bg_state = Arc::clone(state_arc);
            tokio::spawn(async move {
                let state = &*bg_state;
                watch_directories(state, &dep_dirs).await;
                if let Some(out_dir) = output_path.parent() {
                    watch_directory(state, out_dir).await;
                }
                state.cache_system.apply_changes(vec![output_path]);
            });
        }
    }

    Response::CompileResult {
        exit_code,
        stdout,
        stderr,
        cached: false,
    }
}
