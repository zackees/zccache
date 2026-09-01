//! Multi-file compile handler: handle_compile_multi, check_unit_cache, UnitCacheResult.

use super::*;

#[path = "handle_compile_multi_args.rs"]
mod args;
#[path = "handle_compile_multi_preflight.rs"]
mod preflight;
#[path = "handle_compile_multi_staged.rs"]
mod staged;
#[path = "handle_compile_multi_types.rs"]
mod types;

use args::filter_multi_source_args;
use preflight::InputSnapshot;
pub(super) use types::materialize_multi_hit;
use types::{
    invalidate_multi_artifact_after_failure, legacy_depfile_policy, owned_fast_hit_entry,
    MissOutcome, PersistTaskParams, UnitCacheCheck, UnitCacheResult,
};

fn check_unit_cache(
    state: &SharedState,
    compilation: &crate::compiler::CacheableCompilation,
    check: UnitCacheCheck<'_>,
) -> UnitCacheResult {
    let UnitCacheCheck {
        cwd_path,
        key_root,
        system_includes,
        shared_base,
        shared_dep_flags,
        scan_cache,
        cache_now,
        dependency_mode,
    } = check;
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

    let shared_context = shared_base.is_some();
    let (mut ctx, dep_flags) = if let Some(base) = shared_base {
        let mut ctx = base.clone();
        ctx.source_file = source_path.clone();
        (ctx, shared_dep_flags.cloned().unwrap_or_default())
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
    if !shared_context {
        dependency_mode.apply_to_cc_context(&mut ctx, &dep_flags);
    }
    DependencyDiscoveryMode::apply_user_depfile_content_to_cc_context(
        &mut ctx,
        &dep_flags,
        &compilation.original_args,
    );
    let t_ctx = t0.elapsed();
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
    let worktree_salt = if cc_requires_worktree_salt(
        compilation.family,
        source_mode_for_key,
        &compilation.original_args,
        key_root.as_path(),
    ) {
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
    if state.watcher_active.load(Ordering::Acquire) {
        let entry = owned_fast_hit_entry(&state.fast_hit_cache, &context_key);
        if let Some(entry) = entry {
            if cache_entry_fresh_at(cache_now, entry.cached_at, FAST_HIT_MAX_AGE)
                && context_files_fresh(state, &context_key, &source_path, entry.clock)
            {
                let artifact_key_hex = &entry.artifact_key_hex;
                // Write outputs directly from the resolved payload Arc — no
                // DashMap guard is held here (issue #1180's short-lock model):
                // `lookup_artifact_with_disk_fallback` clones the owned
                // artifact out of the map before this runs, and
                // `ensure_payloads_for_materialization` returns an owned,
                // typed result instead of borrowing. Each check_unit_cache
                // runs in its own blocking task, so writes are already
                // parallel across units.
                if let Some(cached) = lookup_artifact_with_disk_fallback(state, artifact_key_hex) {
                    match ensure_payloads_for_materialization_for_state(
                        state,
                        &cached,
                        artifact_key_hex,
                    ) {
                        Ok(payloads) => {
                            record_artifact_access(
                                state,
                                artifact_key_hex,
                                &cached,
                                std::time::Instant::now(),
                            );
                            let names = Arc::clone(&cached.meta.output_names);
                            let artifact_bytes: u64 = cached.meta.total_size;
                            let stdout = cached.stdout.clone();
                            let stderr = cached.stderr.clone();

                            let targets: Vec<NormalizedPath> = (0..payloads.len())
                                .map(|i| {
                                    let out: NormalizedPath = if i == 0 {
                                        output_path.clone()
                                    } else {
                                        cwd_path.join(&names[i]).into()
                                    };
                                    out
                                })
                                .collect();
                            let materialization = materialize_multi_hit(&targets, &payloads);
                            payloads.record_staged_lock_timings(&state.profiler.staged);
                            drop(payloads);
                            if let Err(failure) = &materialization {
                                report_materialization_failure(
                                    &state.cache_dir,
                                    artifact_key_hex,
                                    "multi-fast-hit",
                                    failure,
                                );
                            }
                            if materialization.is_ok() {
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
                                };
                            }
                            // A caller-owned destination failure is a soft miss:
                            // recompile this TU but preserve the valid shared
                            // artifact for its siblings.  Only an absent blob is
                            // structural evidence that permits eviction.
                            if let Err(failure) = &materialization {
                                invalidate_multi_artifact_after_failure(
                                    state,
                                    artifact_key_hex,
                                    failure,
                                );
                            }
                            state.fast_hit_cache.remove(&context_key);
                        }
                        Err(failure) => {
                            report_materialization_failure(
                                &state.cache_dir,
                                artifact_key_hex,
                                "multi-fast-hit",
                                &failure,
                            );
                            invalidate_multi_artifact_after_failure(
                                state,
                                artifact_key_hex,
                                &failure,
                            );
                            state.fast_hit_cache.remove(&context_key);
                        }
                    }
                } else {
                    let failure = cache_blob_missing(
                        &state.artifact_dir.join(artifact_key_hex),
                        std::io::Error::new(
                            std::io::ErrorKind::NotFound,
                            "cached multi-source artifact metadata is unavailable",
                        ),
                    );
                    report_materialization_failure(
                        &state.cache_dir,
                        artifact_key_hex,
                        "multi-fast-hit",
                        &failure,
                    );
                    invalidate_multi_artifact_after_failure(state, artifact_key_hex, &failure);
                    state.fast_hit_cache.remove(&context_key);
                }
            }
        }
    }

    let source_hash = match hash_file(&state.cache_system, &source_path, snap_clock) {
        Ok(h) => h,
        Err(_) => {
            return UnitCacheResult::Miss {
                source_path,
                output_path,
                context_key,
                ctx: Box::new(ctx),
                input_snapshot: InputSnapshot::incomplete(snap_clock),
                reason: miss_reason::INPUT_FINGERPRINT_MISMATCH,
            };
        }
    };
    let t_hash_source = t0.elapsed();

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
    let mut miss_attribution = match &verdict {
        crate::depgraph::CacheVerdict::Hit { .. } => miss_reason::NO_ARTIFACT_FOR_KEY,
        crate::depgraph::CacheVerdict::SourceChanged { .. }
        | crate::depgraph::CacheVerdict::HeadersChanged { .. }
        | crate::depgraph::CacheVerdict::NeedsPreprocessor => {
            miss_reason::INPUT_FINGERPRINT_MISMATCH
        }
        crate::depgraph::CacheVerdict::Cold => miss_reason::CONTEXT_NOT_FOUND,
    };

    let depgraph_claimed_hit = matches!(verdict, crate::depgraph::CacheVerdict::Hit { .. });
    if let crate::depgraph::CacheVerdict::Hit { artifact_key }
    | crate::depgraph::CacheVerdict::SourceChanged { artifact_key } = verdict
    {
        let artifact_key_hex = artifact_key.hash().to_hex();
        if let Some(cached) = lookup_artifact_with_disk_fallback(state, &artifact_key_hex) {
            let t_lookup = t0.elapsed();
            match ensure_payloads_for_materialization_for_state(state, &cached, &artifact_key_hex) {
                Ok(payloads) => {
                    record_artifact_access(
                        state,
                        &artifact_key_hex,
                        &cached,
                        std::time::Instant::now(),
                    );
                    let names = Arc::clone(&cached.meta.output_names);
                    let artifact_bytes: u64 = cached.meta.total_size;
                    let stdout = cached.stdout.clone();
                    let stderr = cached.stderr.clone();

                    let targets: Vec<NormalizedPath> = (0..payloads.len())
                        .map(|i| {
                            let out: NormalizedPath = if i == 0 {
                                output_path.clone()
                            } else {
                                cwd_path.join(&names[i]).into()
                            };
                            out
                        })
                        .collect();
                    let materialization = materialize_multi_hit(&targets, &payloads);
                    payloads.record_staged_lock_timings(&state.profiler.staged);
                    drop(payloads);
                    if let Err(failure) = &materialization {
                        report_materialization_failure(
                            &state.cache_dir,
                            &artifact_key_hex,
                            "multi-depgraph-hit",
                            failure,
                        );
                        miss_attribution = match failure {
                            MaterializationFailure::CacheBlobMissing(_) => {
                                miss_reason::NO_ARTIFACT_FOR_KEY
                            }
                            MaterializationFailure::CacheRead(_) => {
                                miss_reason::NO_ARTIFACT_FOR_KEY
                            }
                            MaterializationFailure::DestinationWrite(_) => {
                                miss_reason::DESTINATION_WRITE_FAILED
                            }
                        };
                    }
                    if depgraph_claimed_hit {
                        if let Err(failure) = &materialization {
                            invalidate_multi_artifact_after_failure(
                                state,
                                &artifact_key_hex,
                                failure,
                            );
                        }
                    }
                    if materialization.is_ok() {
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
                        };
                    }
                }
                Err(failure) => {
                    report_materialization_failure(
                        &state.cache_dir,
                        &artifact_key_hex,
                        "multi-depgraph-hit",
                        &failure,
                    );
                    miss_attribution = match &failure {
                        MaterializationFailure::DestinationWrite(_) => {
                            miss_reason::DESTINATION_WRITE_FAILED
                        }
                        MaterializationFailure::CacheBlobMissing(_)
                        | MaterializationFailure::CacheRead(_) => miss_reason::NO_ARTIFACT_FOR_KEY,
                    };
                    if depgraph_claimed_hit {
                        invalidate_multi_artifact_after_failure(state, &artifact_key_hex, &failure);
                    }
                }
            }
        } else if depgraph_claimed_hit {
            let failure = cache_blob_missing(
                &state.artifact_dir.join(artifact_key_hex.as_str()),
                std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "cached multi-source artifact metadata is unavailable",
                ),
            );
            report_materialization_failure(
                &state.cache_dir,
                artifact_key_hex.as_str(),
                "multi-depgraph-hit",
                &failure,
            );
            invalidate_multi_artifact_after_failure(state, artifact_key_hex.as_str(), &failure);
            miss_attribution = miss_reason::NO_ARTIFACT_FOR_KEY;
        }
    }

    state.fast_hit_cache.remove(&context_key);
    let input_snapshot = InputSnapshot::capture(
        state,
        &source_path,
        &ctx,
        hash_map,
        scan_cache,
        dependency_mode,
    );
    UnitCacheResult::Miss {
        source_path,
        output_path,
        context_key,
        ctx: Box::new(ctx),
        input_snapshot,
        reason: miss_attribution,
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
    stdin: Vec<u8>,
    compile_start: Instant,
    dependency_mode: DependencyDiscoveryMode,
) -> Response {
    let snap_clock = state.cache_system.current_clock();
    let mut all_stdout = Vec::new();
    let mut all_stderr = Vec::new();
    let key_root = worktree_root.as_ref().unwrap_or(&cwd_path).clone();

    if let Some(response) = preflight::run_unsupported_batch(
        &state,
        &sid,
        &compiler,
        &compilations,
        &original_args,
        &cwd_path,
        &client_env,
        &stdin,
    )
    .await
    {
        return response;
    }

    // ── Pre-parse shared args once for all units ─────────────────────
    // All units share the same original_args (via Arc) — only source/output
    // differ. Parse the flags once and reuse the base CompileContext, avoiding
    // redundant arg parsing for each of the N compilation units.
    let (shared_base, shared_dep_flags): (Arc<CompileContext>, UserDepFlags) = {
        let first = &compilations[0];
        let parsed = if first.family == crate::compiler::CompilerFamily::Msvc
            || crate::compiler::parse_msvc::looks_like_msvc_args(&first.original_args)
        {
            crate::depgraph::msvc_args::parse_msvc_args(&first.original_args, &cwd_path)
        } else {
            crate::depgraph::args::parse_gnu_args(&first.original_args, &cwd_path)
        };
        let dep_flags = parsed.dep_flags.clone();
        // Issue #1166: compiler identity must vary the shared base
        // context's key too, mirroring the non-shared per-unit path's
        // `build_compile_context` call above.
        let compiler_hash = state
            .compiler_hash_cache
            .get_or_hash_with(&first.compiler, hash_cc_identity)
            .unwrap_or(COMPILER_HASH_UNAVAILABLE);
        let mut base = CompileContext::from_parsed_args(parsed, compiler_hash);
        for path in &system_includes {
            if !base.include_search.system.contains(path) {
                base.include_search.system.push(path.clone());
            }
        }
        dependency_mode.apply_to_cc_context(&mut base, &dep_flags);
        (Arc::new(base), dep_flags)
    };

    // ── Phase 1: Check cache for each unit (parallel, as-completed) ──
    let mut join_set = tokio::task::JoinSet::new();
    let scan_cache = Arc::new(crate::depgraph::scanner::RecursiveScanCache::default());
    for (idx, compilation) in compilations.iter().enumerate() {
        let state = Arc::clone(&state);
        let cwd_path = cwd_path.clone();
        let key_root = key_root.clone();
        let system_includes = system_includes.clone();
        let compilation = compilation.clone();
        let shared_base = Arc::clone(&shared_base);
        let shared_dep_flags = shared_dep_flags.clone();
        let scan_cache = Arc::clone(&scan_cache);
        let cache_now = compile_start;
        join_set.spawn_blocking(move || {
            (
                idx,
                check_unit_cache(
                    &state,
                    &compilation,
                    UnitCacheCheck {
                        cwd_path: &cwd_path,
                        key_root: &key_root,
                        system_includes: &system_includes,
                        shared_base: Some(&shared_base),
                        shared_dep_flags: Some(&shared_dep_flags),
                        scan_cache: &scan_cache,
                        cache_now,
                        dependency_mode,
                    },
                ),
            )
        });
    }

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
    for (_, result) in indexed_results {
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
        unit_results.push(result);
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
    if let Some(reason) = unit_results
        .iter()
        .filter_map(|result| match result {
            UnitCacheResult::Miss { reason, .. } => Some(*reason),
            UnitCacheResult::Hit { .. } => None,
        })
        .min_by_key(|reason| match *reason {
            miss_reason::DESTINATION_WRITE_FAILED => 0,
            miss_reason::NO_ARTIFACT_FOR_KEY => 1,
            miss_reason::INPUT_FINGERPRINT_MISMATCH => 2,
            _ => 3,
        })
    {
        record_miss_reason(reason);
    }

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

    if let Some(response) = staged::try_handle_staged_misses(
        &state,
        &sid,
        &compiler,
        &compilations,
        &original_args,
        &source_indices,
        &unit_results,
        &cwd_path,
        &key_root,
        &client_env,
        &mut all_stdout,
        &mut all_stderr,
        dependency_mode,
    )
    .await
    {
        return response;
    }

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
    let retained_source_indices: HashSet<usize> = source_indices
        .iter()
        .copied()
        .filter(|index| !hit_indices.contains(index))
        .collect();
    let mut compiler_args =
        filter_multi_source_args(&original_args, &source_indices, &retained_source_indices);
    let depfile_policy = legacy_depfile_policy(&original_args, dependency_mode);
    if supports_depfile {
        if let Some(flag) = depfile_policy.injected_flag {
            compiler_args.push(flag.to_string());
        }
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
    let built_in_exclusive =
        crate::daemon::server::compile_resource_gate::requires_exclusive_access_for_misses(
            compilations[0].family,
            &compiler_args,
            unit_results.iter().map(|unit| match unit {
                UnitCacheResult::Miss { source_path, .. } => Some(source_path.as_path()),
                UnitCacheResult::Hit { .. } => None,
            }),
        );
    let (exclusive, host_admission) =
        match crate::daemon::server::compile_resource_gate::host_admission(
            &state,
            compilations[0].family,
            &compiler_args,
            None,
            built_in_exclusive,
        )
        .await
        {
            Ok(exclusive) => exclusive,
            Err(error) => {
                return Response::Error {
                    message: format!("host compiler admission failed: {error}"),
                };
            }
        };
    let (compiler_admission, _) =
        crate::daemon::server::compile_resource_gate::acquire_compiler_admission(
            &state,
            exclusive,
            host_admission,
        )
        .await;
    let result =
        super::super::process::tokio_command_output_with_priority(&mut cmd, compiler_priority)
            .await;
    let _resource_admission = compiler_admission.release_compiler_slot();

    let output = match result {
        Ok(o) => o,
        Err(e) => {
            return Response::Error {
                message: format!("failed to run compiler: {e}"),
            };
        }
    };

    let exit_code = crate::platform::process::exit::outcome(&output.status).exit_code;
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
    let mut miss_set: tokio::task::JoinSet<MissOutcome> = tokio::task::JoinSet::new();
    for unit in &unit_results {
        let (source_path, output_path, context_key, ctx) = match unit {
            UnitCacheResult::Miss {
                source_path,
                output_path,
                context_key,
                ctx,
                ..
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
        let dependency_mode_task = dependency_mode;
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

            let mut used_static_fallback = !supports_depfile;
            let mut scan_result = if supports_depfile {
                let d_path = source_path.with_extension("d");
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
                        used_static_fallback = true;
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
            if !used_static_fallback
                && dependency_mode_task == DependencyDiscoveryMode::AllHeaders
                && depfile_policy.excludes_system_headers
            {
                scan_result = crate::depgraph::depfile::merge_scan_results_conservative(
                    scan_result,
                    crate::depgraph::scanner::scan_recursive(&source_path, &ctx.include_search),
                );
            }
            if used_static_fallback {
                dependency_mode_task.apply_static_fallback(&mut scan_result, &ctx.include_search);
            }

            let tracked_paths: Vec<NormalizedPath> = std::iter::once(source_path.clone())
                .chain(scan_result.resolved.iter().cloned())
                .collect();
            state_task.cache_system.register_tracked(&tracked_paths);

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

            let Some(_publication_guard) = begin_artifact_publication_blocking(&state_task) else {
                return MissOutcome {
                    dep_dirs,
                    output_path,
                    persist: None,
                };
            };
            let get_hash = |p: &Path| {
                let path = NormalizedPath::new(p);
                hash_map.get(&path).copied()
            };
            let allow_fast_hit = !scan_result.has_computed;
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
                // Build the cached artifact directly (avoid constructing the
                // full ArtifactData wrapper just to compute the same fields).
                // Keep payloads lazy so the first hit resolves whichever
                // persistence layout is active. Staged artifacts do not live
                // at the legacy `{key}_0` path.
                let empty = Arc::new(Vec::new());
                let meta = ArtifactIndex::new(
                    vec![output_name],
                    vec![artifact_bytes],
                    Arc::clone(&empty),
                    Arc::clone(&empty),
                    0,
                );
                let cached = CachedArtifact::from_index(meta.clone());

                state_task
                    .artifacts
                    .insert(artifact_key_hex.clone(), cached);

                if allow_fast_hit {
                    let current_clock = state_task.cache_system.current_clock();
                    state_task.fast_hit_cache.insert(
                        context_key,
                        FastHitEntry {
                            clock: current_clock,
                            artifact_key_hex: artifact_key_hex.clone(),
                            cached_at: std::time::Instant::now(),
                        },
                    );
                }

                state_task.stats.record_miss(0, artifact_bytes);
                let src = source_path.clone();
                record_session_stat(&state_task.sessions, &sid_task, move |t| {
                    t.record_miss(src, artifact_bytes);
                });

                // Files are already on disk via the hardlink above. The
                // remaining work is the index entry, which goes through
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

    let dep_dirs_vec: Vec<NormalizedPath> = all_dep_dirs.into_iter().collect();
    watch_directories(&state, &dep_dirs_vec).await;

    for job in persist_jobs {
        let Some(publication_guard_for_task) = begin_artifact_publication(&state).await else {
            continue;
        };
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
        let index_writer_tx = state.index_writer_tx.clone();
        // Issue #728: capture the per-job enqueue Instant so the WARN below
        // can report `gap_ms` = "linker-success-recorded → persist-attempt-
        // started" (distinguishes queue starvation under burst load from
        // src/dst failures already enriched by `persist::enrich_persist_err`).
        let t_persist_enqueue = std::time::Instant::now();
        // Track this deferred persist in pending_cache_writes so the
        // shutdown drain (wal::drain_durable_state_for_shutdown) awaits it
        // — the single-file path registers its deferred persist the same
        // way (miss_store.rs). Untracked, a 2-core host can shut down
        // between the compile response and persist_artifact_payloads,
        // losing the artifact + index row and downgrading the next
        // daemon's warm multi hits to recompiles (#1154 stabilization).
        let _pending = pending_writes::register(&state.pending_cache_writes, &key_hex);
        let completion_key = key_hex.clone();
        let pending_state = Arc::clone(&state);
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
                let _publication_guard = publication_guard_for_task;
                let gap_ms = t_persist_enqueue.elapsed().as_millis() as u64;
                match persist_artifact_payloads(&artifact_dir, &key_hex, &payloads) {
                    Ok(()) => {
                        let _ =
                            index_writer_tx.send(IndexWriterCommand::Insert(key_hex, persist_meta));
                    }
                    Err(e) => {
                        tracing::warn!(
                            key = %key_hex,
                            gap_ms,
                            "failed to persist artifact output: {e}"
                        );
                    }
                }
            })
            .await;
            if let Err(error) = written {
                tracing::warn!(%error, "multi-source artifact persistence task failed to join");
            }
            pending_writes::complete(&pending_state.pending_cache_writes, &completion_key);
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
