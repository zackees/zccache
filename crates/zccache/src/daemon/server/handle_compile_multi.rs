//! Multi-file compile handler: handle_compile_multi, check_unit_cache, PendingWrite, UnitCacheResult.

use super::*;

/// A deferred output write for a cache hit.
pub(super) struct PendingWrite {
    out_path: NormalizedPath,
    cache_file: NormalizedPath,
    data: Vec<u8>,
}

/// Result of a per-unit cache check in multi-file compile.
pub(super) enum UnitCacheResult {
    /// Cache hit — output write is deferred for batching.
    Hit {
        stdout: Arc<Vec<u8>>,
        stderr: Arc<Vec<u8>>,
        artifact_bytes: u64,
        source_path: NormalizedPath,
        pending_writes: Vec<PendingWrite>,
    },
    /// Cache miss — needs compilation.
    Miss {
        source_path: NormalizedPath,
        output_path: NormalizedPath,
        context_key: ContextKey,
        ctx: Box<CompileContext>,
    },
}

/// Check cache for a single compilation unit. Returns Hit (output written) or Miss.
///
/// If `shared_base` is provided, the CompileContext is built by cloning it and
/// overriding the source_file, avoiding redundant arg parsing for multi-file
/// compilations where all units share the same flags.
pub(super) fn check_unit_cache(
    state: &SharedState,
    compilation: &crate::compiler::CacheableCompilation,
    cwd_path: &Path,
    key_root: &NormalizedPath,
    system_includes: &[NormalizedPath],
    shared_base: Option<&CompileContext>,
    cache_now: Instant,
) -> UnitCacheResult {
    let t0 = std::time::Instant::now();
    let snap_clock = state.cache_system.current_clock();
    state.stats.record_compilation();

    let source_path = if compilation.source_file.is_absolute() {
        compilation.source_file.clone()
    } else {
        cwd_path.join(&compilation.source_file).into()
    };
    let output_path = if compilation.output_file.is_absolute() {
        compilation.output_file.clone()
    } else {
        cwd_path.join(&compilation.output_file).into()
    };

    let (ctx, _dep_flags) = if let Some(base) = shared_base {
        let mut ctx = base.clone();
        ctx.source_file = source_path.clone();
        (
            ctx,
            UserDepFlags {
                has_md: false,
                mf_path: None,
            },
        )
    } else {
        match build_compile_context(
            compilation,
            cwd_path,
            system_includes,
            &[],
            &state.compiler_hash_cache,
        ) {
            BuildContextResult::Cc { ctx, dep_flags } => (ctx, dep_flags),
            BuildContextResult::Rustc { compat_ctx, .. } => (compat_ctx, UserDepFlags::default()),
        }
    };
    let t_ctx = t0.elapsed();
    // Issue #474: PCH / MSVC compiles take a per-worktree salt so the
    // resulting cache entry can't be cross-served between sibling
    // worktrees. Mirror of the single-file gate in
    // `pipeline.rs::handle_compile_request`.
    let source_mode_for_key = if matches!(
        compilation
            .output_file
            .as_path()
            .extension()
            .and_then(|e| e.to_str()),
        Some("pch") | Some("gch")
    ) {
        crate::compiler::SourceMode::Header
    } else {
        crate::compiler::SourceMode::Normal
    };
    let worktree_salt = if requires_worktree_in_key(compilation.family, source_mode_for_key) {
        Some(key_root.as_path())
    } else {
        None
    };
    let context_key = state.dep_graph.load().register_with_root_and_salt(
        ctx.clone(),
        Some(key_root.clone()),
        worktree_salt,
    );
    let t_register = t0.elapsed();

    // ── Ultra-fast path: per-file freshness skip ────────────────────
    // If the watcher is active and none of the source/header files have
    // changed since the last verified hit, skip ALL hash/depgraph work.
    if state.watcher_active.load(Ordering::Acquire) {
        if let Some(entry) = state.fast_hit_cache.get(&context_key) {
            if cache_entry_fresh_at(cache_now, entry.cached_at, FAST_HIT_MAX_AGE)
                && context_files_fresh(state, &context_key, &source_path, entry.clock)
            {
                let artifact_key_hex = &entry.artifact_key_hex;
                // Write outputs directly from DashMap reference — eliminates
                // cloning all .o data (~50-200KB per file) into PendingWrite.
                // Each check_unit_cache runs in its own spawn_blocking task,
                // so writes are already parallel across units.
                if let Some(mut cached_ref) =
                    lookup_artifact_with_disk_fallback(state, artifact_key_hex)
                {
                    cached_ref.last_used = std::time::Instant::now();
                    let loaded =
                        ensure_payloads(&mut cached_ref, &state.artifact_dir, artifact_key_hex)
                            .is_some();
                    if loaded {
                        #[expect(
                            clippy::expect_used,
                            reason = "ensure_payloads on the preceding line returned Some, which is the contract guaranteeing cached_ref.payloads is now populated"
                        )]
                        let payloads = Arc::clone(
                            cached_ref
                                .payloads
                                .as_ref()
                                .expect("ensure_payloads above returned without error"),
                        );
                        let names = Arc::clone(&cached_ref.meta.output_names);
                        let artifact_bytes: u64 = cached_ref.meta.total_size;
                        let stdout = cached_ref.stdout.clone();
                        let stderr = cached_ref.stderr.clone();
                        drop(cached_ref);

                        let targets: Vec<(NormalizedPath, NormalizedPath)> = (0..payloads.len())
                            .map(|i| {
                                let out: NormalizedPath = if i == 0 {
                                    output_path.clone()
                                } else {
                                    cwd_path.join(&names[i]).into()
                                };
                                let cache_file =
                                    state.artifact_dir.join(format!("{artifact_key_hex}_{i}"));
                                (out, cache_file)
                            })
                            .collect();
                        let _ = write_payloads_par(&targets, &payloads);

                        state.stats.record_hit(0, artifact_bytes);
                        state.profiler.record_hit(&HitPhases {
                            parse_args_ns: 0,
                            build_context_ns: t_ctx.as_nanos() as u64,
                            hash_source_ns: 0,
                            hash_headers_ns: 0,
                            depgraph_check_ns: 0,
                            request_cache_lookup_ns: 0,
                            cross_root_validate_ns: 0,
                            artifact_lookup_ns: 0,
                            write_output_ns: 0,
                            bookkeeping_ns: 0,
                            total_ns: t0.elapsed().as_nanos() as u64,
                        });
                        return UnitCacheResult::Hit {
                            stdout,
                            stderr,
                            artifact_bytes,
                            source_path,
                            pending_writes: Vec::new(),
                        };
                    }
                }
            }
        }
    }

    // Hash source
    let source_hash = match hash_file(&state.cache_system, &source_path, snap_clock) {
        Ok(h) => h,
        Err(_) => {
            return UnitCacheResult::Miss {
                source_path,
                output_path,
                context_key,
                ctx: Box::new(ctx),
            };
        }
    };
    let t_hash_source = t0.elapsed();

    // Hash known headers + force-includes in parallel
    let mut hash_map: HashMap<NormalizedPath, ContentHash> = HashMap::new();
    hash_map.insert(source_path.clone(), source_hash);
    {
        use rayon::prelude::*;
        let includes = state.dep_graph.load().get_includes(&context_key);
        let include_iter = includes.iter().flat_map(|v| v.iter());
        let all_paths: Vec<&NormalizedPath> =
            include_iter.chain(ctx.force_includes.iter()).collect();
        let hashes: Vec<_> = all_paths
            .par_iter()
            .filter_map(|path| {
                hash_file(&state.cache_system, path, snap_clock)
                    .ok()
                    .map(|h| ((*path).clone(), h))
            })
            .collect();
        for (path, h) in hashes {
            hash_map.insert(path, h);
        }
    }
    let t_hash_headers = t0.elapsed();

    // Depgraph check
    let verdict = {
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
            .load()
            .check(&context_key, is_fresh, get_hash)
    };
    let t_depgraph = t0.elapsed();

    // Try to serve from cache
    let depgraph_claimed_hit = matches!(verdict, crate::depgraph::CacheVerdict::Hit { .. });
    if let crate::depgraph::CacheVerdict::Hit { artifact_key }
    | crate::depgraph::CacheVerdict::SourceChanged { artifact_key } = verdict
    {
        let artifact_key_hex = artifact_key.hash().to_hex();
        if let Some(mut cached_ref) = lookup_artifact_with_disk_fallback(state, &artifact_key_hex) {
            cached_ref.last_used = std::time::Instant::now();
            let t_lookup = t0.elapsed();
            let loaded =
                ensure_payloads(&mut cached_ref, &state.artifact_dir, &artifact_key_hex).is_some();
            if loaded {
                #[expect(
                    clippy::expect_used,
                    reason = "ensure_payloads on the preceding line returned Some, which is the contract guaranteeing cached_ref.payloads is now populated"
                )]
                let payloads = Arc::clone(
                    cached_ref
                        .payloads
                        .as_ref()
                        .expect("ensure_payloads above returned without error"),
                );
                let names = Arc::clone(&cached_ref.meta.output_names);
                let artifact_bytes: u64 = cached_ref.meta.total_size;
                let stdout = cached_ref.stdout.clone();
                let stderr = cached_ref.stderr.clone();
                drop(cached_ref);

                let targets: Vec<(NormalizedPath, NormalizedPath)> = (0..payloads.len())
                    .map(|i| {
                        let out: NormalizedPath = if i == 0 {
                            output_path.clone()
                        } else {
                            cwd_path.join(&names[i]).into()
                        };
                        let cache_file = state.artifact_dir.join(format!("{artifact_key_hex}_{i}"));
                        (out, cache_file)
                    })
                    .collect();
                let _ = write_payloads_par(&targets, &payloads);

                state.stats.record_hit(0, artifact_bytes);

                // Populate fast-hit cache for future requests
                let tracked_paths =
                    request_cache_input_paths(state, &context_key, &source_path, &ctx);
                state.cache_system.register_tracked(&tracked_paths);
                let current_clock = state.cache_system.current_clock();
                state.fast_hit_cache.insert(
                    context_key,
                    FastHitEntry {
                        clock: current_clock,
                        artifact_key_hex: artifact_key_hex.clone(),
                        cached_at: std::time::Instant::now(),
                    },
                );

                let total_ns = t0.elapsed().as_nanos() as u64;
                state.profiler.record_hit(&HitPhases {
                    parse_args_ns: 0,
                    build_context_ns: t_ctx.as_nanos() as u64,
                    hash_source_ns: (t_hash_source - t_register).as_nanos() as u64,
                    hash_headers_ns: (t_hash_headers - t_hash_source).as_nanos() as u64,
                    depgraph_check_ns: (t_depgraph - t_hash_headers).as_nanos() as u64,
                    request_cache_lookup_ns: 0,
                    cross_root_validate_ns: 0,
                    artifact_lookup_ns: (t_lookup - t_depgraph).as_nanos() as u64,
                    write_output_ns: 0,
                    bookkeeping_ns: 0,
                    total_ns,
                });

                return UnitCacheResult::Hit {
                    stdout,
                    stderr,
                    artifact_bytes,
                    source_path,
                    pending_writes: Vec::new(),
                };
            }
        }
        if depgraph_claimed_hit {
            let evicted: std::collections::HashSet<String> =
                std::iter::once(artifact_key_hex).collect();
            state.dep_graph.load().invalidate_artifact_keys(&evicted);
        }
    }

    state.fast_hit_cache.remove(&context_key);
    UnitCacheResult::Miss {
        source_path,
        output_path,
        context_key,
        ctx: Box::new(ctx),
    }
}

/// Handle a multi-file compile: check cache per-unit in parallel, serve hits, batch misses.
#[allow(clippy::too_many_arguments)]
pub(super) async fn handle_compile_multi(
    state: Arc<SharedState>,
    sid: SessionId,
    compiler: NormalizedPath,
    compilations: Vec<crate::compiler::CacheableCompilation>,
    original_args: Arc<[String]>,
    source_indices: Vec<usize>,
    cwd_path: NormalizedPath,
    worktree_root: Option<NormalizedPath>,
    system_includes: Vec<NormalizedPath>,
    client_env: Option<Vec<(String, String)>>,
    compile_start: Instant,
) -> Response {
    let snap_clock = state.cache_system.current_clock();
    let mut all_stdout = Vec::new();
    let mut all_stderr = Vec::new();
    let key_root = worktree_root.as_ref().unwrap_or(&cwd_path).clone();

    // ── Pre-parse shared args once for all units ─────────────────────
    // All units share the same original_args (via Arc) — only source/output
    // differ. Parse the flags once and reuse the base CompileContext, avoiding
    // redundant arg parsing for each of the N compilation units.
    let shared_base: Arc<CompileContext> = {
        let first = &compilations[0];
        let parsed = match first.family {
            crate::compiler::CompilerFamily::Msvc => {
                crate::depgraph::msvc_args::parse_msvc_args(&first.original_args, &cwd_path)
            }
            _ => crate::depgraph::args::parse_gnu_args(&first.original_args, &cwd_path),
        };
        let mut base = CompileContext::from_parsed_args(parsed);
        for path in &system_includes {
            if !base.include_search.system.contains(path) {
                base.include_search.system.push(path.clone());
            }
        }
        Arc::new(base)
    };

    // ── Phase 1: Check cache for each unit (parallel, as-completed) ──
    let mut join_set = tokio::task::JoinSet::new();
    for (idx, compilation) in compilations.iter().enumerate() {
        let state = Arc::clone(&state);
        let cwd_path = cwd_path.clone();
        let key_root = key_root.clone();
        let system_includes = system_includes.clone();
        let compilation = compilation.clone();
        let shared_base = Arc::clone(&shared_base);
        let cache_now = compile_start;
        join_set.spawn_blocking(move || {
            (
                idx,
                check_unit_cache(
                    &state,
                    &compilation,
                    &cwd_path,
                    &key_root,
                    &system_includes,
                    Some(&shared_base),
                    cache_now,
                ),
            )
        });
    }

    // Collect results in original order
    let mut indexed_results: Vec<(usize, UnitCacheResult)> = Vec::with_capacity(compilations.len());
    while let Some(result) = join_set.join_next().await {
        match result {
            Ok(pair) => indexed_results.push(pair),
            Err(e) => {
                return Response::Error {
                    message: format!("cache check task panicked: {e}"),
                };
            }
        }
    }
    indexed_results.sort_by_key(|(idx, _)| *idx);

    let mut unit_results: Vec<UnitCacheResult> = Vec::with_capacity(indexed_results.len());
    let mut all_pending_writes: Vec<PendingWrite> = Vec::new();
    for (_, mut result) in indexed_results {
        match &result {
            UnitCacheResult::Hit {
                stdout,
                stderr,
                artifact_bytes,
                source_path,
                ..
            } => {
                all_stdout.extend_from_slice(stdout);
                all_stderr.extend_from_slice(stderr);
                let src = source_path.clone();
                let bytes = *artifact_bytes;
                record_session_stat(&state.sessions, &sid, move |t| {
                    t.record_hit(src, 0, bytes);
                });
            }
            UnitCacheResult::Miss { source_path, .. } => {
                write_session_log(
                    &state.sessions,
                    &sid,
                    &format!("multi-file cache miss: {}", source_path.display()),
                );
            }
        }
        // Drain pending writes from hits for batched parallel execution
        if let UnitCacheResult::Hit {
            ref mut pending_writes,
            ..
        } = result
        {
            all_pending_writes.append(pending_writes);
        }
        unit_results.push(result);
    }

    // ── Phase 1b: Execute all output writes in parallel ─────────────
    if !all_pending_writes.is_empty() {
        let mut write_set = tokio::task::JoinSet::new();
        for pw in all_pending_writes {
            write_set.spawn_blocking(move || {
                let _ = write_cached_output(&pw.out_path, &pw.cache_file, &pw.data);
            });
        }
        while write_set.join_next().await.is_some() {}
    }

    // For cache HIT outputs: downgrade metadata without advancing clock
    // (same artifact content). For cache MISS outputs: apply_changes is
    // done later after real compilation. This preserves fast-hit cache
    // validity for unrelated source files.
    {
        let mut output_dirs = HashSet::new();
        for (idx, comp) in compilations.iter().enumerate() {
            let out = if comp.output_file.is_absolute() {
                comp.output_file.clone()
            } else {
                cwd_path.join(&comp.output_file)
            };
            if let Some(parent) = out.parent() {
                output_dirs.insert(parent.into());
            }
            if matches!(&unit_results[idx], UnitCacheResult::Hit { .. }) {
                state.cache_system.metadata().downgrade(&out);
            }
        }
        let dirs: Vec<NormalizedPath> = output_dirs.into_iter().collect();
        watch_directories(&state, &dirs).await;
    }

    let miss_sources: Vec<&NormalizedPath> = unit_results
        .iter()
        .filter_map(|r| match r {
            UnitCacheResult::Miss { source_path, .. } => Some(source_path),
            UnitCacheResult::Hit { .. } => None,
        })
        .collect();

    if miss_sources.is_empty() {
        return Response::CompileResult {
            exit_code: 0,
            stdout: Arc::new(all_stdout),
            stderr: Arc::new(all_stderr),
            cached: true,
        };
    }

    write_session_log(
        &state.sessions,
        &sid,
        &format!(
            "multi-file: compiling {} of {} files",
            miss_sources.len(),
            compilations.len()
        ),
    );

    // Build compiler args from original_args, removing hit source files by index.
    // This preserves all original flags (including unknown ones) exactly as passed.
    let supports_depfile = compilations[0].family.supports_depfile();
    let hit_indices: HashSet<usize> = {
        let miss_set: HashSet<&NormalizedPath> = miss_sources.iter().copied().collect();
        source_indices
            .iter()
            .enumerate()
            .filter_map(|(si_pos, &arg_idx)| {
                let comp = &compilations[si_pos];
                let abs_src = if comp.source_file.is_absolute() {
                    comp.source_file.clone()
                } else {
                    cwd_path.join(&comp.source_file)
                };
                if !miss_set.contains(&abs_src) {
                    Some(arg_idx)
                } else {
                    None
                }
            })
            .collect()
    };
    let mut compiler_args: Vec<String> = original_args
        .iter()
        .enumerate()
        .filter(|(i, _)| !hit_indices.contains(i))
        .map(|(_, a)| a.clone())
        .collect();
    if supports_depfile {
        compiler_args.push("-MD".to_string());
    }

    let _rsp_guard = match crate::compiler::response_file::write_response_file_if_needed(
        &compiler_args,
        &state.depfile_tmpdir,
        compilations[0].family,
    ) {
        Ok(guard) => guard,
        Err(e) => {
            return Response::Error {
                message: format!("failed to write response file: {e}"),
            };
        }
    };

    for unit in &unit_results {
        if let UnitCacheResult::Miss { output_path, .. } = unit {
            if let Err(e) = break_output_hardlink_before_compile(output_path) {
                return Response::Error {
                    message: format!(
                        "failed to detach hardlinked output before compile {}: {e}",
                        output_path.display()
                    ),
                };
            }
        }
    }

    let lineage = super::super::lineage::Lineage::current(
        session_client_pid(&state, &sid),
        Some(sid.to_string()),
    );
    let mut cmd = tokio::process::Command::new(&compiler);
    if let Some(ref rsp) = _rsp_guard {
        cmd.arg(rsp.at_arg()).current_dir(&cwd_path);
    } else {
        cmd.args(&compiler_args).current_dir(&cwd_path);
    }
    apply_client_env(&mut cmd, &client_env, &lineage);
    let compiler_priority = CompilePriority::from_client_env(client_env.as_deref());
    let result =
        super::super::process::tokio_command_output_with_priority(&mut cmd, compiler_priority)
            .await;

    let output = match result {
        Ok(o) => o,
        Err(e) => {
            return Response::Error {
                message: format!("failed to run compiler: {e}"),
            };
        }
    };

    let exit_code = output.status.code().unwrap_or(-1);
    all_stdout.extend_from_slice(&output.stdout);
    all_stderr.extend_from_slice(&output.stderr);

    if exit_code != 0 {
        state.stats.record_error();
        record_session_stat(&state.sessions, &sid, |t| t.record_error());
        return Response::CompileResult {
            exit_code,
            stdout: Arc::new(all_stdout),
            stderr: Arc::new(all_stderr),
            cached: false,
        };
    }

    // ── Phase 3: Cache each miss result in parallel ──────────────────
    //
    // Each miss requires: read .o, parse depfile, hash deps (rayon), update
    // dep_graph, build CachedArtifact, insert into DashMaps. For a 50-file
    // batch the sequential version dominated wall time (~12ms × 50 = 600ms).
    // We fan out the per-miss work onto `spawn_blocking` and batch the async
    // sync points (watch_directories, apply_changes) at the end.
    struct MissOutcome {
        dep_dirs: Vec<NormalizedPath>,
        output_path: NormalizedPath,
        persist: Option<PersistTaskParams>,
    }
    struct PersistTaskParams {
        artifact_key_hex: String,
        persist_meta: ArtifactIndex,
        payloads: Vec<Arc<Vec<u8>>>,
        payload_size: usize,
    }

    let mut miss_set: tokio::task::JoinSet<MissOutcome> = tokio::task::JoinSet::new();
    for unit in &unit_results {
        let (source_path, output_path, context_key, ctx) = match unit {
            UnitCacheResult::Miss {
                source_path,
                output_path,
                context_key,
                ctx,
            } => (
                source_path.clone(),
                output_path.clone(),
                *context_key,
                ctx.clone(),
            ),
            UnitCacheResult::Hit { .. } => continue,
        };

        let state_task = Arc::clone(&state);
        let cwd_path_task = cwd_path.clone();
        let sid_task = sid;
        miss_set.spawn_blocking(move || {
            // The compiler just wrote `output_path`; we only need its size for
            // bookkeeping (artifact-bytes stats, ArtifactIndex.output_sizes).
            // The bytes themselves stay on disk — we persist via hardlink
            // below, so reading them into RAM would waste a memcpy and double
            // the Defender write-scan budget. `unwrap_or(0)` matches the old
            // `unwrap_or_default()` semantics for the size-fetch on a missing
            // output path: caller treats that as a non-cacheable result.
            let output_size = std::fs::metadata(&output_path)
                .map(|m| m.len())
                .unwrap_or(0);

            // Scan includes: use depfile if available, fall back to scanner.
            let scan_result = if supports_depfile {
                let d_path = source_path.with_extension("d");
                // Multi-file -MD places .d files relative to the source
                let cwd_d_path = cwd_path_task.join(
                    d_path
                        .file_name()
                        .unwrap_or_else(|| std::ffi::OsStr::new("deps.d")),
                );
                let depfile_path: NormalizedPath = if d_path.exists() {
                    d_path.into()
                } else if cwd_d_path.exists() {
                    cwd_d_path
                } else {
                    let stem = source_path
                        .file_stem()
                        .unwrap_or_else(|| std::ffi::OsStr::new("out"));
                    cwd_path_task.join(stem).with_extension("d").into()
                };
                match crate::depgraph::depfile::parse_depfile_path(
                    &depfile_path,
                    &source_path,
                    &cwd_path_task,
                ) {
                    Ok(result) => {
                        let _ = std::fs::remove_file(&depfile_path);
                        result
                    }
                    Err(e) => {
                        tracing::warn!(
                            "multi-file depfile parse failed for {}: {e}",
                            source_path.display()
                        );
                        crate::depgraph::scanner::scan_recursive(&source_path, &ctx.include_search)
                    }
                }
            } else {
                crate::depgraph::scanner::scan_recursive(&source_path, &ctx.include_search)
            };

            let tracked_paths: Vec<NormalizedPath> = std::iter::once(source_path.clone())
                .chain(scan_result.resolved.iter().cloned())
                .collect();
            state_task.cache_system.register_tracked(&tracked_paths);

            // Collect parent dirs for the batched watch_directories call.
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
                dirs.into_iter().collect()
            };

            // Hash all files (source + headers) in parallel via rayon.
            let hash_map: HashMap<NormalizedPath, ContentHash> = {
                use rayon::prelude::*;
                let all_paths: Vec<&NormalizedPath> = std::iter::once(&source_path)
                    .chain(scan_result.resolved.iter())
                    .collect();
                all_paths
                    .par_iter()
                    .filter_map(|path| {
                        hash_file(&state_task.cache_system, path, snap_clock)
                            .ok()
                            .map(|h| ((*path).clone(), h))
                    })
                    .collect()
            };

            let get_hash = |p: &Path| {
                let path = NormalizedPath::new(p);
                hash_map.get(&path).copied()
            };
            let update_result =
                state_task
                    .dep_graph
                    .load()
                    .update(&context_key, scan_result, get_hash);

            if let Some(artifact_key) = update_result {
                let output_name = output_path
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned();
                let artifact_key_hex = artifact_key.hash().to_hex();
                let artifact_bytes: u64 = output_size;

                // Persist via hardlink synchronously in this miss task: cheap
                // (~tens of µs on NTFS), keeps the cache file alive at the
                // exact path future hits will look it up at, and eliminates
                // the second `std::fs::write` Defender used to scan on the
                // background persist path. If the hardlink fails (cross-volume
                // fallback to copy also fails, missing source, etc.) we skip
                // caching this artifact rather than risk inserting a state
                // entry whose cache file doesn't exist — same end-user
                // observation as the prior "background persist failed
                // silently" path, just visible immediately.
                if let Err(e) = persist_artifact_paths(
                    state_task.artifact_dir.as_path(),
                    &artifact_key_hex,
                    std::slice::from_ref(&output_path),
                ) {
                    tracing::warn!(
                        key = %artifact_key_hex,
                        output = %output_path.display(),
                        "failed to persist artifact via hardlink: {e}"
                    );
                    return MissOutcome {
                        dep_dirs,
                        output_path,
                        persist: None,
                    };
                }
                let cache_file_path = state_task
                    .artifact_dir
                    .join(format!("{artifact_key_hex}_0"));

                // Build the cached artifact directly (avoid constructing the
                // full ArtifactData wrapper just to compute the same fields).
                // Payloads point at the *cache* file (the just-hardlinked
                // copy), not the original output path: cargo may rewrite
                // the output on the next build via tmp+rename, which would
                // detach the old inode from the user-visible path while the
                // cache-side hardlink keeps it alive — the cache copy is the
                // stable reference.
                let empty = Arc::new(Vec::new());
                let meta = ArtifactIndex::new(
                    vec![output_name],
                    vec![artifact_bytes],
                    Arc::clone(&empty),
                    Arc::clone(&empty),
                    0,
                );
                let cached = CachedArtifact {
                    meta: meta.clone(),
                    stdout: Arc::clone(&empty),
                    stderr: Arc::clone(&empty),
                    payloads: Some(Arc::from(vec![CachedPayload::File(cache_file_path)])),
                    last_used: std::time::Instant::now(),
                };

                state_task
                    .artifacts
                    .insert(artifact_key_hex.clone(), cached);

                let current_clock = state_task.cache_system.current_clock();
                state_task.fast_hit_cache.insert(
                    context_key,
                    FastHitEntry {
                        clock: current_clock,
                        artifact_key_hex: artifact_key_hex.clone(),
                        cached_at: std::time::Instant::now(),
                    },
                );

                state_task.stats.record_miss(0, artifact_bytes);
                let src = source_path.clone();
                record_session_stat(&state_task.sessions, &sid_task, move |t| {
                    t.record_miss(src, artifact_bytes);
                });

                // Files are already on disk via the hardlink above. The
                // remaining work is the redb index entry, which goes through
                // the same background WAL the byte-write path used.
                let _ = state_task
                    .index_writer_tx
                    .send(IndexWriterCommand::Insert(artifact_key_hex, meta));
            }
            // No PersistTaskParams: persistence is complete synchronously.
            let persist: Option<PersistTaskParams> = None;

            MissOutcome {
                dep_dirs,
                output_path,
                persist,
            }
        });
    }

    // Collect outcomes and batch the async sync points.
    let mut all_dep_dirs: HashSet<NormalizedPath> = HashSet::new();
    let mut all_miss_outputs: Vec<NormalizedPath> = Vec::new();
    let mut persist_jobs: Vec<PersistTaskParams> = Vec::new();
    while let Some(joined) = miss_set.join_next().await {
        let outcome = match joined {
            Ok(o) => o,
            Err(e) => {
                tracing::error!("multi-file miss task panicked: {e}");
                continue;
            }
        };
        for d in outcome.dep_dirs {
            all_dep_dirs.insert(d);
        }
        all_miss_outputs.push(outcome.output_path);
        if let Some(p) = outcome.persist {
            persist_jobs.push(p);
        }
    }

    // Single batched watch_directories call (was 1 per miss).
    let dep_dirs_vec: Vec<NormalizedPath> = all_dep_dirs.into_iter().collect();
    watch_directories(&state, &dep_dirs_vec).await;

    // Spawn the artifact-persist tasks now that locks are released.
    for job in persist_jobs {
        let artifact_dir = state.artifact_dir.clone();
        let key_hex = job.artifact_key_hex;
        let persist_meta = job.persist_meta;
        let payloads = job.payloads;
        let payload_size = job.payload_size;
        state
            .in_flight_bytes
            .fetch_add(payload_size, Ordering::Relaxed);
        let guard = InFlightGuard {
            state: Arc::clone(&state),
            size: payload_size,
        };
        let sem = Arc::clone(&state.persist_semaphore);
        let state_ref = Arc::clone(&state);
        // Issue #728: capture the per-job enqueue Instant so the WARN below
        // can report `gap_ms` = "linker-success-recorded → persist-attempt-
        // started" (distinguishes queue starvation under burst load from
        // src/dst failures already enriched by `persist::enrich_persist_err`).
        let t_persist_enqueue = std::time::Instant::now();
        tokio::spawn(async move {
            #[expect(
                clippy::expect_used,
                reason = "persist_semaphore is owned by ServerState for the daemon's lifetime; AcquireError here would be a logic bug (semaphore explicitly closed), not a runtime condition"
            )]
            let _permit = sem
                .acquire()
                .await
                .expect("persist_semaphore is owned by ServerState and never closed");
            let written = tokio::task::spawn_blocking(move || {
                let _guard = guard;
                let gap_ms = t_persist_enqueue.elapsed().as_millis() as u64;
                if let Err(e) = persist_artifact_payloads(&artifact_dir, &key_hex, &payloads) {
                    tracing::warn!(
                        key = %key_hex,
                        gap_ms,
                        "failed to persist artifact output: {e}"
                    );
                }
                (key_hex, persist_meta)
            })
            .await;
            if let Ok((key_hex, meta)) = written {
                let _ = state_ref
                    .index_writer_tx
                    .send(IndexWriterCommand::Insert(key_hex, meta));
            }
        });
    }

    // Single batched apply_changes call (was 1 per miss): all miss outputs
    // have new content; advance the clock once for downstream consumers.
    if !all_miss_outputs.is_empty() {
        state.cache_system.apply_changes(all_miss_outputs);
    }

    Response::CompileResult {
        exit_code: 0,
        stdout: Arc::new(all_stdout),
        stderr: Arc::new(all_stderr),
        cached: false,
    }
}
