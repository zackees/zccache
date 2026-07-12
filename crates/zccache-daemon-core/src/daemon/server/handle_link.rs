//! Link/passthrough handlers: handle_link_ephemeral, run_tool_passthrough, run_post_link_deploy_hook.

use super::*;

fn publish_and_materialize_staged_link(
    state: &SharedState,
    plan: &StagedCompilePlan,
    key: &str,
    metadata: ArtifactIndex,
    sources: &[NormalizedPath],
) -> std::io::Result<bool> {
    let publication = publish_artifact_paths_observed(state, key, metadata, sources);
    let salvage_reason = publication.as_ref().err().map(|reason| reason.id());
    materialize_link_plan_observed(state, plan, salvage_reason)?;
    Ok(publication.is_ok())
}

fn with_link_warning(result: Response, warning: Option<String>) -> Response {
    match (result, warning) {
        (
            Response::LinkResult {
                exit_code,
                stdout,
                stderr,
                cached,
                ..
            },
            warning @ Some(_),
        ) => Response::LinkResult {
            exit_code,
            stdout,
            stderr,
            cached,
            warning,
        },
        (result, _) => result,
    }
}

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
    // Issue #535: collect phase counters when `ZCCACHE_PROFILE_CC_MISS` is set
    // so the bench / perf-guard logs carry breakdown data for cold link/
    // archive operations (c-static-library-link, cpp-driver-link).
    let profile_enabled = std::env::var_os(CC_MISS_PROFILE_ENV).is_some();
    let link_start = std::time::Instant::now();
    let lineage = super::super::lineage::Lineage::current(Some(client_pid), None);
    use crate::compiler::parse_archiver::{parse_archive_invocation, ParsedArchiveInvocation};
    use crate::compiler::parse_linker::{parse_linker_invocation, ParsedLinkerInvocation};

    state.stats.record_link();
    let worktree_root = resolve_worktree_root(cwd, env.as_deref());
    let link_path_remap_key_root = if path_remap_auto_enabled(env.as_deref()) {
        worktree_root.as_deref()
    } else {
        None
    };

    // 1. Parse the tool invocation — try archiver first, then linker
    struct ParsedTool {
        input_files: Vec<NormalizedPath>,
        output_file: NormalizedPath,
        secondary_outputs: Vec<NormalizedPath>,
        cache_relevant_flags: Vec<String>,
        non_deterministic: bool,
        non_determinism_hint: String,
        // True iff parsed as a pure archiver invocation (ar, lib, llvm-ar).
        // Archive tools only bundle their declared inputs into the output
        // archive — they never deploy runtime DLLs, PDBs, or other side-effect
        // files alongside the output. The pre-link `snapshot_directory` and
        // post-link `detect_side_effects` work is wasted for archive cold-misses.
        is_archive: bool,
        output_kind: crate::compiler::parse_linker::LinkOutputKind,
    }

    let parsed_tool = match parse_archive_invocation(tool.to_str().unwrap_or(""), args) {
        ParsedArchiveInvocation::Cacheable(c) => ParsedTool {
            non_determinism_hint: match c.family {
                crate::compiler::parse_archiver::ArchiverFamily::MsvcLib => "/BREPRO".to_string(),
                _ => "D".to_string(),
            },
            input_files: c.input_files,
            output_file: c.output_file,
            secondary_outputs: Vec::new(),
            cache_relevant_flags: c.cache_relevant_flags,
            non_deterministic: c.non_deterministic,
            is_archive: true,
            output_kind: crate::compiler::parse_linker::LinkOutputKind::File,
        },
        ParsedArchiveInvocation::NonCacheable { reason: ar_reason } => {
            // Try linker parser
            match parse_linker_invocation(tool.to_str().unwrap_or(""), args.to_vec()) {
                ParsedLinkerInvocation::Cacheable(c) => ParsedTool {
                    non_determinism_hint: match c.family {
                        crate::compiler::parse_linker::LinkerFamily::MsvcLink => {
                            "/DETERMINISTIC".to_string()
                        }
                        crate::compiler::parse_linker::LinkerFamily::Dsymutil => {
                            "deterministic input debug information".to_string()
                        }
                        _ => "--build-id=sha1 (avoid --build-id=uuid)".to_string(),
                    },
                    input_files: c.input_files,
                    output_file: c.output_file,
                    secondary_outputs: c.secondary_outputs,
                    cache_relevant_flags: c.cache_relevant_flags,
                    non_deterministic: c.non_deterministic,
                    is_archive: false,
                    output_kind: c.output_kind,
                },
                ParsedLinkerInvocation::NonCacheable {
                    reason: link_reason,
                } => {
                    tracing::debug!(
                        ar_reason = %ar_reason,
                        link_reason = %link_reason,
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
            }
        }
    };

    // 2. Non-determinism check: warn but still cache
    let nd_warning = if parsed_tool.non_deterministic {
        let w = format!(
            "non-deterministic invocation (missing {} flag) — output is cached but may differ from a fresh link",
            parsed_tool.non_determinism_hint
        );
        tracing::warn!(%w);
        Some(w)
    } else {
        None
    };

    let parse_args_ns = if profile_enabled {
        link_start.elapsed().as_nanos() as u64
    } else {
        0
    };

    // 3+4. Hash tool binary and input files concurrently via rayon::join
    // (issue #566). Both phases are CPU-bound blake3 work over mmap'd
    // files. Before this overlap, they ran strictly sequentially —
    // `tool_hash` (~150 MB rustc binary, 10–20 ms) blocked the start of
    // input hashing (50 .rlibs in parallel via #564, also 5–15 ms wall).
    // `rayon::join` reduces the combined wall-clock to ~max(tool_hash,
    // input_hashes) instead of the prior sum.
    let tool_path = std::path::Path::new(tool);
    let cwd_path = std::path::Path::new(cwd);

    let link_key_plan = build_link_path_remap_key_plan(
        &parsed_tool.cache_relevant_flags,
        cwd_path,
        link_path_remap_key_root,
    );

    let inputs: Vec<&NormalizedPath> = parsed_tool
        .input_files
        .iter()
        .chain(link_key_plan.extra_input_files.iter())
        .collect();

    let t_hash = profile_enabled.then(std::time::Instant::now);
    let (tool_hash_opt, hash_results) = rayon::join(
        || hash_file_via_cache(state, tool_path),
        || -> Vec<(NormalizedPath, Option<ContentHash>)> {
            use rayon::prelude::*;
            inputs
                .par_iter()
                .map(|input| {
                    let input_path: NormalizedPath = if input.is_absolute() {
                        (*input).clone()
                    } else {
                        cwd_path.join(input).into()
                    };
                    let hash = hash_file_via_cache(state, &input_path);
                    (input_path, hash)
                })
                .collect()
        },
    );
    // Combined wall-clock budget for the overlapped phases. Reported as
    // both tool_hash_ns and input_hash_ns in the LinkMissProfile for now
    // — they're indistinguishable post-overlap. A future diagnostic
    // change can split them via per-closure timers if needed.
    let combined_hash_ns = t_hash.map(|t| t.elapsed().as_nanos() as u64).unwrap_or(0);
    let tool_hash_ns = combined_hash_ns;
    let input_hash_ns = combined_hash_ns;

    let tool_hash = match tool_hash_opt {
        Some(h) => h,
        None => {
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
    if let Some(mut entry) = lookup_artifact_with_disk_fallback(state, &key_hex) {
        entry.last_used = std::time::Instant::now();
        // Load payloads from disk if not already loaded.
        let loaded = ensure_payloads(&mut entry, &state.artifact_dir, &key_hex).is_some();
        if loaded {
            #[expect(
                clippy::expect_used,
                reason = "ensure_payloads on the preceding line returned Some, which is the contract guaranteeing entry.payloads is now populated"
            )]
            let payloads = Arc::clone(
                entry
                    .payloads
                    .as_ref()
                    .expect("ensure_payloads above returned without error"),
            );
            let names = Arc::clone(&entry.meta.output_names);
            let exit_code = entry.meta.exit_code;
            let stdout = entry.stdout.clone();
            let stderr = entry.stderr.clone();
            drop(entry); // Release DashMap lock

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
            let targets: Vec<(NormalizedPath, NormalizedPath)> = (0..payloads.len())
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
                    let cache_file = state.artifact_dir.join(format!("{key_hex}_{i}"));
                    (target, cache_file)
                })
                .collect();
            let has_staged_payload = payloads.iter().any(|payload| {
                matches!(payload, CachedPayload::File(path) if is_staged_artifact_path(path.as_path()))
            });
            let materialize_started = std::time::Instant::now();
            let observed = write_payloads_par_observed(&targets, &payloads);
            let write_ok = if has_staged_payload {
                record_staged_hit_materialization(
                    state,
                    targets.len(),
                    materialize_started,
                    observed,
                )
            } else {
                observed.is_some()
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

    // Compute output path early (needed for pre-link directory snapshot).
    let output_path = if parsed_tool.output_file.is_absolute() {
        parsed_tool.output_file.clone()
    } else {
        cwd_path.join(&parsed_tool.output_file).into()
    };
    use crate::daemon::staged_stats::{StagedCounter, StagedTiming};
    let planning_started = std::time::Instant::now();
    state.profiler.staged.count(StagedCounter::PlanAttempted);
    let directory_plan_result = (parsed_tool.output_kind
        == crate::compiler::parse_linker::LinkOutputKind::DirectoryBundle)
        .then(|| StagedDirectoryPlan::dsymutil(state.staging.path(), args, &output_path, cwd_path));
    let staged_plan_result = directory_plan_result.is_none().then(|| {
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
    });
    state.profiler.staged.timing(
        StagedTiming::Planning,
        planning_started.elapsed().as_nanos() as u64,
    );
    let staged_plan = match staged_plan_result {
        None => None,
        Some(staged_plan_result) => match staged_plan_result {
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
    let output_dir = output_path.parent().unwrap_or(cwd_path);

    // Snapshot the output directory before the link so we can detect
    // side-effect files (e.g., runtime DLLs deployed by compiler wrappers).
    // Issue #605 pass 1: archive tools (ar, lib, llvm-ar) only bundle their
    // declared inputs into the output archive; they never deploy sibling
    // side-effect files. Skip both pre-link snapshot and post-link rescan
    // for archives — saves a `read_dir` + per-entry `stat` on every archive
    // cold-miss.
    let dir_snapshot = if parsed_tool.is_archive || directory_plan.is_some() {
        super::super::side_effect::DirSnapshot::default()
    } else {
        super::super::side_effect::snapshot_directory(output_dir)
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
    let result = run_tool_passthrough(
        tool,
        &compiler_args,
        cwd,
        env,
        &lineage,
        state.depfile_tmpdir.as_path(),
    )
    .await;

    let compiler_process_ns = t_compiler_process
        .map(|t| t.elapsed().as_nanos() as u64)
        .unwrap_or(0);
    if staged_plan.is_some() || directory_plan.is_some() {
        state.profiler.staged.count(StagedCounter::CompilerStaged);
        state.profiler.staged.timing(
            StagedTiming::Compiler,
            staged_compiler_started
                .map(|started| started.elapsed().as_nanos() as u64)
                .unwrap_or(0),
        );
    }

    // 6b. Invoke optional post-link deploy command on successful link.
    // This handles the case where the compiler driver does NOT auto-deploy
    // runtime DLLs (e.g. a native trampoline that skips the Python wrapper
    // layer where clang-tool-chain's `post_link_dll_deployment` lives).
    // The hook runs BEFORE the side-effect scan so scanning picks up
    // whatever it deployed.
    if directory_plan.is_none() {
        if let (Some(cmd), Response::LinkResult { exit_code: 0, .. }) = (&deploy_cmd, &result) {
            run_post_link_deploy_hook(cmd, &output_path, env_for_hook.as_deref(), &lineage).await;
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
            if let Err(error) = cache_staged_directory_link(state, plan, &key_hex, stdout, stderr) {
                return Response::Error {
                    message: format!("failed to materialize staged directory output: {error}"),
                };
            }
            return with_link_warning(result, nd_warning);
        }

        // Enumerate (name, path) for primary + declared secondaries +
        // detected side-effects in cache-index order so that `outputs[i]`
        // maps to `{key}_i` on disk after the parallel reads below. Missing
        // secondaries and read errors are dropped (preserves the prior
        // serial behavior). Side-effect detection uses the full set of
        // declared output names so it doesn't double-capture them.
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
                .collect();
        // Issue #605 pass 1: matches the pre-link snapshot skip above —
        // archives have no side-effect files to detect.
        let side_effects = if parsed_tool.is_archive {
            Vec::new()
        } else {
            super::super::side_effect::detect_side_effects(
                &dir_snapshot,
                output_dir,
                &primary_name_os,
                &already_captured,
            )
            .unwrap_or_default()
        };

        if let Some(plan) = staged_plan.as_ref() {
            let unexpected_staged = plan.unexpected_staged_entries().unwrap_or_else(|error| {
                tracing::warn!(%error, "failed to inspect staged linker output set");
                vec![plan.primary_staged().as_path().to_path_buf()]
            });
            if !side_effects.is_empty() || !unexpected_staged.is_empty() {
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

        let mut read_targets: Vec<(String, std::path::PathBuf)> =
            Vec::with_capacity(1 + parsed_tool.secondary_outputs.len() + side_effects.len());
        read_targets.push((
            primary_name_os.to_string_lossy().into_owned(),
            staged_plan.as_ref().map_or_else(
                || std::path::PathBuf::from(output_path.as_path()),
                |plan| std::path::PathBuf::from(plan.primary_staged().as_path()),
            ),
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
            read_targets.push((name, read_path));
        }
        for se in &side_effects {
            read_targets.push((
                se.file_name.to_string_lossy().into_owned(),
                std::path::PathBuf::from(se.path.as_path()),
            ));
        }

        // Read all output files; preserve order. Falls back to a serial
        // loop when there's only the primary output — rayon dispatch cost
        // (~150 µs) is comparable to one fs::read for small outputs.
        let reads: Vec<Option<ArtifactOutput>> = if read_targets.len() <= 1 {
            read_targets
                .iter()
                .map(|(name, path)| {
                    std::fs::read(path).ok().map(|data| ArtifactOutput {
                        name: name.clone(),
                        payload: ArtifactPayload::Bytes(Arc::new(data)),
                    })
                })
                .collect()
        } else {
            use rayon::prelude::*;
            read_targets
                .par_iter()
                .map(|(name, path)| {
                    std::fs::read(path).ok().map(|data| ArtifactOutput {
                        name: name.clone(),
                        payload: ArtifactPayload::Bytes(Arc::new(data)),
                    })
                })
                .collect()
        };

        if staged_plan.is_some() && reads.iter().any(Option::is_none) {
            let _ = staged_plan.as_ref().map(StagedCompilePlan::cleanup);
            return Response::Error {
                message: "successful linker omitted a staged output".to_string(),
            };
        }

        // Primary read gates the cache populate. If it fails, the link
        // succeeded but the output file is no longer readable — skip
        // caching, same as the prior serial behavior.
        if reads.first().and_then(|r| r.as_ref()).is_some() {
            let outputs: Vec<ArtifactOutput> = reads.into_iter().flatten().collect();
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

            // Persist to disk in background (meta.clone() is cheap — Arc fields only).
            //
            // Issue #296: hardlink each `read_targets` source path into the cache
            // instead of re-writing the in-memory bytes. We still keep the
            // resident `Bytes` payload in the in-memory cache to serve subsequent
            // warm hits inline; the disk persist routes through
            // `persist_artifact_paths` so the cold-miss disk-write count drops
            // from 2 (compiler + cache) to 1 (compiler + hardlink). Cross-volume
            // case falls back to `std::fs::copy` — identical to prior semantics.
            let cacheable = {
                let artifact_dir = state.artifact_dir.clone();
                let kh = key_hex.clone();
                let persist_meta = cached.meta.clone();
                let source_paths: Vec<NormalizedPath> = read_targets
                    .iter()
                    .map(|(_, p)| NormalizedPath::from(p.as_path()))
                    .collect();
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
                    let state_ref = Arc::clone(state);
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
                            let _ = persist_artifact_paths(&artifact_dir, &kh, &source_paths);
                            (kh, persist_meta)
                        })
                        .await;
                        if let Ok((kh, meta)) = written {
                            let _ = state_ref
                                .index_writer_tx
                                .send(IndexWriterCommand::Insert(kh, meta));
                        }
                    });
                    true
                }
            };

            if cacheable {
                state.artifacts.insert(key_hex.clone(), cached);
                tracing::debug!(%key_hex, "link artifact cached");
            }
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
            tool_hash_ns,
            input_hash_ns,
            cache_lookup_ns,
            compiler_process_ns,
            // output_read_ns + artifact_store_ns are not measured today —
            // the cache populate runs in a background spawn after we
            // return. Tracked as zero here so the published line stays
            // parseable; a follow-up can plumb them through if needed.
            output_read_ns: 0,
            artifact_store_ns: 0,
        });
    }

    final_response
}

#[cfg(test)]
#[path = "handle_link_tests.rs"]
mod tests;
/// Run a tool directly (passthrough) and return a LinkResult response.
///
/// `tmp_dir` is where the synthesized Windows response file lands when the
/// command line exceeds the OS limit. Production callers pass the daemon's
/// `state.depfile_tmpdir` (under the cache root) so the contents are
/// covered by the wrapper's Defender exclusion — see issue #275.
pub(super) async fn run_tool_passthrough(
    tool: &Path,
    args: &[String],
    cwd: &Path,
    env: Option<Vec<(String, String)>>,
    lineage: &super::super::lineage::Lineage,
    tmp_dir: &Path,
) -> Response {
    // Family hint controls response-file dialect (#634); detect from
    // the tool path. Rustc driver linking can land here too (cargo's
    // `--crate-type bin` link step shells out to rustc, which uses
    // file.lines() to parse @rsp). `detect_family` falls back to Gcc
    // for unknown names, which matches the historical behaviour.
    let family_hint = crate::compiler::detect_family(&tool.to_string_lossy());
    let _rsp_guard = match crate::compiler::response_file::write_response_file_if_needed(
        args,
        tmp_dir,
        family_hint,
    ) {
        Ok(guard) => guard,
        Err(e) => {
            return Response::Error {
                message: format!("failed to write response file for {}: {e}", tool.display()),
            };
        }
    };

    let mut cmd = tokio::process::Command::new(tool);
    if let Some(ref rsp) = _rsp_guard {
        cmd.arg(rsp.at_arg());
    } else {
        cmd.args(args);
    }
    cmd.current_dir(cwd);

    apply_client_env(&mut cmd, &env, lineage);

    let priority = CompilePriority::from_client_env(env.as_deref());
    match super::super::process::tokio_command_output_with_priority(&mut cmd, priority).await {
        Ok(output) => Response::LinkResult {
            exit_code: output.status.code().unwrap_or(1),
            stdout: Arc::new(output.stdout),
            stderr: Arc::new(output.stderr),
            cached: false,
            warning: None,
        },
        Err(e) => Response::Error {
            message: format!("failed to run {}: {e}", tool.display()),
        },
    }
}

/// Run an optional post-link deploy command on the link output.
///
/// Invoked when `ZCCACHE_LINK_DEPLOY_CMD` is set in the client's env. The
/// command is expected to be a tool like `clang-tool-chain-libdeploy` that
/// takes one positional argument — the path to the just-linked binary — and
/// deploys any runtime dependencies (runtime DLLs on Windows, libc++/libunwind
/// on Linux/macOS) alongside it.
///
/// This fills a gap that exists when the compiler driver does not auto-deploy
/// runtime dependencies during link (for example a native-compiled trampoline
/// that bypasses the driver's post-link Python hooks). The subsequent
/// `side_effect::detect_side_effects` scan in the caller will then pick up
/// whatever this hook deployed and cache it alongside the primary output.
///
/// Failures are non-fatal: we log a warning and return. The link itself has
/// already succeeded — the build will continue, just without the deployed
/// runtime files cached. Consumers relying on the hook should surface
/// failures at their own layer (e.g. via a separate post-build lint).
///
/// The command is parsed as shell-style (split on whitespace) with one trailing
/// argument appended: the output path. For example:
/// ```text
/// ZCCACHE_LINK_DEPLOY_CMD=clang-tool-chain-libdeploy
/// # runs: clang-tool-chain-libdeploy <output_path>
/// ZCCACHE_LINK_DEPLOY_CMD="clang-tool-chain-libdeploy --quiet"
/// # runs: clang-tool-chain-libdeploy --quiet <output_path>
/// ```
pub(super) async fn run_post_link_deploy_hook(
    cmd_str: &str,
    output_path: &Path,
    env: Option<&[(String, String)]>,
    lineage: &super::super::lineage::Lineage,
) {
    // Split command string on whitespace — first token is the executable,
    // remaining tokens are extra args. We don't support quoted args yet;
    // keep it simple.
    let mut parts = cmd_str.split_whitespace();
    let program = match parts.next() {
        Some(p) => p,
        None => {
            tracing::warn!("ZCCACHE_LINK_DEPLOY_CMD is empty — skipping deploy hook");
            return;
        }
    };
    let extra_args: Vec<&str> = parts.collect();

    let mut cmd = tokio::process::Command::new(program);
    cmd.args(&extra_args);
    cmd.arg(output_path);

    // Run the hook in the output directory so any relative paths the deploy
    // tool emits land sensibly next to the binary.
    if let Some(parent) = output_path.parent() {
        cmd.current_dir(parent);
    }

    // Propagate the client's env — the deploy tool may rely on PATH, TMP,
    // language-specific vars (CLANG_TOOL_CHAIN_*), etc. Spawn-lineage env
    // vars are layered on top so the hook (and anything it spawns) can be
    // attributed back to the daemon.
    if let Some(vars) = env {
        cmd.env_clear();
        for (key, val) in vars {
            if client_env_var_is_safe_to_replay(key) {
                cmd.env(key, val);
            }
        }
    }
    lineage.apply_to_tokio(&mut cmd, env);

    tracing::debug!(
        program = %program,
        output = %output_path.display(),
        "running post-link deploy hook"
    );

    let priority = CompilePriority::from_client_env(env);
    match super::super::process::tokio_command_output_with_priority(&mut cmd, priority).await {
        Ok(out) if out.status.success() => {
            tracing::debug!(
                program = %program,
                "post-link deploy hook succeeded"
            );
        }
        Ok(out) => {
            tracing::warn!(
                program = %program,
                exit_code = out.status.code().unwrap_or(-1),
                stderr = %String::from_utf8_lossy(&out.stderr),
                "post-link deploy hook exited non-zero"
            );
        }
        Err(e) => {
            tracing::warn!(
                program = %program,
                error = %e,
                "post-link deploy hook failed to start"
            );
        }
    }
}
