//! Link/passthrough handlers: handle_link_ephemeral, run_tool_passthrough, run_post_link_deploy_hook.

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
    let lineage = crate::lineage::Lineage::current(Some(client_pid), None);
    use zccache_compiler::parse_archiver::{parse_archive_invocation, ParsedArchiveInvocation};
    use zccache_compiler::parse_linker::{parse_linker_invocation, ParsedLinkerInvocation};

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
    }

    let parsed_tool = match parse_archive_invocation(tool.to_str().unwrap_or(""), args) {
        ParsedArchiveInvocation::Cacheable(c) => ParsedTool {
            non_determinism_hint: match c.family {
                zccache_compiler::parse_archiver::ArchiverFamily::MsvcLib => "/BREPRO".to_string(),
                _ => "D".to_string(),
            },
            input_files: c.input_files,
            output_file: c.output_file,
            secondary_outputs: Vec::new(),
            cache_relevant_flags: c.cache_relevant_flags,
            non_deterministic: c.non_deterministic,
        },
        ParsedArchiveInvocation::NonCacheable { reason: ar_reason } => {
            // Try linker parser
            match parse_linker_invocation(tool.to_str().unwrap_or(""), args.to_vec()) {
                ParsedLinkerInvocation::Cacheable(c) => ParsedTool {
                    non_determinism_hint: match c.family {
                        zccache_compiler::parse_linker::LinkerFamily::MsvcLink => {
                            "/DETERMINISTIC".to_string()
                        }
                        _ => "--build-id=sha1 (avoid --build-id=uuid)".to_string(),
                    },
                    input_files: c.input_files,
                    output_file: c.output_file,
                    secondary_outputs: c.secondary_outputs,
                    cache_relevant_flags: c.cache_relevant_flags,
                    non_deterministic: c.non_deterministic,
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

    // 3. Hash the tool binary
    let tool_path = std::path::Path::new(tool);
    let tool_hash = match hash_file_via_cache(state, tool_path) {
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

    // 4. Hash all input files
    let cwd_path = std::path::Path::new(cwd);
    let link_key_plan = build_link_path_remap_key_plan(
        &parsed_tool.cache_relevant_flags,
        cwd_path,
        link_path_remap_key_root,
    );
    let mut key_builder = zccache_monocrate::hash::link_cache_key::LinkCacheKeyBuilder::new().tool(tool_hash);

    if link_path_remap_key_root.is_some() {
        key_builder = key_builder.flag(LINK_PATH_REMAP_AUTO_KEY_FLAG);
    }
    if link_key_plan.root_specific {
        let root_identity = link_path_remap_key_root
            .map(zccache_monocrate::core::path::normalize_for_key)
            .unwrap_or_default();
        key_builder = key_builder.flag(format!(
            "{LINK_PATH_REMAP_ROOT_SPECIFIC_FLAG}:{root_identity}"
        ));
    }
    for flag in &link_key_plan.flags {
        key_builder = key_builder.flag(flag);
    }

    for input in parsed_tool
        .input_files
        .iter()
        .chain(link_key_plan.extra_input_files.iter())
    {
        let input_path = if input.is_absolute() {
            input.clone()
        } else {
            cwd_path.join(input).into()
        };
        let input_hash = match hash_file_via_cache(state, &input_path) {
            Some(h) => h,
            None => {
                tracing::warn!(
                    "cannot hash input file {}: skipping cache",
                    input_path.display()
                );
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
        key_builder = key_builder.input(input_hash);
    }

    let cache_key = key_builder.build();
    let key_hex = cache_key.to_hex();

    // 5. Cache lookup
    if let Some(mut entry) = lookup_artifact_with_disk_fallback(state, &key_hex) {
        entry.last_used = std::time::Instant::now();
        // Load payloads from disk if not already loaded.
        let loaded = ensure_payloads(&mut entry, &state.artifact_dir, &key_hex).is_some();
        if loaded {
            let payloads = Arc::clone(entry.payloads.as_ref().unwrap());
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
            let targets: Vec<(NormalizedPath, NormalizedPath)> = (0..payloads.len())
                .map(|i| {
                    let target: NormalizedPath = if payloads.len() == 1 {
                        output_path.clone()
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
            let write_ok = write_payloads_par(&targets, &payloads);
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

    // 6. Cache miss — run the real tool
    tracing::debug!(%key_hex, "link cache miss");
    state.stats.record_link_miss();

    // Compute output path early (needed for pre-link directory snapshot).
    let output_path = if parsed_tool.output_file.is_absolute() {
        parsed_tool.output_file.clone()
    } else {
        cwd_path.join(&parsed_tool.output_file).into()
    };
    let output_dir = output_path.parent().unwrap_or(cwd_path);

    // Snapshot the output directory before the link so we can detect
    // side-effect files (e.g., runtime DLLs deployed by compiler wrappers).
    let dir_snapshot = crate::side_effect::snapshot_directory(output_dir);

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

    let result = run_tool_passthrough(
        tool,
        args,
        cwd,
        env,
        &lineage,
        state.depfile_tmpdir.as_path(),
    )
    .await;

    // 6b. Invoke optional post-link deploy command on successful link.
    // This handles the case where the compiler driver does NOT auto-deploy
    // runtime DLLs (e.g. a native trampoline that skips the Python wrapper
    // layer where clang-tool-chain's `post_link_dll_deployment` lives).
    // The hook runs BEFORE the side-effect scan so scanning picks up
    // whatever it deployed.
    if let (Some(cmd), Response::LinkResult { exit_code: 0, .. }) = (&deploy_cmd, &result) {
        run_post_link_deploy_hook(cmd, &output_path, env_for_hook.as_deref(), &lineage).await;
    }

    // 7. If successful, cache the output
    if let Response::LinkResult {
        exit_code: 0,
        ref stdout,
        ref stderr,
        ..
    } = result
    {
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
        let side_effects = crate::side_effect::detect_side_effects(
            &dir_snapshot,
            output_dir,
            &primary_name_os,
            &already_captured,
        )
        .unwrap_or_default();

        let mut read_targets: Vec<(String, std::path::PathBuf)> =
            Vec::with_capacity(1 + parsed_tool.secondary_outputs.len() + side_effects.len());
        read_targets.push((
            primary_name_os.to_string_lossy().into_owned(),
            std::path::PathBuf::from(output_path.as_path()),
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
            read_targets.push((name, sec_path));
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
            {
                let artifact_dir = state.artifact_dir.clone();
                let kh = key_hex.clone();
                let persist_meta = cached.meta.clone();
                // link-ephemeral always reads outputs into memory above, so
                // every payload is the `Bytes` variant in practice. The
                // match keeps us forward-compatible if a `Path` variant
                // ever reaches this site (materialise by read; degraded but
                // correct).
                let payloads: Vec<Arc<Vec<u8>>> = artifact
                    .outputs
                    .iter()
                    .filter_map(|o| match &o.payload {
                        ArtifactPayload::Bytes(b) => Some(Arc::clone(b)),
                        ArtifactPayload::Path(p) => std::fs::read(p.as_path()).ok().map(Arc::new),
                    })
                    .collect();
                let payload_size: usize = payloads.iter().map(|p| p.len()).sum();
                state
                    .in_flight_bytes
                    .fetch_add(payload_size, Ordering::Relaxed);
                let guard = InFlightGuard {
                    state: Arc::clone(state),
                    size: payload_size,
                };
                let sem = Arc::clone(&state.persist_semaphore);
                let state_ref = Arc::clone(state);
                tokio::spawn(async move {
                    let _permit = sem.acquire().await.unwrap();
                    let written = tokio::task::spawn_blocking(move || {
                        let _guard = guard;
                        let _ = persist_artifact_payloads(&artifact_dir, &kh, &payloads);
                        (kh, persist_meta)
                    })
                    .await;
                    if let Ok((kh, meta)) = written {
                        let _ = state_ref.index_writer_tx.send((kh, meta));
                    }
                });
            }

            state.artifacts.insert(key_hex.clone(), cached);
            tracing::debug!(%key_hex, "link artifact cached");
        }
    }

    match (result, nd_warning) {
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
    lineage: &crate::lineage::Lineage,
    tmp_dir: &Path,
) -> Response {
    let _rsp_guard =
        match zccache_compiler::response_file::write_response_file_if_needed(args, tmp_dir) {
            Ok(guard) => guard,
            Err(e) => {
                return Response::Error {
                    message: format!("failed to write response file for {}: {e}", tool.display()),
                };
            }
        };

    let mut cmd = std::process::Command::new(tool);
    if let Some(ref rsp) = _rsp_guard {
        cmd.arg(rsp.at_arg());
    } else {
        cmd.args(args);
    }
    cmd.current_dir(cwd);

    apply_client_env_sync(&mut cmd, env.as_deref(), lineage);

    let priority = CompilePriority::from_client_env(env.as_deref());
    match crate::process::command_output_with_priority(&mut cmd, priority) {
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
    lineage: &crate::lineage::Lineage,
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

    let mut cmd = std::process::Command::new(program);
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
    apply_client_env_sync(&mut cmd, env, lineage);

    tracing::debug!(
        program = %program,
        output = %output_path.display(),
        "running post-link deploy hook"
    );

    let priority = CompilePriority::from_client_env(env);
    match crate::process::command_output_with_priority(&mut cmd, priority) {
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
