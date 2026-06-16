//! Successful-compile store path: scan deps, hash all, store artifact, emit
//! miss profiles, and schedule background watcher updates.
//!
//! Runs only when the compiler returned `exit_code == 0`. Drives the
//! `miss_store` + `miss_profile` helpers and finalizes the response.

use super::super::super::*;
use super::super::miss_profile::{
    emit_cc_miss_profile, emit_rust_miss_profile, CcMissProfile, RustMissProfile,
};
use super::super::miss_store::{
    store_miss_artifact, MissArtifactStoreRequest, MissArtifactStoreStats,
};

pub(super) struct StoreOutcomeRequest<'a> {
    pub(super) state_arc: &'a Arc<SharedState>,
    pub(super) sid: &'a SessionId,
    pub(super) context_key: &'a ContextKey,
    pub(super) source_path: &'a NormalizedPath,
    pub(super) output_path: &'a NormalizedPath,
    pub(super) cwd: &'a Path,
    pub(super) cwd_path: &'a NormalizedPath,
    pub(super) ctx: &'a CompileContext,
    pub(super) compilation: &'a crate::compiler::CacheableCompilation,
    pub(super) rustc_args_opt: Option<&'a crate::depgraph::RustcParsedArgs>,
    pub(super) rustc_extern_paths: &'a [NormalizedPath],
    pub(super) is_rustc: bool,
    pub(super) rust_profile_enabled: bool,
    pub(super) rust_profile_mode: &'a str,
    pub(super) stdout: Arc<Vec<u8>>,
    pub(super) stderr: Arc<Vec<u8>>,
    pub(super) exit_code: i32,
    pub(super) depfile_strategy: DepfileStrategy,
    pub(super) show_includes_scan: Option<crate::depgraph::ScanResult>,
    pub(super) pre_hash_task: Option<tokio::task::JoinHandle<HashMap<NormalizedPath, ContentHash>>>,
    pub(super) compiler_priority_decision: crate::daemon::process::CompilePriorityDecision,
    pub(super) compile_start: std::time::Instant,
    pub(super) snap_clock: Clock,
    // Phase timings carried into miss profiles
    pub(super) compiler_exec_ns: u64,
    pub(super) compiler_process_ns: u64,
    pub(super) compiler_prep_ns: u64,
    pub(super) post_exec_ns: u64,
    pub(super) pre_exec_ns: u64,
    pub(super) system_includes_ns: u64,
    pub(super) system_watch_ns: u64,
    pub(super) parse_args_ns: u64,
    pub(super) build_context_ns: u64,
    pub(super) hash_source_ns: u64,
    pub(super) hash_headers_ns: u64,
    pub(super) depgraph_check_ns: u64,
    pub(super) break_outputs_ns: u64,
}

/// Drive the post-compile success path. Returns `Some(Response)` only when an
/// output-collection failure forces an uncached `CompileResult`; otherwise
/// returns `None` to signal the orchestrator should respond with the standard
/// `CompileResult { cached: false }` after this returns.
#[allow(clippy::too_many_lines)] // Mirrors the original monolithic block.
pub(super) async fn store_successful_compile(req: StoreOutcomeRequest<'_>) -> Option<Response> {
    let StoreOutcomeRequest {
        state_arc,
        sid,
        context_key,
        source_path,
        output_path,
        cwd,
        cwd_path,
        ctx,
        compilation,
        rustc_args_opt,
        rustc_extern_paths,
        is_rustc,
        rust_profile_enabled,
        rust_profile_mode,
        stdout,
        stderr,
        exit_code,
        depfile_strategy,
        show_includes_scan,
        pre_hash_task,
        compiler_priority_decision,
        compile_start,
        snap_clock,
        compiler_exec_ns,
        compiler_process_ns,
        compiler_prep_ns,
        post_exec_ns,
        pre_exec_ns,
        system_includes_ns,
        system_watch_ns,
        parse_args_ns,
        build_context_ns,
        hash_source_ns,
        hash_headers_ns,
        depgraph_check_ns,
        break_outputs_ns,
    } = req;

    let state = state_arc.as_ref();

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
        let all = collect_rustc_output_files(rustc_args_opt.unwrap(), output_path, cwd_path);
        if all.is_empty() {
            tracing::warn!("failed to stat output file {}", output_path.display());
            return Some(Response::CompileResult {
                exit_code,
                stdout: Arc::clone(&stdout),
                stderr: Arc::clone(&stderr),
                cached: false,
            });
        }
        (Vec::new(), Some(all))
    } else {
        match std::fs::read(output_path) {
            Ok(data) => (data, None),
            Err(e) => {
                tracing::warn!("failed to read output file {}: {e}", output_path.display());
                return Some(Response::CompileResult {
                    exit_code,
                    stdout: Arc::clone(&stdout),
                    stderr: Arc::clone(&stderr),
                    cached: false,
                });
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
    // Issue #643: while we read the user's depfile to populate the
    // depgraph, also capture its bytes (only for the `UserSpecified` /
    // `UserDefault` strategies — `Injected` is a daemon-private file
    // the user never asked for; MSVC `ShowIncludes` has no on-disk
    // depfile). Those captured bytes are cached as a second artifact
    // output so the next hit can write the depfile back to the
    // *current* build's `-MF` target, even after `git clean` or
    // worktree-rename — closing the stale-incremental-build bug.
    let t_scan = std::time::Instant::now();
    let mut user_depfile_capture: Option<(NormalizedPath, Vec<u8>)> = None;
    let scan_result = if is_rustc {
        // Rustc: try to parse the dep-info file if --emit included dep-info.
        // The dep-info file is in --out-dir with crate name and extra-filename.
        scan_rustc_deps(rustc_args_opt.unwrap(), source_path, cwd_path)
    } else {
        match &depfile_strategy {
            DepfileStrategy::Injected { path }
            | DepfileStrategy::UserSpecified { path }
            | DepfileStrategy::UserDefault { path } => {
                let want_capture = matches!(
                    depfile_strategy,
                    DepfileStrategy::UserSpecified { .. } | DepfileStrategy::UserDefault { .. }
                );
                let scan_cwd: NormalizedPath = cwd.into();
                match crate::depgraph::depfile::parse_depfile_path(path, source_path, &scan_cwd) {
                    Ok(result) => {
                        if want_capture {
                            if let Ok(bytes) = std::fs::read(path) {
                                user_depfile_capture = Some((path.clone(), bytes));
                            }
                        }
                        if matches!(depfile_strategy, DepfileStrategy::Injected { .. }) {
                            let _ = std::fs::remove_file(path);
                        }
                        result
                    }
                    Err(e) => {
                        tracing::warn!("depfile parse failed, falling back to scanner: {e}");
                        write_session_log(
                            &state.sessions,
                            sid,
                            &format!(
                                "[DIAG] depfile_parse_fail: path={} error={e}",
                                path.display()
                            ),
                        );
                        if matches!(depfile_strategy, DepfileStrategy::Injected { .. }) {
                            let _ = std::fs::remove_file(path);
                        }
                        crate::depgraph::scanner::scan_recursive(source_path, &ctx.include_search)
                    }
                }
            }
            DepfileStrategy::ShowIncludes => {
                // Already parsed from stderr above.
                show_includes_scan.unwrap_or_else(|| {
                    crate::depgraph::scanner::scan_recursive(source_path, &ctx.include_search)
                })
            }
            DepfileStrategy::Unsupported => {
                crate::depgraph::scanner::scan_recursive(source_path, &ctx.include_search)
            }
        }
    };
    let include_scan_ns = t_scan.elapsed().as_nanos() as u64;

    // Register scanned paths for zero-syscall fast path on future hits.
    let tracked_paths: Vec<NormalizedPath> = std::iter::once(source_path.clone())
        .chain(scan_result.resolved.iter().cloned())
        .chain(ctx.force_includes.iter().cloned())
        .chain(rustc_extern_paths.iter().cloned())
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
        for ext in rustc_extern_paths {
            if let Some(parent) = ext.parent() {
                dirs.insert(parent.into());
            }
        }
        dirs.into_iter().collect()
    };
    let dep_dirs_ns = t_dep_dirs.elapsed().as_nanos() as u64;

    // ── Phase: hash all files (parallel) ─────────────────────────
    // Hash source + resolved headers + force-includes + rustc externs
    // using rayon parallel iteration. Issue #532: for rustc, the
    // source + extern paths were already kicked off pre-compile via
    // `pre_hash_task`; join here and then only hash the late-arriving
    // includes (scan_result.resolved + force_includes), which are
    // typically empty for workspace link.
    let t_hash = std::time::Instant::now();
    let mut hash_map: HashMap<NormalizedPath, ContentHash> = match pre_hash_task {
        Some(task) => task.await.unwrap_or_default(),
        None => HashMap::new(),
    };
    let pre_hashed_count = hash_map.len();
    {
        use rayon::prelude::*;
        // Build the post-compile path list. When pre_hash_task ran,
        // we skip source + externs (already hashed). Otherwise hash
        // everything as before (C/C++ fallback).
        let post_paths: Vec<&NormalizedPath> = if pre_hashed_count > 0 {
            scan_result
                .resolved
                .iter()
                .chain(ctx.force_includes.iter())
                .filter(|p| !hash_map.contains_key(*p))
                .collect()
        } else {
            std::iter::once(source_path)
                .chain(scan_result.resolved.iter())
                .chain(ctx.force_includes.iter())
                .chain(rustc_extern_paths.iter())
                .collect()
        };

        let results: Vec<_> = post_paths
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
                sid,
                &format!(
                    "[DIAG] hash_failures: {} of {} files failed to hash for {}",
                    hash_failures,
                    1 + scan_result.resolved.len()
                        + ctx.force_includes.len()
                        + rustc_extern_paths.len(),
                    source_path.display(),
                ),
            );
        }
    }
    let hash_all_ns = t_hash.elapsed().as_nanos() as u64;

    let MissArtifactStoreStats {
        artifact_store_ns,
        depgraph_update_ns,
        artifact_build_ns,
        persist_enqueue_ns,
        artifact_insert_stats_ns,
        artifact_meta_build_ns,
        rust_snapshot_ns,
        rust_snapshot_hardlink_count,
        rust_snapshot_copy_count,
        rust_snapshot_copy_bytes,
        rust_snapshot_error_count,
        artifact_index_build_ns,
        artifact_index_persist_ns,
        artifact_memory_insert_ns,
    } = store_miss_artifact(MissArtifactStoreRequest {
        state_arc,
        sid,
        context_key,
        source_path,
        output_path,
        scan_result,
        hash_map: &hash_map,
        output_data,
        user_depfile: user_depfile_capture,
        rustc_all_outputs: rustc_all_outputs.as_deref(),
        stdout: &stdout,
        stderr: &stderr,
        exit_code,
        compile_start,
    });

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
    // Issue #535: emit a non-rustc cold-miss profile when
    // `ZCCACHE_PROFILE_CC_MISS` is set. Lets the benchmark-stats log
    // carry phase data for c-static-library-link / cpp-driver-link
    // cold rows, the next perf targets after #517 closed.
    if !is_rustc && std::env::var_os(CC_MISS_PROFILE_ENV).is_some() {
        emit_cc_miss_profile(CcMissProfile {
            family: match compilation.family {
                crate::compiler::CompilerFamily::Gcc => "gcc",
                crate::compiler::CompilerFamily::Clang => "clang",
                crate::compiler::CompilerFamily::Msvc => "msvc",
                // Rust handled above; Rustfmt is a non-cacheable formatter.
                crate::compiler::CompilerFamily::Rustc => "rustc",
                crate::compiler::CompilerFamily::Rustfmt => "rustfmt",
            },
            compiler_priority_decision,
            total_ns,
            pre_exec_ns,
            system_includes_ns,
            system_watch_ns,
            parse_args_ns,
            build_context_ns,
            break_outputs_ns,
            compiler_process_ns,
            post_exec_ns,
            include_scan_ns,
            register_tracked_ns,
            dep_dirs_ns,
            hash_all_ns,
            artifact_store_ns,
            depgraph_update_ns,
            artifact_build_ns,
            persist_enqueue_ns,
            artifact_insert_stats_ns,
            artifact_index_build_ns,
            artifact_index_persist_ns,
            artifact_memory_insert_ns,
        });
    }

    if rust_profile_enabled {
        emit_rust_miss_profile(RustMissProfile {
            mode: rust_profile_mode,
            compiler_priority_decision,
            total_ns,
            pre_exec_ns,
            system_includes_ns,
            system_watch_ns,
            parse_args_ns,
            build_context_ns,
            hash_source_ns,
            hash_headers_ns,
            depgraph_check_ns,
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
        });
    }

    {
        let bg_state = Arc::clone(state_arc);
        let bg_output_path = output_path.clone();
        tokio::spawn(async move {
            let state = &*bg_state;
            watch_directories(state, &dep_dirs).await;
            if let Some(out_dir) = bg_output_path.parent() {
                watch_directory(state, out_dir).await;
            }
            state.cache_system.apply_changes(vec![bg_output_path]);
        });
    }

    None
}
