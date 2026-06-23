//! Compile request pipeline orchestrator.
//!
//! The per-request pipeline (parse, build context, hash, depgraph check,
//! hit-branch dispatch, miss exec, store) was originally a single 1.2k LOC
//! function. The implementation is now split per phase under this directory:
//!
//! - `system_includes.rs` — per-compiler system include discovery + watch
//! - `hash_verify.rs` — source + header hashing and depgraph verdict
//! - `compile_exec.rs` — depfile/response-file prep + compiler spawn
//! - `store_outcome.rs` — successful-compile post path (scan, hash all, store, profiles)
//!
//! This module is the orchestrator: it threads local timings + per-phase
//! results through the early-return tree and finally returns the `Response`.

mod compile_exec;
mod hash_verify;
mod store_outcome;
mod system_includes;

use super::super::*;
use super::cached_hit::{
    materialize_cached_compile_hit, CachedHitMaterializeRequest, CachedHitPhases,
};
use super::error_cache::{compile_failure_stderr, maybe_store_rustc_error_artifact};
use super::hit_branches::{
    try_depgraph_cached_hit, try_fast_hit, try_request_cache_hit, DepgraphHitProbe, FastHitProbe,
    RequestCacheHitProbe,
};
use super::request::CompileRequest;

use compile_exec::{run_compile_exec, CompileExecOutcome, CompileExecRequest, CompileExecResult};
use hash_verify::{hash_and_verify, HashSourceOutcome, HashVerifyInput, HashVerifyOutcome};
use store_outcome::{store_successful_compile, StoreOutcomeRequest};
use system_includes::{discover_system_includes, SystemIncludesOutcome};

const DEPGRAPH_STARTUP_WAIT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

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
    // function. Note this is broader than `rust_profile_enabled` below (which
    // also requires `is_rustc`); allowing the env-only check to gate these
    // early phases costs at most a handful of bytes when a non-rustc
    // compile happens with the env set — acceptable since that's the rare
    // diagnostic path.
    let want_rust_miss_profile = std::env::var_os(RUST_MISS_PROFILE_ENV).is_some();

    let compiler_priority = CompilePriority::from_client_env(client_env.as_deref());
    let SystemIncludesOutcome {
        includes: system_includes,
        system_includes_ns,
        system_watch_ns,
    } = discover_system_includes(
        state,
        &compiler,
        &lineage,
        compiler_priority,
        want_rust_miss_profile,
    )
    .await;

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
            wait_for_startup_depgraph_load(state, &sid).await;
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
    wait_for_startup_depgraph_load(state, &sid).await;

    let t1 = std::time::Instant::now();
    let env_slice = client_env.as_deref().unwrap_or(&[]);
    let build_result = build_compile_context_async(
        &compilation,
        &cwd_path,
        &system_includes,
        env_slice,
        &state.compiler_hash_cache,
    )
    .await;
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
    let (
        ctx,
        dep_flags,
        rustc_args_opt,
        context_key,
        rustc_metadata_compat_key,
        worktree_equivalent_context,
    ) = match build_result {
        BuildContextResult::Cc { ctx, dep_flags } => {
            let registration = state.dep_graph.load().register_with_root_and_salt_result(
                ctx.clone(),
                Some(default_key_root.clone()),
                worktree_salt,
            );
            (
                ctx,
                dep_flags,
                None,
                registration.key,
                None,
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
            let compat_key =
                rustc_ctx.check_metadata_compat_key_with_root(rustc_key_root.as_deref());
            let published_compat_key = rustc_args
                .emit_types
                .iter()
                .any(|emit| emit == "link")
                .then_some(compat_key)
                .flatten();
            let rustc_externs = rustc_args
                .externs
                .iter()
                .map(|ext| (ext.name.clone(), ext.path.clone()))
                .collect();
            let registration = state
                .dep_graph
                .load()
                .register_rustc_with_key_and_root_result(
                    key,
                    compat_ctx.clone(),
                    rustc_key_root.clone(),
                    rustc_externs,
                    published_compat_key,
                );
            (
                compat_ctx,
                UserDepFlags::default(),
                Some(rustc_args),
                registration.key,
                compat_key,
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
    let rustc_current_externs: Vec<(String, NormalizedPath)> = rustc_args_opt
        .as_ref()
        .map(|rustc_args| {
            rustc_args
                .externs
                .iter()
                .map(|ext| (ext.name.clone(), ext.path.clone()))
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
        // Issue #643: only the C/C++ path has on-disk depfile contracts
        // we cache. Rustc emits its own dep-info via a different
        // mechanism handled by the rustc miss/hit paths.
        dep_flags: if is_rustc { None } else { Some(&dep_flags) },
        is_rustc,
        worktree_equivalent_context,
        worktree_bound,
        compile_start,
        parse_args_ns,
        build_context_ns,
    })
    .await
    {
        return response;
    }

    // ── Slow path: hash + depgraph verify ────────────────────────────
    let HashVerifyOutcome {
        hash_map,
        hash_source_ns,
        hash_headers_ns,
        depgraph_check_ns,
        verdict,
        diag_reason,
    } = match hash_and_verify(HashVerifyInput {
        state,
        sid: &sid,
        context_key,
        source_path: &source_path,
        ctx: &ctx,
        rustc_extern_paths: &rustc_extern_paths,
        snap_clock,
    }) {
        HashSourceOutcome::Ready(outcome) => outcome,
        HashSourceOutcome::Fallback => {
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
    };

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
        crate::depgraph::CacheVerdict::Hit { artifact_key } => {
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
                // Issue #643: see `FastHitProbe` site above for rationale.
                dep_flags: if is_rustc { None } else { Some(&dep_flags) },
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
                record_session_stat(&state.sessions, &sid, |t| {
                    t.record_depgraph_hit_artifact_hit();
                });
                return response;
            }
            // Artifact key computed but no artifact stored yet, or payload delivery
            // failed. Fall through to compile.
            record_session_stat(&state.sessions, &sid, |t| {
                t.record_depgraph_hit_artifact_miss();
            });
            write_session_log(
                &state.sessions,
                &sid,
                &format!("[DIAG] artifact_not_found: key={artifact_key_hex}"),
            );
            // Drop the stale depgraph entry pointing at the missing
            // payload so the next lookup for this source does not
            // re-fire the same wasted-hit dance. `invalidate_missing_
            // depgraph_artifact` logs `cleared=N` so the
            // regression test in `daemon_rustc_restore_test.rs` can
            // assert the expected cleared count of 1; an earlier
            // version of this branch invalidated inline too, racing
            // the helper to `cleared=0` and breaking the test.
            invalidate_missing_depgraph_artifact(state, &sid, &artifact_key_hex);
        }
        crate::depgraph::CacheVerdict::SourceChanged { artifact_key } => {
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
                // Issue #643: see `FastHitProbe` site above for rationale.
                dep_flags: if is_rustc { None } else { Some(&dep_flags) },
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
            record_session_stat(&state.sessions, &sid, |t| {
                t.record_depgraph_other_miss(&diag_reason);
            });
            write_session_log(
                &state.sessions,
                &sid,
                &format!("[DIAG] artifact_not_found: key={artifact_key_hex}"),
            );
        }
        crate::depgraph::CacheVerdict::Cold => {
            record_session_stat(&state.sessions, &sid, |t| {
                if diag_reason == "cold_skip" {
                    t.record_depgraph_cold_skip();
                } else {
                    t.record_depgraph_other_miss(&diag_reason);
                }
            });
            // Need to compile and scan includes
        }
        crate::depgraph::CacheVerdict::HeadersChanged { .. }
        | crate::depgraph::CacheVerdict::NeedsPreprocessor => {
            record_session_stat(&state.sessions, &sid, |t| {
                t.record_depgraph_other_miss(&diag_reason);
            });
            // Need to compile and scan includes
        }
    }

    // Cache miss — invalidate fast-hit cache for this context
    if is_rustc {
        if let (Some(compat_key), Some(rustc_args)) =
            (rustc_metadata_compat_key, rustc_args_opt.as_deref())
        {
            let check_style_request = !rustc_args.emit_types.iter().any(|emit| emit == "link");
            if check_style_request {
                let compat_hash_map = std::cell::RefCell::new(hash_map);
                let get_hash = |p: &Path| {
                    let path = NormalizedPath::new(p);
                    if let Some(hash) = compat_hash_map.borrow().get(&path).copied() {
                        return Some(hash);
                    }
                    let hash = hash_file(&state.cache_system, &path, snap_clock).ok()?;
                    compat_hash_map.borrow_mut().insert(path, hash);
                    Some(hash)
                };
                let is_fresh = |p: &Path| {
                    let path = NormalizedPath::new(p);
                    !state
                        .cache_system
                        .journal()
                        .changed_since(&path, snap_clock)
                };
                let (compat_verdict, compat_reason, actual_context_key) = state
                    .dep_graph
                    .load()
                    .check_rustc_metadata_compat_diagnostic(
                        &compat_key,
                        &rustc_current_externs,
                        is_fresh,
                        get_hash,
                    );
                write_session_log(
                    &state.sessions,
                    &sid,
                    &format!(
                        "[DIAG] rustc_emit_compat_check: {} -> {} compat_ctx={} verdict={} reason={}",
                        source_path.display(),
                        output_path.display(),
                        &compat_key.hash().to_hex()[..8],
                        match &compat_verdict {
                            crate::depgraph::CacheVerdict::Hit { .. } => "Hit",
                            crate::depgraph::CacheVerdict::SourceChanged { .. } => "SourceChanged",
                            crate::depgraph::CacheVerdict::HeadersChanged { .. } => "HeadersChanged",
                            crate::depgraph::CacheVerdict::Cold => "Cold",
                            crate::depgraph::CacheVerdict::NeedsPreprocessor => "NeedsPreprocessor",
                        },
                        compat_reason,
                    ),
                );
                if let crate::depgraph::CacheVerdict::Hit { artifact_key } = compat_verdict {
                    let artifact_key_hex = artifact_key.hash().to_hex();
                    pending_writes::await_pending(
                        &state.pending_cache_writes,
                        &artifact_key_hex,
                        pending_writes::PENDING_WAIT_TIMEOUT,
                    )
                    .await;
                    let requested_outputs =
                        rustc_expected_output_paths(rustc_args, output_path.as_path(), cwd);
                    if let Some(response) =
                        materialize_cached_compile_hit(CachedHitMaterializeRequest {
                            state,
                            sid: &sid,
                            artifact_key_hex: &artifact_key_hex,
                            source_path: &source_path,
                            output_path: &output_path,
                            secondary_output_dir: output_path
                                .parent()
                                .unwrap_or(cwd_path.as_path())
                                .into(),
                            current_depfile_dest: None,
                            compile_start,
                            hit_label: "HIT_RUSTC_EMIT_COMPAT",
                            cached_error_label: "CACHED_ERROR_RUSTC_EMIT_COMPAT",
                            record_compilation: false,
                            downgrade_output_metadata: true,
                            mtime_floor_paths: request_cache_input_paths(
                                state,
                                actual_context_key.as_ref().unwrap_or(&context_key),
                                &source_path,
                                &ctx,
                            ),
                            rustc_metadata_compat_outputs: Some(requested_outputs),
                            phases: CachedHitPhases {
                                parse_args_ns,
                                build_context_ns,
                                hash_source_ns,
                                hash_headers_ns,
                                depgraph_check_ns,
                                request_cache_lookup_ns: 0,
                                cross_root_validate_ns: 0,
                            },
                        })
                    {
                        record_session_stat(&state.sessions, &sid, |t| {
                            t.record_depgraph_hit_artifact_hit();
                        });
                        return response;
                    }
                    record_session_stat(&state.sessions, &sid, |t| {
                        t.record_depgraph_hit_artifact_miss();
                    });
                    write_session_log(
                        &state.sessions,
                        &sid,
                        &format!(
                            "[DIAG] rustc_emit_compat_artifact_not_found: key={artifact_key_hex}"
                        ),
                    );
                }
            }
        }
    }

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

    // ── Phase: compiler exec ────────────────────────────────────────
    let exec_result = run_compile_exec(CompileExecRequest {
        state_arc,
        compiler: &compiler,
        effective_args: &effective_args,
        cwd,
        cwd_path: &cwd_path,
        source_path: &source_path,
        output_path: &output_path,
        compilation: &compilation,
        dep_flags: &dep_flags,
        rustc_args_opt: rustc_args_opt.as_deref(),
        rustc_extern_paths: &rustc_extern_paths,
        is_rustc,
        client_env: &client_env,
        lineage: &lineage,
        compile_start,
        snap_clock,
    })
    .await;
    let CompileExecOutcome {
        exit_code,
        stdout,
        stderr,
        depfile_strategy,
        show_includes_scan,
        pre_hash_task,
        compiler_priority_decision,
        pre_exec_ns,
        break_outputs_ns,
        compiler_process_ns,
        compiler_exec_ns,
        compiler_prep_ns,
        post_exec_ns,
    } = match exec_result {
        CompileExecResult::Ok(outcome) => outcome,
        CompileExecResult::Error(resp) => return resp,
    };

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
        if let Some(response) = store_successful_compile(StoreOutcomeRequest {
            state_arc,
            sid: &sid,
            context_key: &context_key,
            source_path: &source_path,
            output_path: &output_path,
            cwd_path: &cwd_path,
            ctx: &ctx,
            compilation: &compilation,
            rustc_args_opt: rustc_args_opt.as_deref(),
            rustc_extern_paths: &rustc_extern_paths,
            is_rustc,
            rust_profile_enabled,
            rust_profile_mode,
            stdout: Arc::clone(&stdout),
            stderr: Arc::clone(&stderr),
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
        })
        .await
        {
            return response;
        }
    }

    Response::CompileResult {
        exit_code,
        stdout,
        stderr,
        cached: false,
    }
}

fn invalidate_missing_depgraph_artifact(
    state: &SharedState,
    sid: &SessionId,
    artifact_key_hex: &str,
) {
    let mut stale_keys = std::collections::HashSet::with_capacity(1);
    stale_keys.insert(artifact_key_hex.to_string());
    let cleared = state.dep_graph.load().invalidate_artifact_keys(&stale_keys);
    write_session_log(
        &state.sessions,
        sid,
        &format!("[DIAG] depgraph_invalidate_artifact: key={artifact_key_hex} cleared={cleared}"),
    );
}

async fn wait_for_startup_depgraph_load(state: &SharedState, sid: &SessionId) {
    if state.dep_graph_load_complete.load(Ordering::Acquire) {
        return;
    }

    write_session_log(
        &state.sessions,
        sid,
        "[DIAG] depgraph_load_pending: waiting before compile context registration",
    );

    let deadline = tokio::time::sleep(DEPGRAPH_STARTUP_WAIT_TIMEOUT);
    tokio::pin!(deadline);
    loop {
        let notified = state.dep_graph_load_notify.notified();
        if state.dep_graph_load_complete.load(Ordering::Acquire) {
            return;
        }
        tokio::select! {
            () = notified => {
                if state.dep_graph_load_complete.load(Ordering::Acquire) {
                    return;
                }
            }
            () = &mut deadline => {
                write_session_log(
                    &state.sessions,
                    sid,
                    "[WARN] depgraph_load_pending: timed out; continuing with current graph",
                );
                return;
            }
        }
    }
}
