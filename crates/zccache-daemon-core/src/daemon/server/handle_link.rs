//! Ephemeral link/archive request handler.

use super::link_hash::*;
use super::link_process::*;
use super::*;

/// Handle a single-roundtrip ephemeral link/archive request.
///
/// Parses the tool invocation, computes a cache key from the tool binary and
/// all input file hashes, and returns a cached result or runs the real tool.
pub(super) async fn handle_link_ephemeral(
    state: &Arc<SharedState>,
    client_pid: u32,
    tool: &Path,
    args: &[String],
    cwd: &Path,
    env: Option<Vec<(String, String)>>,
) -> Response {
    let _active_request = state.begin_cache_request();
    // Emit hosted cold link/archive phase profiles when requested (#535).
    let profile_enabled = std::env::var_os(CC_MISS_PROFILE_ENV).is_some();
    let link_start = std::time::Instant::now();
    let mut output_read_ns = 0;
    let mut artifact_store_ns = 0;
    let lineage = super::super::lineage::Lineage::current(Some(client_pid), None);
    state.stats.record_link();
    let worktree_root = resolve_worktree_root(cwd, env.as_deref());
    let link_path_remap_key_root = if path_remap_auto_enabled(env.as_deref()) {
        worktree_root.as_deref()
    } else {
        None
    };

    // 1. Parse the tool invocation — try archiver first, then linker.
    let parsed_tool = match parse_link_tool(tool, args) {
        ParseLinkToolOutcome::Cacheable(parsed) => parsed,
        ParseLinkToolOutcome::NonCacheable {
            archive_reason,
            link_reason,
        } => {
            tracing::debug!(
                %archive_reason,
                %link_reason,
                "link non-cacheable, passing through"
            );
            state.stats.record_link_non_cacheable();
            return run_tool_passthrough(
                tool,
                args,
                cwd,
                env,
                &lineage,
                state.depfile_tmpdir.as_path(),
            )
            .await;
        }
    };

    // 2. Non-determinism check: warn but still cache.
    let nd_warning = link_non_determinism_warning(&parsed_tool);

    let parse_args_ns = if profile_enabled {
        link_start.elapsed().as_nanos() as u64
    } else {
        0
    };

    // 3+4. Hash the tool and inputs concurrently; both are file/CPU bound.
    let tool_path = std::path::Path::new(tool);
    let cwd_path = std::path::Path::new(cwd);

    let link_key_plan = build_link_path_remap_key_plan(
        &parsed_tool.cache_relevant_flags,
        cwd_path,
        link_path_remap_key_root,
    );

    let inputs: Vec<NormalizedPath> = parsed_tool
        .input_files
        .iter()
        .chain(link_key_plan.extra_input_files.iter())
        .map(|input| {
            if input.is_absolute() {
                input.clone()
            } else {
                cwd_path.join(input).into()
            }
        })
        .collect();

    // Only cold pure archives may execute while their strong key is discovered;
    // warm hits never launch the tool, and cache hits discard the staged result.
    let output_path = if parsed_tool.output_file.is_absolute() {
        parsed_tool.output_file.clone()
    } else {
        cwd_path.join(&parsed_tool.output_file).into()
    };
    let output_dir = output_path.parent().unwrap_or(cwd_path);
    // Exclude declared sibling inputs from implicit-output scans while still
    // covering undeclared DLLs, PDBs, and wrapper-deployed runtimes.
    let sibling_input_names: std::collections::HashSet<std::ffi::OsString> = inputs
        .iter()
        .filter(|input| input.as_path().parent() == Some(output_dir))
        .filter_map(|input| input.file_name().map(std::ffi::OsStr::to_os_string))
        .collect();

    // Hits share the cold link's output-directory exclusion window.
    let _side_effect_guard = if parsed_tool.is_archive {
        None
    } else {
        Some(
            state
                .link_output_lock(NormalizedPath::from(output_dir))
                .lock_owned()
                .await,
        )
    };
    let hash_cache_is_cold = link_hash_cache_is_cold(state, tool_path, &inputs);
    let archive_hash_speculating = parsed_tool.is_archive && hash_cache_is_cold;
    let mut speculative_archive_plan = None;
    if archive_hash_speculating {
        use crate::daemon::staged_stats::{StagedCounter, StagedTiming};

        let planning_started = std::time::Instant::now();
        state.profiler.staged.count(StagedCounter::PlanAttempted);
        match StagedCompilePlan::archive(state.staging.path(), args, &output_path, cwd_path) {
            StagedPlanOutcome::Enabled(plan) => {
                state.profiler.staged.count(StagedCounter::PlanEnabled);
                state.profiler.staged.timing(
                    StagedTiming::Planning,
                    planning_started.elapsed().as_nanos() as u64,
                );
                speculative_archive_plan = Some(plan);
            }
            StagedPlanOutcome::Unsupported(reason) => {
                state.profiler.staged.count(StagedCounter::PlanUnsupported);
                state.profiler.staged.failure(reason.failure());
                state.profiler.staged.timing(
                    StagedTiming::Planning,
                    planning_started.elapsed().as_nanos() as u64,
                );
            }
            StagedPlanOutcome::Error(error) => {
                state.profiler.staged.count(StagedCounter::PlanError);
                state.profiler.staged.failure(error.reason.failure());
                state.profiler.staged.timing(
                    StagedTiming::Planning,
                    planning_started.elapsed().as_nanos() as u64,
                );
                tracing::warn!(
                    reason = error.reason.id(),
                    error = %error.source,
                    "speculative archive staging plan failed"
                );
            }
        }
    }

    let (hashes, mut speculative_archive, mut prepared_link_miss) =
        if let Some(plan) = speculative_archive_plan {
            let hash_state = Arc::clone(state);
            let hash_tool = tool.to_path_buf();
            let hash_inputs = inputs.clone();
            let hash_task = tokio::task::spawn_blocking(move || {
                hash_link_inputs(&hash_state, &hash_tool, &hash_inputs, profile_enabled)
            });
            let process_started = std::time::Instant::now();
            let archive = async {
                let result = run_archive_tool_passthrough(
                    tool,
                    &plan.rewritten_args,
                    cwd,
                    env.clone(),
                    &lineage,
                )
                .await;
                (result, process_started.elapsed().as_nanos() as u64)
            };
            let ((result, process_ns), hash_result) = tokio::join!(archive, hash_task);
            let hashes = match hash_result {
                Ok(hashes) => hashes,
                Err(error) => {
                    let _ = plan.cleanup();
                    tracing::warn!(%error, "archive hash task failed; retrying without cache");
                    return run_archive_tool_passthrough(tool, args, cwd, env, &lineage).await;
                }
            };
            (
                hashes,
                Some(CompletedSpeculativeArchive {
                    plan,
                    result,
                    process_ns,
                }),
                None,
            )
        } else if should_prepare_link_miss_during_hash(parsed_tool.is_archive, hash_cache_is_cold) {
            let (hashes, prepared) = rayon::join(
                || hash_link_inputs(state, tool_path, &inputs, profile_enabled),
                || {
                    prepare_link_miss(LinkMissPreparationRequest {
                        staging_dir: state.staging.path(),
                        args,
                        output_path: &output_path,
                        secondary_outputs: &parsed_tool.secondary_outputs,
                        cwd: cwd_path,
                        is_directory_bundle: parsed_tool.output_kind
                            == crate::compiler::parse_linker::LinkOutputKind::DirectoryBundle,
                        output_dir,
                        sibling_input_names: &sibling_input_names,
                    })
                },
            );
            (hashes, None, Some(prepared))
        } else {
            (
                hash_link_inputs(state, tool_path, &inputs, profile_enabled),
                None,
                None,
            )
        };
    let hash_wall_ns = hashes.wall_ns;
    let tool_hash_ns = hashes.tool_ns;
    let input_hash_ns = hashes.inputs_ns;
    let tool_hash_opt = hashes.tool_hash;
    let hash_results = hashes.inputs;
    let tool_hash = match tool_hash_opt {
        Some(h) => h,
        None => {
            discard_speculative_archive(&mut speculative_archive);
            tracing::warn!("cannot hash tool {}", tool.display());
            return run_tool_passthrough(
                tool,
                args,
                cwd,
                env,
                &lineage,
                state.depfile_tmpdir.as_path(),
            )
            .await;
        }
    };

    let mut key_builder = crate::hash::link_cache_key::LinkCacheKeyBuilder::new().tool(tool_hash);

    if link_path_remap_key_root.is_some() {
        key_builder = key_builder.flag(LINK_PATH_REMAP_AUTO_KEY_FLAG);
    }
    if link_key_plan.root_specific {
        let root_identity = link_path_remap_key_root
            .map(crate::core::path::normalize_for_key)
            .unwrap_or_default();
        key_builder = key_builder.flag(format!(
            "{LINK_PATH_REMAP_ROOT_SPECIFIC_FLAG}:{root_identity}"
        ));
    }
    for flag in &link_key_plan.flags {
        key_builder = key_builder.flag(flag);
    }

    for (path, hash) in &hash_results {
        let Some(input_hash) = hash else {
            discard_speculative_archive(&mut speculative_archive);
            tracing::warn!("cannot hash input file {}: skipping cache", path.display());
            return run_tool_passthrough(
                tool,
                args,
                cwd,
                env,
                &lineage,
                state.depfile_tmpdir.as_path(),
            )
            .await;
        };
        key_builder = key_builder.input(*input_hash);
    }
    let input_count = parsed_tool.input_files.len() + link_key_plan.extra_input_files.len();
    let cache_key = key_builder.build();
    let key_hex = cache_key.to_hex();

    // 5. Cache lookup
    let t_cache_lookup = profile_enabled.then(std::time::Instant::now);
    if let Some(entry) = lookup_artifact_with_disk_fallback(state, &key_hex) {
        // Load payloads from disk if not already loaded.
        if let Ok(payloads) = ensure_payloads_for_materialization_for_state(state, &entry, &key_hex)
            .map_err(|failure| {
                report_materialization_failure(&state.cache_dir, &key_hex, "link-hit", &failure);
            })
        {
            record_artifact_access(state, &key_hex, &entry, std::time::Instant::now());
            discard_speculative_archive(&mut speculative_archive);
            let names = Arc::clone(&entry.meta.output_names);
            let exit_code = entry.meta.exit_code;
            let stdout = entry.stdout.clone();
            let stderr = entry.stderr.clone();
            tracing::debug!(%key_hex, "link cache hit");
            state.stats.record_link_hit();

            // Write cached output to disk
            let output_path = if parsed_tool.output_file.is_absolute() {
                parsed_tool.output_file.clone()
            } else {
                cwd_path.join(&parsed_tool.output_file).into()
            };
            if parsed_tool.output_kind
                == crate::compiler::parse_linker::LinkOutputKind::DirectoryBundle
            {
                let valid_bundle =
                    payloads.len() == 1 && names.len() == 1 && is_directory_output_name(&names[0]);
                let materialize_started = std::time::Instant::now();
                let observed = valid_bundle
                    .then(|| materialize_directory_payload(&payloads[0], &output_path))
                    .transpose()
                    .ok()
                    .flatten()
                    .map(|copy_bytes| StagedMaterializationStats {
                        copy_count: 1,
                        copy_bytes,
                        ..StagedMaterializationStats::default()
                    });
                payloads.record_staged_lock_timings(&state.profiler.staged);
                drop(payloads);
                if record_staged_hit_materialization(state, 1, materialize_started, observed) {
                    return Response::LinkResult {
                        exit_code,
                        stdout,
                        stderr,
                        cached: true,
                        warning: nd_warning,
                    };
                }
                return run_tool_passthrough(
                    tool,
                    args,
                    cwd,
                    env,
                    &lineage,
                    state.depfile_tmpdir.as_path(),
                )
                .await;
            }
            let targets: Vec<NormalizedPath> = (0..payloads.len())
                .map(|i| {
                    let target: NormalizedPath = if i == 0 {
                        output_path.clone()
                    } else if let Some(secondary) = parsed_tool.secondary_outputs.get(i - 1) {
                        if secondary.is_absolute() {
                            secondary.clone()
                        } else {
                            cwd_path.join(secondary).into()
                        }
                    } else {
                        output_path
                            .parent()
                            .unwrap_or(cwd_path)
                            .join(&names[i])
                            .into()
                    };
                    target
                })
                .collect();
            let has_staged_payload = payloads.iter().any(|payload| {
                matches!(payload, CachedPayload::File(path) if is_staged_artifact_path(path.as_path()))
            });
            let materialize_started = std::time::Instant::now();
            let observed = write_payloads_par_observed(&targets, &payloads);
            payloads.record_staged_lock_timings(&state.profiler.staged);
            drop(payloads);
            if let Err(failure) = &observed {
                report_materialization_failure(&state.cache_dir, &key_hex, "link-hit", failure);
            }
            let write_ok = if has_staged_payload {
                record_staged_hit_materialization(
                    state,
                    targets.len(),
                    materialize_started,
                    observed.ok(),
                )
            } else {
                observed.is_ok()
            };
            if write_ok {
                return Response::LinkResult {
                    exit_code,
                    stdout,
                    stderr,
                    cached: true,
                    warning: nd_warning,
                };
            }
            // Fall through to passthrough if write failed
            return run_tool_passthrough(
                tool,
                args,
                cwd,
                env,
                &lineage,
                state.depfile_tmpdir.as_path(),
            )
            .await;
        }
        // Payloads missing — treat as cache miss, fall through
    }

    let cache_lookup_ns = t_cache_lookup
        .map(|t| t.elapsed().as_nanos() as u64)
        .unwrap_or(0);

    // 6. Cache miss — run the real tool
    tracing::debug!(%key_hex, "link cache miss");
    state.stats.record_link_miss();

    let archive_hash_tool_overlapped = speculative_archive.is_some();
    let (speculative_plan, speculative_result, speculative_process_ns) =
        match speculative_archive.take() {
            Some(speculative) => (
                Some(speculative.plan),
                Some(speculative.result),
                Some(speculative.process_ns),
            ),
            None => (None, None, None),
        };
    let (
        prepared_during_hash,
        prepared_directory_plan_result,
        prepared_staged_plan_result,
        prepared_dir_snapshot_result,
        prepared_planning_ns,
    ) = match prepared_link_miss.take() {
        Some(prepared) => (
            true,
            prepared.directory_plan_result,
            prepared.staged_plan_result,
            prepared.dir_snapshot_result,
            prepared.planning_ns,
        ),
        None => (false, None, None, None, 0),
    };
    use crate::daemon::staged_stats::{StagedBytes, StagedCounter, StagedFailure, StagedTiming};
    let planning_started = std::time::Instant::now();
    let plans_now = speculative_plan.is_none();
    if plans_now {
        state.profiler.staged.count(StagedCounter::PlanAttempted);
    }
    let directory_plan_result = if prepared_during_hash {
        prepared_directory_plan_result
    } else {
        (parsed_tool.output_kind == crate::compiler::parse_linker::LinkOutputKind::DirectoryBundle)
            .then(|| {
                StagedDirectoryPlan::dsymutil(state.staging.path(), args, &output_path, cwd_path)
            })
    };
    let staged_plan_result = if prepared_during_hash {
        prepared_staged_plan_result
    } else {
        (speculative_plan.is_none() && directory_plan_result.is_none()).then(|| {
            if parsed_tool.is_archive && parsed_tool.secondary_outputs.is_empty() {
                StagedCompilePlan::archive(state.staging.path(), args, &output_path, cwd_path)
            } else {
                StagedCompilePlan::link(
                    state.staging.path(),
                    args,
                    &output_path,
                    &parsed_tool.secondary_outputs,
                    cwd_path,
                )
            }
        })
    };
    if plans_now {
        state.profiler.staged.timing(
            StagedTiming::Planning,
            if prepared_during_hash {
                prepared_planning_ns
            } else {
                planning_started.elapsed().as_nanos() as u64
            },
        );
    }
    let mut staged_plan = match (speculative_plan, staged_plan_result) {
        (Some(plan), _) => Some(plan),
        (None, None) => None,
        (None, Some(staged_plan_result)) => match staged_plan_result {
            StagedPlanOutcome::Enabled(plan) => {
                state.profiler.staged.count(StagedCounter::PlanEnabled);
                Some(plan)
            }
            StagedPlanOutcome::Unsupported(reason) => {
                state.profiler.staged.count(StagedCounter::PlanUnsupported);
                state.profiler.staged.failure(reason.failure());
                None
            }
            StagedPlanOutcome::Error(error) => {
                state.profiler.staged.count(StagedCounter::PlanError);
                state.profiler.staged.failure(error.reason.failure());
                tracing::warn!(
                    reason = error.reason.id(),
                    error = %error.source,
                    "link staging plan failed; using legacy path"
                );
                None
            }
        },
    };
    let directory_plan = match directory_plan_result {
        None => None,
        Some(StagedPlanOutcome::Enabled(plan)) => {
            state.profiler.staged.count(StagedCounter::PlanEnabled);
            Some(plan)
        }
        Some(StagedPlanOutcome::Unsupported(reason)) => {
            state.profiler.staged.count(StagedCounter::PlanUnsupported);
            state.profiler.staged.failure(reason.failure());
            None
        }
        Some(StagedPlanOutcome::Error(error)) => {
            state.profiler.staged.count(StagedCounter::PlanError);
            state.profiler.staged.failure(error.reason.failure());
            tracing::warn!(
                reason = error.reason.id(),
                error = %error.source,
                "directory output staging plan failed; using passthrough path"
            );
            None
        }
    };
    let compiler_args = directory_plan.as_ref().map_or_else(
        || {
            staged_plan
                .as_ref()
                .map_or_else(|| args.to_vec(), |plan| plan.rewritten_args.clone())
        },
        |plan| plan.rewritten_args.clone(),
    );
    // Snapshot before non-archive links to detect undeclared sibling outputs.
    let mut side_effects_cacheable = true;
    let mut side_effects_uncacheable_reason = None;
    let dir_snapshot_result = if prepared_during_hash {
        prepared_dir_snapshot_result
    } else if parsed_tool.is_archive || directory_plan.is_some() {
        None
    } else {
        Some(super::super::side_effect::snapshot_directory_excluding(
            output_dir,
            &sibling_input_names,
        ))
    };
    let dir_snapshot = match dir_snapshot_result {
        None => None,
        Some(Ok(snapshot)) => Some(snapshot),
        Some(Err(error)) => {
            side_effects_cacheable = false;
            side_effects_uncacheable_reason =
                Some(format!("failed to snapshot link output directory: {error}"));
            None
        }
    };

    // Extract post-link deploy command from env (if any) BEFORE we consume
    // `env` in the passthrough call. See run_post_link_deploy_hook for rationale.
    let deploy_cmd = env
        .as_ref()
        .and_then(|v| {
            v.iter()
                .find(|(k, _)| k == "ZCCACHE_LINK_DEPLOY_CMD")
                .map(|(_, val)| val.clone())
        })
        .filter(|s| !s.is_empty());
    // Clone env for the hook (we need to re-use it; passthrough consumes env).
    let env_for_hook = env.clone();

    let t_compiler_process = profile_enabled.then(std::time::Instant::now);
    let staged_compiler_started =
        (staged_plan.is_some() || directory_plan.is_some()).then(std::time::Instant::now);
    let result = if let Some(result) = speculative_result {
        result
    } else if parsed_tool.is_archive {
        run_archive_tool_passthrough(tool, &compiler_args, cwd, env, &lineage).await
    } else {
        run_tool_passthrough(
            tool,
            &compiler_args,
            cwd,
            env,
            &lineage,
            state.depfile_tmpdir.as_path(),
        )
        .await
    };

    let compiler_process_ns = speculative_process_ns.unwrap_or_else(|| {
        t_compiler_process
            .map(|t| t.elapsed().as_nanos() as u64)
            .unwrap_or(0)
    });
    if staged_plan.is_some() || directory_plan.is_some() {
        state.profiler.staged.count(StagedCounter::CompilerStaged);
        state.profiler.staged.timing(
            StagedTiming::Compiler,
            speculative_process_ns.unwrap_or_else(|| {
                staged_compiler_started
                    .map(|started| started.elapsed().as_nanos() as u64)
                    .unwrap_or(0)
            }),
        );
    }

    // Run the optional deploy hook before scanning for its sibling outputs.
    if directory_plan.is_none() {
        if let (Some(cmd), Response::LinkResult { exit_code: 0, .. }) = (&deploy_cmd, &result) {
            run_post_link_deploy_hook(cmd, &output_path, env_for_hook.as_deref(), &lineage).await;
        }
    }

    // Archives have no undeclared side effects. Materialize the staged
    // archive before building the in-memory cache entry so disk persistence
    // can run asynchronously, matching the C compile miss path.
    if parsed_tool.is_archive {
        if let Some(plan) = staged_plan.take() {
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
                    state.cache_system.apply_changes(vec![output_path.clone()]);
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
                    return Response::Error {
                        message: format!("failed to materialize archive output: {error}"),
                    };
                }
            }
        }
    }

    // 7. If successful, cache the output
    if let Response::LinkResult {
        exit_code: 0,
        ref stdout,
        ref stderr,
        ..
    } = result
    {
        if let Some(plan) = directory_plan.as_ref() {
            let publication_guard = begin_artifact_publication(state).await;
            let materialized = if publication_guard.is_some() {
                cache_staged_directory_link(state, plan, &key_hex, stdout, stderr)
            } else {
                materialize_uncached_staged_directory_link(state, plan)
            };
            if let Err(error) = materialized {
                return Response::Error {
                    message: format!("failed to materialize staged directory output: {error}"),
                };
            }
            return with_link_warning(result, nd_warning);
        }

        // Preserve cache-index order; any partial capture is uncacheable.
        let primary_name_os = parsed_tool
            .output_file
            .file_name()
            .unwrap_or_default()
            .to_os_string();
        let already_captured: std::collections::HashSet<std::ffi::OsString> =
            std::iter::once(primary_name_os.clone())
                .chain(
                    parsed_tool
                        .secondary_outputs
                        .iter()
                        .filter_map(|s| s.file_name().map(|n| n.to_os_string())),
                )
                .chain(sibling_input_names.iter().cloned())
                .collect();
        // Issue #605 pass 1: matches the pre-link snapshot skip above —
        // archives have no side-effect files to detect.
        let side_effects = if parsed_tool.is_archive || !side_effects_cacheable {
            Vec::new()
        } else if let Some(snapshot) = dir_snapshot.as_ref() {
            match super::super::side_effect::detect_side_effects(
                snapshot,
                output_dir,
                &primary_name_os,
                &already_captured,
            ) {
                Ok(super::super::side_effect::SideEffectScan::Complete(files)) => files,
                Ok(super::super::side_effect::SideEffectScan::Uncacheable { reason }) => {
                    side_effects_cacheable = false;
                    side_effects_uncacheable_reason = Some(reason);
                    Vec::new()
                }
                Err(error) => {
                    side_effects_cacheable = false;
                    side_effects_uncacheable_reason =
                        Some(format!("failed to scan link side effects: {error}"));
                    Vec::new()
                }
            }
        } else {
            side_effects_cacheable = false;
            side_effects_uncacheable_reason =
                Some("link output directory snapshot was unavailable".to_string());
            Vec::new()
        };

        if !side_effects_cacheable {
            tracing::warn!(
                reason = side_effects_uncacheable_reason
                    .as_deref()
                    .unwrap_or("unknown"),
                "successful link is uncacheable because side-effect capture was incomplete"
            );
        }

        if let Some(plan) = staged_plan.as_ref() {
            let unexpected_staged = plan.unexpected_staged_entries().unwrap_or_else(|error| {
                tracing::warn!(%error, "failed to inspect staged linker output set");
                vec![plan.primary_staged().clone()]
            });
            if !side_effects_cacheable || !side_effects.is_empty() || !unexpected_staged.is_empty()
            {
                tracing::warn!(
                    external_count = side_effects.len(),
                    staged_count = unexpected_staged.len(),
                    "undeclared linker side effects invalidate staged publication"
                );
                if let Err(error) = materialize_link_plan_observed(state, plan, None) {
                    return Response::Error {
                        message: format!("failed to materialize staged link output: {error}"),
                    };
                }
                return result;
            }
        }

        let mut read_targets: Vec<(String, std::path::PathBuf, Option<ContentHash>)> =
            Vec::with_capacity(1 + parsed_tool.secondary_outputs.len() + side_effects.len());
        read_targets.push((
            primary_name_os.to_string_lossy().into_owned(),
            staged_plan.as_ref().map_or_else(
                || std::path::PathBuf::from(output_path.as_path()),
                |plan| std::path::PathBuf::from(plan.primary_staged().as_path()),
            ),
            None,
        ));
        for secondary in &parsed_tool.secondary_outputs {
            let sec_path = if secondary.is_absolute() {
                secondary.to_path_buf()
            } else {
                cwd_path.join(secondary)
            };
            let name = secondary
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned();
            let read_path = staged_plan
                .as_ref()
                .and_then(|plan| plan.staged_for_requested(&sec_path))
                .map_or(sec_path.clone(), |path| path.into_path_buf());
            read_targets.push((name, read_path, None));
        }
        for se in &side_effects {
            read_targets.push((
                se.file_name.to_string_lossy().into_owned(),
                std::path::PathBuf::from(se.path.as_path()),
                Some(se.content_hash),
            ));
        }

        // Read all output files in cache-index order.
        let output_read_started = profile_enabled.then(std::time::Instant::now);
        let reads: std::io::Result<Vec<ArtifactOutput>> = read_targets
            .iter()
            .map(|(name, path, expected_hash)| {
                let data = std::fs::read(path)?;
                if let Some(expected_hash) = expected_hash {
                    if crate::hash::hash_bytes(&data) != *expected_hash {
                        return Err(std::io::Error::other(format!(
                            "side-effect changed after scan: {}",
                            path.display()
                        )));
                    }
                }
                Ok(ArtifactOutput {
                    name: name.clone(),
                    payload: ArtifactPayload::Bytes(Arc::new(data)),
                })
            })
            .collect();
        output_read_ns = output_read_started
            .map(|started| started.elapsed().as_nanos() as u64)
            .unwrap_or(0);

        let outputs = match reads {
            Ok(outputs) if side_effects_cacheable => Some(outputs),
            Ok(_) => None,
            Err(error) if staged_plan.is_some() => {
                let _ = staged_plan.as_ref().map(StagedCompilePlan::cleanup);
                return Response::Error {
                    message: format!("successful linker omitted a staged output: {error}"),
                };
            }
            Err(error) => {
                tracing::warn!(%error, "successful link is uncacheable because output capture failed");
                None
            }
        };

        // A successful link with unreadable outputs remains uncacheable.
        if let Some(outputs) = outputs {
            // Log side-effect captures (preserves prior tracing).
            let side_effect_start = 1 + parsed_tool.secondary_outputs.len();
            for o in outputs.iter().skip(side_effect_start) {
                tracing::debug!(file = %o.name, size = o.payload.size_bytes(), "caching side-effect file");
            }

            let artifact = ArtifactData {
                outputs,
                stdout: stdout.clone(),
                stderr: stderr.clone(),
                exit_code: 0,
            };

            // Build CachedArtifact once (no deep copies — all Arc clones).
            let cached = CachedArtifact::from_artifact_data(&artifact);

            // Persist source paths in the background via hardlink/copy (#296),
            // retaining resident bytes for immediate warm hits.
            let artifact_store_started = profile_enabled.then(std::time::Instant::now);
            // Transfer one owned guard through live-map and disk/index publication.
            let Some(guard) = begin_artifact_publication(state).await else {
                if let Some(plan) = staged_plan.as_ref() {
                    if let Err(error) = materialize_link_plan_observed(state, plan, None) {
                        return Response::Error {
                            message: format!("failed to materialize staged link output: {error}"),
                        };
                    }
                }
                return with_link_warning(result, nd_warning);
            };
            let mut publication_guard = Some(guard);
            let cacheable = {
                let artifact_dir = state.artifact_dir.clone();
                let kh = key_hex.clone();
                let persist_meta = cached.meta.clone();
                let source_paths: Vec<NormalizedPath> = read_targets
                    .iter()
                    .map(|(_, path, _)| NormalizedPath::from(path.as_path()))
                    .collect();
                let mut payloads = Vec::with_capacity(artifact.outputs.len());
                for output in &artifact.outputs {
                    let Some(bytes) = output.payload.as_bytes() else {
                        tracing::warn!(file = %output.name, "link output lacks immutable bytes");
                        return result;
                    };
                    payloads.push(Arc::clone(bytes));
                }
                let payload_size: usize = artifact
                    .outputs
                    .iter()
                    .map(|o| o.payload.size_bytes() as usize)
                    .sum();
                state
                    .in_flight_bytes
                    .fetch_add(payload_size, Ordering::Relaxed);
                let guard = InFlightGuard {
                    state: Arc::clone(state),
                    size: payload_size,
                };
                if let Some(plan) = staged_plan.as_ref() {
                    let _guard = guard;
                    match publish_and_materialize_staged_link(
                        state,
                        plan,
                        &kh,
                        persist_meta,
                        &source_paths,
                    ) {
                        Ok(cacheable) => cacheable,
                        Err(error) => {
                            return Response::Error {
                                message: format!(
                                    "failed to materialize staged link output: {error}"
                                ),
                            };
                        }
                    }
                } else {
                    let sem = Arc::clone(&state.persist_semaphore);
                    state.artifacts.insert(key_hex.clone(), cached.clone());
                    let publication_guard_for_task = publication_guard
                        .take()
                        .expect("link publication acquired an owned guard");
                    let index_writer_tx = state.index_writer_tx.clone();
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
                            if persist_artifact_payloads(&artifact_dir, &kh, &payloads).is_ok() {
                                let _ = index_writer_tx
                                    .send(IndexWriterCommand::Insert(kh, persist_meta));
                            }
                        })
                        .await;
                        if let Err(error) = written {
                            tracing::warn!(%error, "link artifact persistence task failed to join");
                        }
                    });
                    true
                }
            };
            artifact_store_ns = artifact_store_started
                .map(|started| started.elapsed().as_nanos() as u64)
                .unwrap_or(0);

            if cacheable && publication_guard.is_some() {
                state.artifacts.insert(key_hex.clone(), cached);
                tracing::debug!(%key_hex, "link artifact cached");
            }
            drop(publication_guard);
        } else if staged_plan.is_some() {
            return Response::Error {
                message: "successful archive omitted its staged output".to_string(),
            };
        }
    }

    let final_response = with_link_warning(result, nd_warning);

    if profile_enabled {
        let total_ns = link_start.elapsed().as_nanos() as u64;
        // Tool-family label derived from the original parse — keep it
        // short for grep/awk friendliness in published bench logs.
        let family = tool
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("link")
            .to_string();
        super::handle_compile::emit_link_miss_profile(super::handle_compile::LinkMissProfile {
            family: family.as_str(),
            input_count,
            total_ns,
            parse_args_ns,
            hash_wall_ns,
            tool_hash_ns,
            input_hash_ns,
            cache_lookup_ns,
            compiler_process_ns,
            output_read_ns,
            artifact_store_ns,
            hash_tool_overlapped: archive_hash_tool_overlapped,
        });
    }

    final_response
}

#[cfg(test)]
#[path = "handle_link_tests.rs"]
mod tests;
