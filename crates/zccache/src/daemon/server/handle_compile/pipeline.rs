//! Compile request pipeline.

use super::super::*;
use super::error_cache::{compile_failure_stderr, maybe_store_rustc_error_artifact};
use super::hit_branches::{
    try_depgraph_cached_hit, try_fast_hit, try_request_cache_hit, DepgraphHitProbe, FastHitProbe,
    RequestCacheHitProbe,
};
use super::miss_profile::{
    emit_cc_miss_profile, emit_rust_miss_profile, CcMissProfile, RustMissProfile,
};
use super::miss_store::{store_miss_artifact, MissArtifactStoreRequest, MissArtifactStoreStats};
use super::request::CompileRequest;

/// Handle a Compile request: parse args, check depgraph, run compiler or return cached.
pub(super) async fn handle_compile_request(req: CompileRequest<'_>) -> Response {
    let CompileRequest {
        state_arc,
        session_id,
        args,
        cwd,
        compiler_path,
        client_env,
        stdin,
    } = req;
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

    // Ultra-fast request-level cache: skip request preparation when the exact
    // compiler/args/root request still maps to a fresh fast-hit entry.
    if let Some(response) = try_request_cache_hit(RequestCacheHitProbe {
        state,
        sid: &sid,
        compiler_path,
        effective_args: &effective_args,
        cwd,
        request_cache_key_root: &request_cache_key_root,
        client_env: client_env.as_deref(),
        compile_start,
        snap_clock,
    }) {
        return response;
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
    let lineage = crate::daemon::lineage::Lineage::current(
        session_client_pid(state, &sid),
        Some(session_id.into()),
    );

    // Issue #461: the timed phases below feed only `RustMissProfile`, which is
    // emitted only when `ZCCACHE_PROFILE_RUST_MISS` is set. Decide once here
    // whether to capture phase boundaries — otherwise we pay 4 unconditional
    // clock reads (~50ns Linux / ~500ns Windows each) on EVERY request,
    // including warm hits that never reach the miss emitter. Cheap env var
    // probe is ~50ns; tied here for amortization across the rest of the
    // function. Note this is broader than `rust_profile_enabled` at line 268
    // (which also requires `is_rustc`); allowing the env-only check to gate
    // these early phases costs at most a handful of bytes when a non-rustc
    // compile happens with the env set — acceptable since that's the rare
    // diagnostic path.
    let want_rust_miss_profile = std::env::var_os(RUST_MISS_PROFILE_ENV).is_some();

    // Discover system includes for this compiler (cached per compiler path).
    //
    // Issue #517: skip discovery entirely for the rust toolchain. The
    // discovery args (`-v -E -x c++ NUL`) are C/C++-preprocessor flags;
    // rustc / clippy-driver / rustfmt do not understand them and do not have
    // a notion of system includes anyway. Spawning rustc just to capture an
    // error contributes ~30-50 ms (Linux) on every first-after-clear rust
    // compile, which is the dominant share of the 91 ms `rust-workspace-link
    // Cold` overhead measured in `benchmark-stats/latest.json`. Short-circuit
    // to an empty include list — `watch_directories(&[])` is a fast no-op.
    let t_system_includes = want_rust_miss_profile.then(std::time::Instant::now);
    let compiler_priority = CompilePriority::from_client_env(client_env.as_deref());
    let compiler_family = crate::compiler::detect_family(&compiler.to_string_lossy());
    let needs_discovery = compiler_family.needs_system_include_discovery();
    let system_includes = if !needs_discovery {
        Vec::new()
    } else {
        // Issue #541 option B: for the clang family the daemon prefers
        // `clang -###` discovery (~3-5 ms) over the slower `-v -E`
        // (~30-50 ms). Clang's `-###` prints the cc1 command line with
        // every `-internal-isystem` / `-internal-externc-isystem`
        // argument WITHOUT spawning the real preprocessor, so the
        // parser can pull include paths straight out of the printed
        // argv. Gcc / Msvc don't emit this format; they keep using
        // the slow path.
        let use_fast = matches!(compiler_family, crate::compiler::CompilerFamily::Clang);
        let mut cache = state.system_includes.lock().await;
        let lineage_for_probe = lineage.clone();
        cache
            .get_or_discover(&compiler, |c| {
                let disc_args = if use_fast {
                    crate::depgraph::discovery_args_fast()
                } else {
                    crate::depgraph::discovery_args()
                };
                let output = {
                    let mut cmd = std::process::Command::new(c);
                    cmd.args(&disc_args);
                    lineage_for_probe.apply_to_sync(&mut cmd, None);
                    crate::daemon::process::command_output_with_priority(
                        &mut cmd,
                        compiler_priority,
                    )
                };
                match output {
                    Ok(out) => {
                        let stderr = String::from_utf8_lossy(&out.stderr);
                        let mut paths = if use_fast {
                            crate::depgraph::parse_cc1_system_include_output(&stderr)
                        } else {
                            crate::depgraph::parse_system_include_output(&stderr)
                        };
                        // Defensive fall-through: if the fast probe
                        // returned no paths (e.g. an older clang that
                        // doesn't emit `-internal-isystem` flags, or
                        // the binary detected as Clang turned out to
                        // be gcc behind a clang symlink), retry with
                        // the slow `-v -E` discovery. The cache
                        // memoizes the result either way.
                        if use_fast && paths.is_empty() {
                            let slow_args = crate::depgraph::discovery_args();
                            let mut cmd = std::process::Command::new(c);
                            cmd.args(&slow_args);
                            lineage_for_probe.apply_to_sync(&mut cmd, None);
                            if let Ok(out) = crate::daemon::process::command_output_with_priority(
                                &mut cmd,
                                compiler_priority,
                            ) {
                                let stderr = String::from_utf8_lossy(&out.stderr);
                                paths = crate::depgraph::parse_system_include_output(&stderr);
                            }
                        }
                        paths
                    }
                    Err(e) => {
                        tracing::warn!("failed to run compiler for include discovery: {e}");
                        Vec::new()
                    }
                }
            })
            .to_vec()
    };
    let system_includes_ns = t_system_includes
        .map(|t| t.elapsed().as_nanos() as u64)
        .unwrap_or(0);

    // Watch system include directories
    let t_system_watch = want_rust_miss_profile.then(std::time::Instant::now);
    watch_directories(state, &system_includes).await;
    let system_watch_ns = t_system_watch
        .map(|t| t.elapsed().as_nanos() as u64)
        .unwrap_or(0);

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
    // Issue #474: PCH (output ends in .pch / .gch) and MSVC compiles must
    // get a per-worktree cache key — the compiler embeds absolute paths in
    // the artifact in a form the `-ffile-prefix-map` family can't scrub.
    // See `keys::requires_worktree_in_key` for the truth table.
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
    // Issue #489: PCH/MSVC artifacts embed absolute paths the path-remap
    // family can't scrub, so the request-level cache must refuse to share
    // their entries across worktrees regardless of how root-relative the
    // captured paths look. `requires_worktree_in_key` is the single source
    // of truth — we mirror it here into both the context-key salt and the
    // request-cache `worktree_bound` flag so the two cache layers agree.
    let worktree_bound = requires_worktree_in_key(compilation.family, source_mode_for_key);
    let worktree_salt = if worktree_root.is_some() && worktree_bound {
        Some(default_key_root.as_path())
    } else {
        None
    };
    let (ctx, dep_flags, rustc_args_opt, context_key, worktree_equivalent_context) =
        match build_result {
            BuildContextResult::Cc { ctx, dep_flags } => {
                let registration = state.dep_graph.register_with_root_and_salt_result(
                    ctx.clone(),
                    Some(default_key_root.clone()),
                    worktree_salt,
                );
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
                let rustc_externs = rustc_args
                    .externs
                    .iter()
                    .map(|ext| (ext.name.clone(), ext.path.clone()))
                    .collect();
                let registration = state.dep_graph.register_rustc_with_key_and_root_result(
                    key,
                    compat_ctx.clone(),
                    rustc_key_root.clone(),
                    rustc_externs,
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
    let rustc_extern_paths: Vec<NormalizedPath> = rustc_args_opt
        .as_ref()
        .map(|rustc_args| {
            rustc_args
                .externs
                .iter()
                .map(|ext| ext.path.clone())
                .collect()
        })
        .unwrap_or_default();
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

    // Ultra-fast context cache: per-file freshness lets us skip source/header
    // hashing and depgraph checks for a previously verified context.
    if let Some(response) = try_fast_hit(FastHitProbe {
        state,
        sid: &sid,
        context_key,
        source_path: &source_path,
        output_path: &output_path,
        cwd_path: &cwd_path,
        ctx: &ctx,
        compiler_path,
        effective_args: &effective_args,
        cwd,
        request_cache_key_root: &request_cache_key_root,
        client_env: client_env.as_deref(),
        is_rustc,
        worktree_equivalent_context,
        worktree_bound,
        compile_start,
        parse_args_ns,
        build_context_ns,
    }) {
        return response;
    }

    // ── Slow path: hash + depgraph verify ────────────────────────────

    // Skip pre-compile hashing for cold contexts — the depgraph would
    // return Cold without examining any hashes, so the work is wasted.
    // Jump straight to compiler exec.
    let context_is_cold = state.dep_graph.is_cold(&context_key);

    // ── Phase: hash source ───────────────────────────────────────────
    // Issue #468: env-gated sub-phase trace. When ZCCACHE_HIT_TRACE=1, the
    // daemon dumps per-compile sub-phase counts to stderr so the perf
    // harness can break down the dominant "metadata cache (source+hdrs)"
    // phase into source vs headers vs metadata-hit-rate components.
    let hit_trace = std::env::var_os("ZCCACHE_HIT_TRACE").is_some();
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
        let headers_count: usize;
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
            let extern_iter = rustc_extern_paths
                .iter()
                .map(|h| (h, "rustc_extern_hash_fail"));
            let all_paths: Vec<_> = include_iter.chain(force_iter).chain(extern_iter).collect();
            headers_count = all_paths.len();

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

        // Issue #468: ZCCACHE_HIT_TRACE=1 dumps per-compile sub-phase breakdown
        // so the perf harness can decompose the dominant metadata-cache phase.
        // Format is a single line per compile, easy to grep/awk over a session.
        if hit_trace {
            let hdr_avg_us = if headers_count > 0 {
                hash_headers_ns / headers_count as u64 / 1_000
            } else {
                0
            };
            eprintln!(
                "ZCCACHE_HIT_TRACE source_us={} headers_count={} headers_us={} hdr_avg_us={} \
                 source_path={}",
                hash_source_ns / 1_000,
                headers_count,
                hash_headers_ns / 1_000,
                hdr_avg_us,
                source_path.display()
            );
        }

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

    // Issue #353: include `key_root` and `path_remap` state in the diag line so
    // cross-runner cache-miss bisection (two GHA runners hit the same cache via
    // actions/cache@v4 but see 0% hit rate) can diff the per-runner resolution.
    // `path_remap=auto_no_git` exposes the silent-fallback case where
    // `ZCCACHE_PATH_REMAP=auto` was requested but `find_git_root` returned None.
    write_session_log(
        &state.sessions,
        &sid,
        &format!(
            "[DIAG] depgraph_check: {} -> {} ctx={} verdict={} reason={} key_root={} path_remap={}",
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
            default_key_root.display(),
            diag_path_remap_state(client_env.as_deref(), worktree_root.is_some()),
        ),
    );
    match verdict {
        crate::depgraph::CacheVerdict::Hit { artifact_key }
        | crate::depgraph::CacheVerdict::SourceChanged { artifact_key } => {
            let artifact_key_hex = artifact_key.hash().to_hex();
            if let Some(response) = try_depgraph_cached_hit(DepgraphHitProbe {
                state,
                sid: &sid,
                context_key,
                artifact_key_hex: &artifact_key_hex,
                source_path: &source_path,
                output_path: &output_path,
                cwd_path: &cwd_path,
                ctx: &ctx,
                compiler_path,
                effective_args: &effective_args,
                cwd,
                request_cache_key_root: &request_cache_key_root,
                client_env: client_env.as_deref(),
                is_rustc,
                worktree_equivalent_context,
                worktree_bound,
                compile_start,
                parse_args_ns,
                build_context_ns,
                hash_source_ns,
                hash_headers_ns,
                depgraph_check_ns,
            }) {
                return response;
            }
            // Artifact key computed but no artifact stored yet, or payload delivery
            // failed. Fall through to compile.
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

    // Issue #532: kick off hashing of pre-known inputs (source +
    // rustc_extern_paths) on a blocking thread, in parallel with the
    // rustc spawn. The 50-rlib externs of a workspace link dominate
    // hash_all_ns (~64 ms on a 4-core CI runner); overlapping them with
    // the ~38 ms rustc exec hides most of that cost. Late-arriving
    // include paths (from rustc's dep-info) are hashed post-compile and
    // merged with the pre-hash result. Skip for non-rustc compilers —
    // they don't have a known-ahead extern list, and their cold hash_all
    // is small anyway.
    let pre_hash_task: Option<tokio::task::JoinHandle<HashMap<NormalizedPath, ContentHash>>> =
        if is_rustc && !rustc_extern_paths.is_empty() {
            let pre_state = Arc::clone(state_arc);
            let pre_source = source_path.clone();
            let pre_externs = rustc_extern_paths.clone();
            let pre_clock = snap_clock;
            Some(tokio::task::spawn_blocking(move || {
                use rayon::prelude::*;
                let all_paths: Vec<&NormalizedPath> = std::iter::once(&pre_source)
                    .chain(pre_externs.iter())
                    .collect();
                all_paths
                    .par_iter()
                    .filter_map(|path| {
                        let hash_path = resolve_pch_source(path, &pre_state.pch_source_map)
                            .unwrap_or_else(|| (*path).clone());
                        hash_file(&pre_state.cache_system, &hash_path, pre_clock)
                            .ok()
                            .map(|h| ((*path).clone(), h))
                    })
                    .collect()
            }))
        } else {
            None
        };

    let result = crate::daemon::process::tokio_command_output_with_priority(
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
        if let Some(rustc_args) = rustc_args_opt.as_ref() {
            if let Some(artifact_key_hex) = maybe_store_rustc_error_artifact(
                state,
                &context_key,
                &source_path,
                &cwd_path,
                &ctx,
                rustc_args,
                &stdout,
                &stderr,
                exit_code,
                snap_clock,
            ) {
                write_session_log(
                    &state.sessions,
                    &sid,
                    &format!(
                        "[CACHED_ERROR_STORE] {} key={}",
                        source_path.display(),
                        &artifact_key_hex[..8]
                    ),
                );
            }
        }
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
            for ext in &rustc_extern_paths {
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
                std::iter::once(&source_path)
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
                    &sid,
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
            sid: &sid,
            context_key: &context_key,
            source_path: &source_path,
            output_path: &output_path,
            scan_result,
            hash_map: &hash_map,
            output_data,
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
