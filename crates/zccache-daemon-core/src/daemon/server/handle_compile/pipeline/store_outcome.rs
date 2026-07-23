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
    preserve_staged_depfile_for_persistence, store_miss_artifact, MissArtifactStoreRequest,
    MissArtifactStoreStats,
};

#[path = "store_outcome_scan.rs"]
mod scan;
use scan::{collect_compile_scan_blocking, CompileScanCollection, CompileScanRequest};

pub(super) struct StoreOutcomeRequest<'a> {
    pub(super) state_arc: &'a Arc<SharedState>,
    pub(super) sid: &'a SessionId,
    pub(super) context_key: &'a ContextKey,
    pub(super) source_path: &'a NormalizedPath,
    pub(super) output_path: &'a NormalizedPath,
    pub(super) compiler_output_path: NormalizedPath,
    pub(super) staged_output_paths: Option<Vec<NormalizedPath>>,
    pub(super) staged_plan: Option<StagedCompilePlan>,
    pub(super) synchronous_persist: bool,
    pub(super) cwd_path: &'a NormalizedPath,
    pub(super) ctx: &'a CompileContext,
    pub(super) compilation: &'a crate::compiler::CacheableCompilation,
    pub(super) dependency_mode: DependencyDiscoveryMode,
    pub(super) rustc_args_opt: Option<&'a crate::depgraph::RustcParsedArgs>,
    pub(super) rustc_extern_paths: &'a [NormalizedPath],
    /// Request env the compile ran under — the value source for rustc
    /// env-dep snapshots (zccache#1021).
    pub(super) client_env: Option<&'a [(String, String)]>,
    pub(super) is_rustc: bool,
    pub(super) rust_profile_enabled: bool,
    pub(super) rust_profile_mode: &'a str,
    pub(super) stdout: Arc<Vec<u8>>,
    pub(super) stderr: Arc<Vec<u8>>,
    pub(super) exit_code: i32,
    pub(super) depfile_strategy: DepfileStrategy,
    pub(super) show_includes_scan: Option<crate::depgraph::ScanResult>,
    pub(super) pre_hash_task: Option<tokio::task::JoinHandle<HashMap<NormalizedPath, ContentHash>>>,
    /// Issue #401: pre-compile hashes already produced by `hash_and_verify`
    /// on the cc/cpp miss path. `None` means the pre-compile hash phase did
    /// not run (e.g. cold context, source-hash fallback) — fall back to
    /// hashing everything as before. When `Some`, the same `(path, hash)`
    /// pairs are seeded into the post-compile `hash_map` so the parallel
    /// rayon hash skips files already covered. The rustc path uses
    /// `pre_hash_task` (a `JoinHandle`) instead and is unaffected.
    pub(super) pre_hashed: Option<HashMap<NormalizedPath, ContentHash>>,
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

enum CompileOutputCollection {
    Rustc(Vec<RustcOutputFile>),
    Bytes(Vec<u8>),
}

async fn collect_compile_outputs_blocking(
    is_rustc: bool,
    rustc_args: Option<crate::depgraph::RustcParsedArgs>,
    output_path: NormalizedPath,
    cwd_path: NormalizedPath,
    staged_output_paths: Option<Vec<NormalizedPath>>,
) -> std::io::Result<CompileOutputCollection> {
    tokio::task::spawn_blocking(move || {
        if is_rustc {
            let Some(rustc_args) = rustc_args else {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "missing parsed rustc args for rustc output collection",
                ));
            };
            let outputs = if let Some(paths) = staged_output_paths {
                paths
                    .into_iter()
                    .filter_map(|path| {
                        let metadata = std::fs::metadata(&path).ok()?;
                        if !metadata.is_file() {
                            return None;
                        }
                        let name = path.file_name()?.to_string_lossy().into_owned();
                        Some(RustcOutputFile {
                            name,
                            path,
                            size: metadata.len(),
                        })
                    })
                    .collect()
            } else {
                collect_rustc_output_files(&rustc_args, &output_path, &cwd_path)
            };
            if outputs.is_empty() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "rustc primary output was not found",
                ));
            }
            Ok(CompileOutputCollection::Rustc(outputs))
        } else {
            std::fs::read(&output_path).map(CompileOutputCollection::Bytes)
        }
    })
    .await
    .map_err(|e| std::io::Error::other(format!("compile output worker failed: {e}")))?
}

#[cfg(test)]
#[path = "store_outcome_tests.rs"]
mod tests;

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
        mut compiler_output_path,
        staged_output_paths,
        mut staged_plan,
        synchronous_persist,
        cwd_path,
        ctx,
        compilation,
        dependency_mode,
        rustc_args_opt,
        rustc_extern_paths,
        client_env,
        is_rustc,
        rust_profile_enabled,
        rust_profile_mode,
        stdout,
        stderr,
        exit_code,
        depfile_strategy,
        show_includes_scan,
        pre_hash_task,
        pre_hashed,
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

    if let Some(plan) = staged_plan.as_ref() {
        if let Err(error) = plan.rewrite_logical_side_outputs() {
            use crate::daemon::staged_stats::{StagedCounter, StagedFailure};
            state
                .profiler
                .staged
                .count(StagedCounter::PublicationFailure);
            state
                .profiler
                .staged
                .failure(StagedFailure::LogicalDepfileRewrite);
            crate::core::lifecycle::write_event(
                "staged_logical_depfile_rewrite_failed",
                serde_json::json!({
                    "reason": "logical_depfile_rewrite",
                    "output_count": plan.output_paths().len(),
                }),
            );
            let _ = plan.cleanup();
            return Some(Response::Error {
                message: format!(
                    "failed to canonicalize staged depfile before publication: {error}"
                ),
            });
        }
    }

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
    let rustc_args_owned = rustc_args_opt.cloned();
    let output_collection = match collect_compile_outputs_blocking(
        is_rustc,
        rustc_args_owned.clone(),
        compiler_output_path.clone(),
        cwd_path.clone(),
        staged_output_paths,
    )
    .await
    {
        Ok(output_collection) => output_collection,
        Err(e) => {
            if let Some(plan) = staged_plan.as_ref() {
                let _ = plan.cleanup();
                return Some(Response::Error {
                    message: format!("successful compiler omitted a staged output: {e}"),
                });
            }
            if is_rustc {
                tracing::warn!("failed to stat output file {}: {e}", output_path.display());
            } else {
                tracing::warn!("failed to read output file {}: {e}", output_path.display());
            }
            return Some(Response::CompileResult {
                exit_code,
                stdout: Arc::clone(&stdout),
                stderr: Arc::clone(&stderr),
                cached: false,
            });
        }
    };
    let (output_data, rustc_all_outputs) = match output_collection {
        CompileOutputCollection::Rustc(outputs) => (Vec::new(), Some(outputs)),
        CompileOutputCollection::Bytes(data) => (data, None),
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
    let scan_rustc_args = rustc_args_owned.clone().map(|mut args| {
        if let Some(paths) = staged_plan.as_ref().map(StagedCompilePlan::output_paths) {
            if let Some(parent) = paths.first().and_then(|path| path.as_path().parent()) {
                args.out_dir = Some(parent.into());
            }
        }
        args
    });
    let requested_user_depfile_path = match &depfile_strategy {
        DepfileStrategy::UserSpecified { path, .. } | DepfileStrategy::UserDefault { path, .. } => {
            Some(path.clone())
        }
        _ => None,
    };
    let depfile_strategy = staged_plan
        .as_ref()
        .map_or(depfile_strategy.clone(), |plan| {
            plan.rewrite_depfile_strategy(depfile_strategy.clone())
        });
    let scan_collection = collect_compile_scan_blocking(CompileScanRequest {
        is_rustc,
        rustc_args: scan_rustc_args,
        source_path: source_path.clone(),
        cwd_path: cwd_path.clone(),
        depfile_strategy,
        show_includes_scan,
        include_search: ctx.include_search.clone(),
        dependency_mode,
    })
    .await;
    if let Some(warning) = &scan_collection.depfile_parse_warning {
        tracing::warn!("depfile parse failed, falling back to scanner: {warning}");
        write_session_log(
            &state.sessions,
            sid,
            &format!("[DIAG] depfile_parse_fail: {warning}"),
        );
    };
    let CompileScanCollection {
        scan_result,
        rustc_env_dep_names,
        mut user_depfile_capture,
        ..
    } = scan_collection;
    let user_depfile_persist_temp = if !is_rustc && staged_plan.is_some() {
        match preserve_staged_depfile_for_persistence(
            &mut user_depfile_capture,
            requested_user_depfile_path.as_ref(),
            state.depfile_tmpdir.as_path(),
        ) {
            Ok(temp) => temp,
            Err(error) => {
                use crate::daemon::staged_stats::{StagedCounter, StagedFailure};
                state
                    .profiler
                    .staged
                    .count(StagedCounter::PublicationFailure);
                state.profiler.staged.failure(StagedFailure::Publication);
                if let Some(plan) = staged_plan.as_ref() {
                    let _ = plan.cleanup();
                }
                return Some(Response::Error {
                    message: format!(
                        "failed to preserve staged depfile for durable publication: {error}"
                    ),
                });
            }
        }
    } else {
        None
    };
    // Resolve env-dep values from the request env NOW (the borrow doesn't
    // survive into the store task): the compile ran under exactly this env.
    let rustc_env_dep_values: Vec<(String, Option<String>)> = rustc_env_dep_names
        .iter()
        .map(|name| {
            let value = client_env
                .and_then(|env| env.iter().find(|(k, _)| k == name))
                .map(|(_, v)| v.clone());
            (name.clone(), value)
        })
        .collect();
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
    //
    // Issue #401: for cc/cpp, the miss path already hashed source +
    // the depgraph's stored include set in `hash_and_verify`. Seed the
    // local `hash_map` from `pre_hashed` so the parallel rayon hash
    // below skips files already present, instead of re-hashing every
    // header from scratch.
    let t_hash = std::time::Instant::now();
    let mut hash_map: HashMap<NormalizedPath, ContentHash> = match pre_hash_task {
        Some(task) => task.await.unwrap_or_default(),
        None => pre_hashed.unwrap_or_default(),
    };
    let pre_hashed_count = hash_map.len();
    // #955: run the parallel hash under block_in_place so a large extern
    // set (the whole workspace, for a consolidated crate) doesn't park the
    // tokio worker and stall the runtime under concurrent misses. See
    // process::run_cpu_blocking.
    crate::daemon::process::run_cpu_blocking(|| {
        use rayon::prelude::*;
        // Build the post-compile path list. When the pre-hash phase
        // populated `hash_map` (either rustc's `pre_hash_task` or the
        // cc/cpp `pre_hashed` seed from `hash_and_verify`), skip paths
        // already covered. Otherwise hash everything as before.
        let post_paths: Vec<&NormalizedPath> = if pre_hashed_count > 0 {
            std::iter::once(source_path)
                .chain(scan_result.resolved.iter())
                .chain(ctx.force_includes.iter())
                .chain(rustc_extern_paths.iter())
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
    });
    let hash_all_ns = t_hash.elapsed().as_nanos() as u64;

    // C/C++ staged outputs are already captured in memory above. Materialize
    // them before publishing the cache entry so the persistence worker can
    // hard-link the requested output asynchronously. Keeping the staged plan
    // alive through synchronous persistence made every cold C miss pay for a
    // second copy/hash publication on the response path.
    let staged_cc_materialized = if !is_rustc { staged_plan.take() } else { None };
    let mut staged_cc_materialization = None;
    if let Some(plan) = staged_cc_materialized {
        use crate::daemon::staged_stats::{
            StagedBytes, StagedCounter, StagedFailure, StagedTiming,
        };
        let started = std::time::Instant::now();
        match plan.materialize() {
            Ok(materialized) => {
                state.profiler.staged.add_count(
                    StagedCounter::MaterializeReflink,
                    materialized.reflink_count,
                );
                state
                    .profiler
                    .staged
                    .add_count(StagedCounter::MaterializeCopy, materialized.copy_count);
                state
                    .profiler
                    .staged
                    .bytes(StagedBytes::Materialization, materialized.copy_bytes);
                state.profiler.staged.timing(
                    StagedTiming::MissMaterialization,
                    started.elapsed().as_nanos() as u64,
                );
                compiler_output_path = output_path.clone();
                state.cache_system.apply_changes(vec![output_path.clone()]);
                staged_cc_materialization = Some(());
            }
            Err(error) => {
                state
                    .profiler
                    .staged
                    .count(StagedCounter::MaterializeFailure);
                state
                    .profiler
                    .staged
                    .failure(StagedFailure::RequestedMaterialization);
                state.profiler.staged.timing(
                    StagedTiming::MissMaterialization,
                    started.elapsed().as_nanos() as u64,
                );
                return Some(Response::Error {
                    message: format!("failed to materialize compiler output: {error}"),
                });
            }
        }
    }
    let synchronous_persist = synchronous_persist && staged_cc_materialization.is_none();

    // Acquire exactly one owned guard and either retain it for synchronous
    // publication or transfer it to the detached persist task. Re-acquiring a
    // fair RwLock while already holding a read guard can self-deadlock when a
    // Clear/GC writer is queued between the two reads.
    let store_stats = if let Some(publication_guard) = begin_artifact_publication(state_arc).await {
        // #955: the miss persist (a large rustc `.rlib` copy when it can't be
        // hardlinked cross-volume) is synchronous; run it under block_in_place
        // so it doesn't park the tokio worker. See process::run_cpu_blocking.
        crate::daemon::process::run_cpu_blocking(|| {
            store_miss_artifact(MissArtifactStoreRequest {
                state_arc,
                sid,
                context_key,
                source_path,
                output_path: &compiler_output_path,
                scan_result,
                rustc_env_dep_values,
                hash_map: &hash_map,
                output_data,
                user_depfile: user_depfile_capture,
                user_depfile_persist_temp,
                rustc_all_outputs: rustc_all_outputs.as_deref(),
                stdout: &stdout,
                stderr: &stderr,
                exit_code,
                compile_start,
                synchronous_persist,
                publication_guard,
            })
        })
    } else {
        MissArtifactStoreStats::default()
    };
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
        staged_failure_reason,
        artifact_index_build_ns,
        artifact_index_persist_ns,
        artifact_memory_insert_ns,
    } = store_stats;

    if let Some(plan) = staged_plan {
        use crate::daemon::staged_stats::{
            StagedBytes, StagedCounter, StagedFailure, StagedTiming,
        };
        let salvage = rust_snapshot_error_count > 0;
        let salvage_reason = staged_failure_reason.unwrap_or("publication");
        let output_count = plan.output_paths().len();
        let materialize_started = std::time::Instant::now();
        if salvage {
            state.profiler.staged.count(StagedCounter::SalvageAttempt);
            crate::core::lifecycle::write_event(
                "staged_salvage_started",
                serde_json::json!({
                    "reason": salvage_reason,
                    "output_count": output_count,
                    "copied_bytes": 0,
                    "elapsed_ns": 0,
                }),
            );
        }
        match plan.materialize() {
            Ok(materialized) => {
                let elapsed_ns = materialize_started.elapsed().as_nanos() as u64;
                state.profiler.staged.add_count(
                    StagedCounter::MaterializeReflink,
                    materialized.reflink_count,
                );
                state
                    .profiler
                    .staged
                    .add_count(StagedCounter::MaterializeCopy, materialized.copy_count);
                state
                    .profiler
                    .staged
                    .bytes(StagedBytes::Materialization, materialized.copy_bytes);
                if salvage {
                    state.profiler.staged.count(StagedCounter::SalvageSuccess);
                    state
                        .profiler
                        .staged
                        .timing(StagedTiming::Salvage, elapsed_ns);
                    state
                        .profiler
                        .staged
                        .bytes(StagedBytes::Salvage, materialized.copy_bytes);
                    crate::core::lifecycle::write_event(
                        "staged_salvage_complete",
                        serde_json::json!({
                            "reason": salvage_reason,
                            "output_count": output_count,
                            "copied_bytes": materialized.copy_bytes,
                            "elapsed_ns": elapsed_ns,
                        }),
                    );
                } else {
                    state
                        .profiler
                        .staged
                        .timing(StagedTiming::MissMaterialization, elapsed_ns);
                }
            }
            Err(error) => {
                let elapsed_ns = materialize_started.elapsed().as_nanos() as u64;
                let progress = materialization_error_progress(&error);
                state
                    .profiler
                    .staged
                    .add_count(StagedCounter::MaterializeReflink, progress.reflink_count);
                state
                    .profiler
                    .staged
                    .add_count(StagedCounter::MaterializeCopy, progress.copy_count);
                state
                    .profiler
                    .staged
                    .bytes(StagedBytes::Materialization, progress.copy_bytes);
                state
                    .profiler
                    .staged
                    .count(StagedCounter::MaterializeFailure);
                state
                    .profiler
                    .staged
                    .failure(StagedFailure::RequestedMaterialization);
                if salvage {
                    state.profiler.staged.count(StagedCounter::SalvageFailure);
                    state.profiler.staged.failure(StagedFailure::Salvage);
                    state
                        .profiler
                        .staged
                        .timing(StagedTiming::Salvage, elapsed_ns);
                    state
                        .profiler
                        .staged
                        .bytes(StagedBytes::Salvage, progress.copy_bytes);
                    crate::core::lifecycle::write_event(
                        "staged_salvage_failed",
                        serde_json::json!({
                            "reason": "requested_materialization",
                            "output_count": output_count,
                            "copied_bytes": progress.copy_bytes,
                            "elapsed_ns": elapsed_ns,
                        }),
                    );
                } else {
                    state
                        .profiler
                        .staged
                        .timing(StagedTiming::MissMaterialization, elapsed_ns);
                    crate::core::lifecycle::write_event(
                        "staged_materialization_failed",
                        serde_json::json!({
                            "reason": "requested_materialization",
                            "output_count": output_count,
                            "copied_bytes": progress.copy_bytes,
                            "elapsed_ns": elapsed_ns,
                        }),
                    );
                }
                write_session_log(
                    &state.sessions,
                    sid,
                    &format!("[DIAG] staged_materialization_failed: {error}"),
                );
                return Some(Response::Error {
                    message: format!("failed to materialize compiler output: {error}"),
                });
            }
        }
    }

    // Record miss phase profile
    let total_ns = compile_start.elapsed().as_nanos() as u64;
    state.profiler.record_miss(&MissPhases {
        compiler_exec_ns,
        include_scan_ns,
        hash_all_ns,
        artifact_store_ns,
        total_ns,
    });

    // zccache#940: emit the cache-miss sub-phase markers for the inner trace.
    // Every value here is already measured for the miss profile above; we just
    // forward the ones the issue enumerates. No-op unless this compile runs
    // inside an embedded `inner_trace::scope` with ZCCACHE_INNER_TRACE set.
    // `rustc_spawn`/`rustc_wait` reuse the fused prep/process timings — the
    // subprocess spawn+wait+drain is one measured region (`compiler_process_ns`)
    // in `process.rs`; splitting it would need a process-layer refactor the
    // diagnostic doesn't warrant.
    use crate::daemon::server::inner_trace::record_ns;
    record_ns("input_hash", hash_source_ns.saturating_add(hash_headers_ns));
    record_ns("cache_lookup", depgraph_check_ns);
    record_ns("rustc_spawn", compiler_prep_ns);
    record_ns("rustc_wait", compiler_process_ns);
    record_ns("output_read", collect_outputs_ns);
    record_ns("cache_store", artifact_store_ns);

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
