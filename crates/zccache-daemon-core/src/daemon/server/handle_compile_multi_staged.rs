//! All-or-nothing private execution for cache-missed multi-source units.

use super::args::filter_multi_source_args;
use super::*;
use crate::daemon::{lineage::Lineage, process};

struct StagedMiss {
    unit_index: usize,
    source_path: NormalizedPath,
    context_key: ContextKey,
    ctx: Box<CompileContext>,
    plan: StagedMultiUnitPlan,
    scan_result: Option<crate::depgraph::ScanResult>,
}

struct PublishedMiss {
    source_path: NormalizedPath,
    context_key: ContextKey,
    plan: StagedMultiUnitPlan,
    cache_entry: Option<(String, CachedArtifact)>,
    salvage_reason: Option<&'static str>,
    dep_dirs: Vec<NormalizedPath>,
    artifact_bytes: u64,
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn try_handle_staged_misses(
    state: &Arc<SharedState>,
    sid: &SessionId,
    compiler: &NormalizedPath,
    compilations: &[crate::compiler::CacheableCompilation],
    original_args: &[String],
    source_indices: &[usize],
    unit_results: &[UnitCacheResult],
    cwd: &NormalizedPath,
    client_env: &Option<Vec<(String, String)>>,
    snap_clock: Clock,
    all_stdout: &mut Vec<u8>,
    all_stderr: &mut Vec<u8>,
) -> Option<Response> {
    let case_insensitive_outputs = compilations
        .iter()
        .any(|compilation| compilation.family == crate::compiler::CompilerFamily::Msvc)
        || crate::compiler::parse_msvc::looks_like_msvc_args(original_args);
    let mut requested_outputs = HashSet::new();
    for compilation in compilations {
        let output = compilation.output_file.to_string_lossy();
        let identity = if case_insensitive_outputs {
            output.to_ascii_lowercase()
        } else {
            output.into_owned()
        };
        if !requested_outputs.insert(identity) {
            use crate::daemon::staged_stats::{StagedCounter, StagedFailure};
            state.profiler.staged.count(StagedCounter::PlanAttempted);
            state.profiler.staged.count(StagedCounter::PlanUnsupported);
            state
                .profiler
                .staged
                .failure(StagedFailure::PlanOutputNameCollision);
            return None;
        }
    }
    let mut misses = Vec::new();
    for (index, unit) in unit_results.iter().enumerate() {
        let UnitCacheResult::Miss {
            source_path,
            output_path,
            context_key,
            ctx,
        } = unit
        else {
            continue;
        };
        let selected_source_index = source_indices[index];
        let retained_source_indices = HashSet::from([selected_source_index]);
        let unit_args =
            filter_multi_source_args(original_args, source_indices, &retained_source_indices);

        use crate::daemon::staged_stats::{StagedCounter, StagedTiming};
        let planning_started = std::time::Instant::now();
        state.profiler.staged.count(StagedCounter::PlanAttempted);
        let outcome = StagedMultiUnitPlan::build(
            state.staging.path(),
            compilations[index].family,
            unit_args,
            output_path,
            cwd,
        );
        state.profiler.staged.timing(
            StagedTiming::Planning,
            planning_started.elapsed().as_nanos() as u64,
        );
        let plan = match outcome {
            StagedPlanOutcome::Enabled(plan) => {
                state.profiler.staged.count(StagedCounter::PlanEnabled);
                plan
            }
            StagedPlanOutcome::Unsupported(reason) => {
                state.profiler.staged.count(StagedCounter::PlanUnsupported);
                state.profiler.staged.failure(reason.failure());
                return None;
            }
            StagedPlanOutcome::Error(error) => {
                state.profiler.staged.count(StagedCounter::PlanError);
                state.profiler.staged.failure(error.reason.failure());
                tracing::warn!(
                    reason = error.reason.id(),
                    error = %error.source,
                    "multi-source staging plan failed; using legacy batch path"
                );
                return None;
            }
        };
        misses.push(StagedMiss {
            unit_index: index,
            source_path: source_path.clone(),
            context_key: *context_key,
            ctx: ctx.clone(),
            plan,
            scan_result: None,
        });
    }
    if misses.is_empty() {
        return None;
    }

    let mut ordered_output: Vec<(Vec<u8>, Vec<u8>)> = unit_results
        .iter()
        .map(|unit| match unit {
            UnitCacheResult::Hit { stdout, stderr, .. } => {
                (stdout.as_ref().clone(), stderr.as_ref().clone())
            }
            UnitCacheResult::Miss { .. } => (Vec::new(), Vec::new()),
        })
        .collect();
    let mut failed_exit_code = None;

    let lineage = Lineage::current(session_client_pid(state, sid), Some(sid.to_string()));
    for miss in &mut misses {
        let compiler_started = std::time::Instant::now();
        let mut compiler_args = miss.plan.rewritten_args.clone();
        if miss.plan.msvc_syntax
            && !compiler_args
                .iter()
                .any(|arg| arg.eq_ignore_ascii_case("/showIncludes"))
        {
            compiler_args.push("/showIncludes".to_string());
        }
        let rsp_guard = match crate::compiler::response_file::write_response_file_if_needed(
            &compiler_args,
            &state.depfile_tmpdir,
            compilations[0].family,
        ) {
            Ok(guard) => guard,
            Err(error) => {
                return Some(Response::Error {
                    message: format!("failed to write staged multi-source response file: {error}"),
                });
            }
        };
        let mut command = tokio::process::Command::new(compiler);
        if let Some(response_file) = &rsp_guard {
            command.arg(response_file.at_arg()).current_dir(cwd);
        } else {
            command.args(&compiler_args).current_dir(cwd);
        }
        apply_client_env(&mut command, client_env, &lineage);
        let priority = CompilePriority::from_client_env(client_env.as_deref());
        let output = match process::tokio_command_output_with_priority(&mut command, priority).await
        {
            Ok(output) => output,
            Err(error) => {
                return Some(Response::Error {
                    message: format!("failed to run staged multi-source compiler: {error}"),
                });
            }
        };
        state
            .profiler
            .staged
            .count(crate::daemon::staged_stats::StagedCounter::CompilerStaged);
        state.profiler.staged.timing(
            crate::daemon::staged_stats::StagedTiming::Compiler,
            compiler_started.elapsed().as_nanos() as u64,
        );
        let exit_code = output.status.code().unwrap_or(-1);
        let stderr = if miss.plan.msvc_syntax {
            let (scan_result, filtered_stderr) =
                crate::depgraph::show_includes::parse_show_includes(
                    &output.stderr,
                    &miss.source_path,
                    cwd,
                );
            miss.scan_result = Some(scan_result);
            filtered_stderr
        } else {
            output.stderr
        };
        ordered_output[miss.unit_index] = (output.stdout, stderr);
        if exit_code != 0 && failed_exit_code.is_none() {
            failed_exit_code = Some(exit_code);
        }
    }
    all_stdout.clear();
    all_stderr.clear();
    for (stdout, stderr) in &ordered_output {
        all_stdout.extend_from_slice(stdout);
        all_stderr.extend_from_slice(stderr);
    }
    if let Some(exit_code) = failed_exit_code {
        state.stats.record_error();
        record_session_stat(&state.sessions, sid, |stats| stats.record_error());
        return Some(Response::CompileResult {
            exit_code,
            stdout: Arc::new(std::mem::take(all_stdout)),
            stderr: Arc::new(std::mem::take(all_stderr)),
            cached: false,
        });
    }

    let mut validated_sizes = Vec::with_capacity(misses.len());
    for miss in &misses {
        match miss.plan.validated_output_sizes() {
            Ok(output_sizes) => validated_sizes.push(output_sizes),
            Err(error) => {
                return Some(Response::Error {
                    message: format!("successful multi-source compiler output is invalid: {error}"),
                });
            }
        }
    }

    let mut published = Vec::with_capacity(misses.len());
    for (miss, output_sizes) in misses.into_iter().zip(validated_sizes) {
        let artifact_bytes = output_sizes.iter().sum();
        let scan_result =
            miss.scan_result
                .unwrap_or_else(|| {
                    match crate::depgraph::depfile::parse_depfile_path(
                        &miss.plan.depfile,
                        &miss.source_path,
                        cwd,
                    ) {
                        Ok(result) => result,
                        Err(error) => {
                            tracing::warn!(
                                source = %miss.source_path.display(),
                                %error,
                                "staged multi-source depfile parse failed; using recursive scan"
                            );
                            crate::depgraph::scanner::scan_recursive(
                                &miss.source_path,
                                &miss.ctx.include_search,
                            )
                        }
                    }
                });
        let tracked_paths: Vec<NormalizedPath> = std::iter::once(miss.source_path.clone())
            .chain(scan_result.resolved.iter().cloned())
            .collect();
        state.cache_system.register_tracked(&tracked_paths);
        let dep_dirs = tracked_paths
            .iter()
            .filter_map(|path| path.parent().map(NormalizedPath::from))
            .collect();
        let hash_map: HashMap<NormalizedPath, ContentHash> = {
            use rayon::prelude::*;
            tracked_paths
                .par_iter()
                .filter_map(|path| {
                    hash_file(&state.cache_system, path, snap_clock)
                        .ok()
                        .map(|hash| (path.clone(), hash))
                })
                .collect()
        };
        let get_hash = |path: &Path| hash_map.get(&NormalizedPath::new(path)).copied();
        let artifact_key = state
            .dep_graph
            .load()
            .update(&miss.context_key, scan_result, get_hash);
        let mut cache_entry = None;
        let mut salvage_reason = None;
        if let Some(artifact_key) = artifact_key {
            let key = artifact_key.hash().to_hex();
            let output_names = miss
                .plan
                .outputs
                .iter()
                .map(|output| {
                    output
                        .requested
                        .strip_prefix(cwd)
                        .unwrap_or(output.requested.as_path())
                        .to_string_lossy()
                        .into_owned()
                })
                .collect();
            let empty = Arc::new(Vec::new());
            let metadata = ArtifactIndex::new(
                output_names,
                output_sizes,
                Arc::clone(&empty),
                Arc::clone(&empty),
                0,
            );
            let staged_paths: Vec<NormalizedPath> = miss
                .plan
                .outputs
                .iter()
                .map(|output| output.staged.clone())
                .collect();
            match publish_artifact_paths_observed(state, &key, metadata.clone(), &staged_paths) {
                Ok(_) => cache_entry = Some((key, CachedArtifact::from_index(metadata))),
                Err(reason) => salvage_reason = Some(reason.id()),
            }
        }
        published.push(PublishedMiss {
            source_path: miss.source_path,
            context_key: miss.context_key,
            plan: miss.plan,
            cache_entry,
            salvage_reason,
            dep_dirs,
            artifact_bytes,
        });
    }

    let mut changed_outputs = Vec::new();
    let mut dependency_directories = HashSet::new();
    let mut completed = Vec::with_capacity(published.len());
    for miss in published {
        if let Err(error) = materialize_multi_plan_observed(state, &miss.plan, miss.salvage_reason)
        {
            return Some(Response::Error {
                message: format!("failed to materialize multi-source output: {error}"),
            });
        }
        changed_outputs.extend(
            miss.plan
                .outputs
                .iter()
                .map(|output| output.requested.clone()),
        );
        dependency_directories.extend(miss.dep_dirs);
        completed.push((
            miss.source_path,
            miss.context_key,
            miss.cache_entry,
            miss.artifact_bytes,
        ));
    }
    for (source, context_key, cache_entry, artifact_bytes) in completed {
        state.stats.record_miss(0, artifact_bytes);
        record_session_stat(&state.sessions, sid, move |stats| {
            stats.record_miss(source, artifact_bytes);
        });
        if let Some((key, cached)) = cache_entry {
            state.artifacts.insert(key.clone(), cached);
            state.fast_hit_cache.insert(
                context_key,
                FastHitEntry {
                    clock: state.cache_system.current_clock(),
                    artifact_key_hex: key,
                    cached_at: std::time::Instant::now(),
                },
            );
        }
    }
    watch_directories(
        state,
        &dependency_directories.into_iter().collect::<Vec<_>>(),
    )
    .await;
    state.cache_system.apply_changes(changed_outputs);
    Some(Response::CompileResult {
        exit_code: 0,
        stdout: Arc::new(std::mem::take(all_stdout)),
        stderr: Arc::new(std::mem::take(all_stderr)),
        cached: false,
    })
}
