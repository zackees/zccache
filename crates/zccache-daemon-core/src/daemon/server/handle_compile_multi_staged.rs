//! All-or-nothing private execution for cache-missed multi-source units.

use super::args::filter_multi_source_args;
use super::*;
use crate::daemon::{lineage::Lineage, process};

struct StagedMiss {
    unit_index: usize,
    source_path: NormalizedPath,
    context_key: ContextKey,
    ctx: Box<CompileContext>,
    input_snapshot: InputSnapshot,
    plan: StagedMultiUnitPlan,
    scan_result: Option<crate::depgraph::ScanResult>,
}

struct PublishedMiss {
    source_path: NormalizedPath,
    context_key: ContextKey,
    plan: StagedMultiUnitPlan,
    cache_entry: Option<(String, CachedArtifact)>,
    graph_update: Option<(
        crate::depgraph::ScanResult,
        HashMap<NormalizedPath, ContentHash>,
    )>,
    dep_dirs: Vec<NormalizedPath>,
    artifact_bytes: u64,
    validation_clock: Clock,
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
    key_root: &NormalizedPath,
    client_env: &Option<Vec<(String, String)>>,
    all_stdout: &mut Vec<u8>,
    all_stderr: &mut Vec<u8>,
) -> Option<Response> {
    let mut requested_outputs = HashSet::new();
    for compilation in compilations {
        let output = if compilation.output_file.is_absolute() {
            compilation.output_file.clone()
        } else {
            cwd.join(&compilation.output_file)
        };
        if !requested_outputs.insert(output) {
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
            input_snapshot,
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
            input_snapshot: input_snapshot.clone(),
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
            if miss.plan.msvc_syntax {
                crate::compiler::CompilerFamily::Msvc
            } else {
                compilations[miss.unit_index].family
            },
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
        let mut scan_result =
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
        let mut tracked: HashSet<NormalizedPath> =
            miss.input_snapshot.hashes.keys().cloned().collect();
        tracked.insert(miss.source_path.clone());
        tracked.extend(scan_result.resolved.iter().cloned());
        tracked.extend(miss.ctx.force_includes.iter().cloned());
        let tracked_paths: Vec<NormalizedPath> = tracked.into_iter().collect();
        let resolved: HashSet<NormalizedPath> = scan_result.resolved.iter().cloned().collect();
        scan_result.resolved.extend(
            miss.input_snapshot
                .hashes
                .keys()
                .filter(|path| {
                    path.as_path() != miss.source_path.as_path() && !resolved.contains(*path)
                })
                .cloned(),
        );
        state.cache_system.register_tracked(&tracked_paths);
        let dep_dirs = tracked_paths
            .iter()
            .filter_map(|path| path.parent().map(NormalizedPath::from))
            .collect();
        let current_clock = state.cache_system.current_clock();
        let hash_map: HashMap<NormalizedPath, ContentHash> = {
            use rayon::prelude::*;
            tracked_paths
                .par_iter()
                .filter_map(|path| {
                    hash_file(&state.cache_system, path, current_clock)
                        .ok()
                        .map(|hash| (path.clone(), hash))
                })
                .collect()
        };
        let stable = miss.input_snapshot.stable(state, &tracked_paths, &hash_map);
        let mut cache_entry = None;
        let mut graph_update = None;
        if stable {
            let file_hashes: Vec<(NormalizedPath, ContentHash)> = hash_map
                .iter()
                .map(|(path, hash)| (path.clone(), *hash))
                .collect();
            let artifact_key = crate::depgraph::context::compute_artifact_key_normalized_with_root(
                &miss.context_key,
                &file_hashes,
                Some(key_root.as_path()),
            );
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
            let (stdout, stderr) = &ordered_output[miss.unit_index];
            let metadata = ArtifactIndex::new(
                output_names,
                output_sizes,
                Arc::new(stdout.clone()),
                Arc::new(stderr.clone()),
                0,
            );
            cache_entry = Some((key, CachedArtifact::from_index(metadata)));
            graph_update = Some((scan_result, hash_map));
        }
        published.push(PublishedMiss {
            source_path: miss.source_path,
            context_key: miss.context_key,
            plan: miss.plan,
            cache_entry,
            graph_update,
            dep_dirs,
            artifact_bytes,
            validation_clock: current_clock,
        });
    }

    let mut changed_outputs = Vec::new();
    let mut dependency_directories = HashSet::new();
    for miss in &published {
        if let Err(error) = materialize_multi_plan_observed(state, &miss.plan, None) {
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
        dependency_directories.extend(miss.dep_dirs.iter().cloned());
    }

    let mut completed = Vec::with_capacity(published.len());
    for mut miss in published {
        if let Some((key, metadata)) = miss
            .cache_entry
            .as_ref()
            .map(|(key, cached)| (key.clone(), cached.meta.clone()))
        {
            let staged_paths: Vec<NormalizedPath> = miss
                .plan
                .outputs
                .iter()
                .map(|output| output.staged.clone())
                .collect();
            if let Err(reason) =
                publish_artifact_paths_observed(state, &key, metadata, &staged_paths)
            {
                record_prepublication_salvage_success(state, miss.plan.outputs.len(), reason.id());
                miss.cache_entry = None;
                miss.graph_update = None;
            } else if let Some((scan_result, hash_map)) = miss.graph_update.take() {
                if let Some((_, cached)) = miss.cache_entry.as_ref() {
                    state.artifacts.insert(key.clone(), cached.clone());
                }
                let get_hash = |path: &Path| hash_map.get(&NormalizedPath::new(path)).copied();
                let committed_key = state
                    .dep_graph
                    .load()
                    .update(&miss.context_key, scan_result, get_hash)
                    .map(|artifact_key| artifact_key.hash().to_hex());
                if committed_key.as_deref() != Some(key.as_str()) {
                    tracing::error!(
                        expected = %key,
                        actual = ?committed_key,
                        "staged multi-source artifact key changed during depgraph commit"
                    );
                    let mut invalid = HashSet::from([key.clone()]);
                    invalid.extend(committed_key);
                    state.dep_graph.load().invalidate_artifact_keys(&invalid);
                    state.artifacts.remove(&key);
                    miss.cache_entry = None;
                }
            }
        }
        if let Err(error) = miss.plan.cleanup() {
            tracing::warn!(%error, "failed to clean staged multi-source workspace");
        }
        completed.push((
            miss.source_path,
            miss.context_key,
            miss.cache_entry,
            miss.artifact_bytes,
            miss.validation_clock,
        ));
    }
    for (source, context_key, cache_entry, artifact_bytes, validation_clock) in completed {
        state.stats.record_miss(0, artifact_bytes);
        record_session_stat(&state.sessions, sid, move |stats| {
            stats.record_miss(source, artifact_bytes);
        });
        if let Some((key, cached)) = cache_entry {
            state.artifacts.insert(key.clone(), cached);
            state.fast_hit_cache.insert(
                context_key,
                FastHitEntry {
                    clock: validation_clock,
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
