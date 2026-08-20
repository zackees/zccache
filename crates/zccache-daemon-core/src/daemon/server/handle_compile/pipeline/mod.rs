//! Compile request pipeline orchestrator.
//!
//! The per-request pipeline (parse, build context, hash, depgraph check,
//! hit-branch dispatch, miss exec, store) was originally a single 1.2k LOC
//! function. The implementation is now split per phase under this directory:
//!
//! - `request_prep.rs` — validation, Dylint/time-macro prep, invocation parse
//! - `system_includes.rs` — per-compiler system include discovery + watch
//! - `hash_verify.rs` — source + header hashing and depgraph verdict
//! - `compile_exec.rs` — depfile/response-file prep + compiler spawn
//! - `store_outcome.rs` — successful-compile post path (hash all, store, profiles)
//! - `store_outcome_scan.rs` — depfile/include scan collection and fallback policy
//!
//! This module is the orchestrator: it threads local timings + per-phase
//! results through the early-return tree and finally returns the `Response`.

mod compile_exec;
mod hash_verify;
mod request_prep;
mod store_outcome;
mod system_includes;
mod time_macros;

use super::super::*;
use super::cached_hit::{
    materialize_cached_compile_hit, CachedHitFailure, CachedHitMaterializeRequest, CachedHitPhases,
};
use super::error_cache::maybe_store_rustc_error_artifact;
use super::hit_branches::{
    try_depgraph_cached_hit, try_fast_hit, try_request_cache_hit, DepgraphHitProbe, FastHitProbe,
    RequestCacheHitProbe,
};
use super::request::CompileRequest;

use compile_exec::{run_compile_exec, CompileExecOutcome, CompileExecRequest, CompileExecResult};
use hash_verify::{hash_and_verify, HashSourceOutcome, HashVerifyInput, HashVerifyOutcome};
use request_prep::{
    begin_prepared_request, discover_request_system_includes, invalidate_missing_depgraph_artifact,
    parse_single_compile_request, prepare_request_arguments, record_dylint_input_hash,
    wait_for_startup_depgraph_load, ParsedRequestArguments, ParsedSingleRequest,
};
use store_outcome::{store_successful_compile, StoreOutcomeRequest};
use system_includes::SystemIncludesOutcome;

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
    let ParsedRequestArguments {
        sid,
        client_env,
        dependency_mode,
        worktree_root,
        effective_args,
        request_cache_key_root,
    } = match prepare_request_arguments(
        state,
        session_id,
        compiler_path,
        args,
        cwd,
        client_env,
        &stdin,
    )
    .await
    {
        Ok(prepared) => prepared,
        Err(response) => return response,
    };

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
    })
    .await
    {
        return response;
    }

    let (compiler, lineage, want_rust_miss_profile) =
        begin_prepared_request(state, &sid, session_id, compiler_path);

    let SystemIncludesOutcome {
        includes: system_includes,
        system_includes_ns,
        system_watch_ns,
        ..
    } = match discover_request_system_includes(
        state,
        &sid,
        &compiler,
        &lineage,
        want_rust_miss_profile,
        args,
        cwd,
        &client_env,
        &stdin,
    )
    .await
    {
        Ok(outcome) => outcome,
        Err(response) => return response,
    };

    state.sessions.touch(&sid);

    let ParsedSingleRequest {
        compilation,
        cwd_path,
        source_path,
        output_path,
        system_includes,
        client_env,
        stdin,
        parse_args_ns,
    } = match parse_single_compile_request(
        state_arc,
        state,
        &sid,
        &compiler,
        &effective_args,
        args,
        cwd,
        worktree_root.as_ref(),
        system_includes,
        client_env,
        stdin,
        compile_start,
        dependency_mode,
    )
    .await
    {
        Ok(parsed) => parsed,
        Err(response) => return response,
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
    let native_cpu_family = match &build_result {
        BuildContextResult::Cc { ctx, .. } => ctx
            .flags
            .iter()
            .chain(&ctx.unknown_flags)
            .any(|flag| crate::depgraph::native_cpu::is_cxx_native_cpu_flag(flag))
            .then_some("cxx"),
        BuildContextResult::Rustc { rustc_ctx, .. } => rustc_ctx
            .codegen_flags
            .iter()
            .any(|flag| crate::depgraph::native_cpu::is_rustc_native_cpu_flag(flag))
            .then_some("rustc"),
    };
    if let Some(compiler_family) = native_cpu_family {
        tracing::info!(
            event = "native_flag_host_salted",
            compiler_family,
            "native CPU selection makes this compile key host-specific"
        );
        crate::core::lifecycle::write_event(
            crate::core::lifecycle::EVENT_NATIVE_FLAG_HOST_SALTED,
            serde_json::json!({ "compiler_family": compiler_family }),
        );
    }
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
    let worktree_bound = cc_requires_worktree_salt(
        compilation.family,
        source_mode_for_key,
        &effective_args,
        default_key_root.as_path(),
    );
    let worktree_salt = if worktree_bound {
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
        registered_context_state,
    ) = match build_result {
        BuildContextResult::Cc { mut ctx, dep_flags } => {
            dependency_mode.apply_to_cc_context(&mut ctx, &dep_flags);
            DependencyDiscoveryMode::apply_user_depfile_content_to_cc_context(
                &mut ctx,
                &dep_flags,
                &compilation.original_args,
            );
            let registration = state.dep_graph.load().register_with_root_and_salt_result(
                ctx.clone(),
                Some(default_key_root.clone()),
                worktree_salt,
            );
            (
                ctx,
                dep_flags,
                None,
                registration.map_key,
                None,
                registration.rebased_from_equivalent_root,
                registration.state,
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
            if crate::compiler::is_dylint_driver(&compiler_path.to_string_lossy()) {
                super::super::inner_trace::record_ns(
                    match rust_remap_gate(&rustc_args.remap_path_prefixes, worktree_root.as_ref()) {
                        RustRemapGate::Ok => "dylint_remap_gate_ok",
                        RustRemapGate::Missing => "dylint_remap_gate_missing",
                        RustRemapGate::OldOutsideRoot => "dylint_remap_gate_old_outside_root",
                        RustRemapGate::Malformed => "dylint_remap_gate_malformed",
                    },
                    0,
                );
                super::super::inner_trace::record_ns(
                    if rustc_key_root.is_some() {
                        "dylint_key_root_present"
                    } else {
                        "dylint_key_root_missing"
                    },
                    0,
                );
                record_dylint_input_hash("context_compiler", rustc_ctx.compiler_hash.as_bytes());
                record_dylint_input_hash(
                    "context_source",
                    rustc_ctx.source_file.as_path().to_string_lossy().as_bytes(),
                );
                record_dylint_input_hash(
                    "context_codegen",
                    format!("{:?}", rustc_ctx.codegen_flags).as_bytes(),
                );
                record_dylint_input_hash(
                    "context_externs",
                    format!("{:?}", rustc_ctx.extern_crates).as_bytes(),
                );
                record_dylint_input_hash(
                    "context_unknown",
                    format!("{:?}", rustc_ctx.unknown_flags).as_bytes(),
                );
                record_dylint_input_hash(
                    "context_remap",
                    format!("{:?}", rustc_ctx.remap_path_prefixes).as_bytes(),
                );
                record_dylint_input_hash(
                    "context_env",
                    format!("{:?}", rustc_ctx.env_vars).as_bytes(),
                );
            }
            let key = rustc_ctx.context_key_with_root(rustc_key_root.as_deref());
            let compat_key =
                rustc_ctx.check_metadata_compat_key_with_root(rustc_key_root.as_deref());
            let compat_map_key = compat_key.map(|compat_key| {
                crate::depgraph::DepGraph::rustc_metadata_compat_map_key(
                    compat_key,
                    &compat_ctx.source_file,
                    rustc_key_root.as_ref(),
                )
            });
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
                registration.map_key,
                compat_map_key,
                registration.rebased_from_equivalent_root,
                registration.state,
            )
        }
    };
    let is_rustc = rustc_args_opt.is_some();
    record_context_key(&context_key);
    // Make cross-worktree registration decisions visible in the existing
    // opt-in inner trace. A `context_not_found` journal reason
    // alone cannot distinguish "no equivalent context was indexed" from
    // "an equivalent context was found but was itself cold". The six
    // categorical phase names keep the disabled path allocation-free and
    // let a production trace prove that boundary without logging source
    // paths or compiler arguments.
    super::super::inner_trace::record_context_registration(
        worktree_equivalent_context,
        registered_context_state,
    );
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
    //
    // Issue #401 plumbing: the `hash_map` extracted here is fed back
    // into the cc/cpp miss path's `StoreOutcomeRequest.pre_hashed` so
    // the post-compile parallel hash skips files we already hashed
    // here. The rustc miss path uses `pre_hash_task` (a background
    // join handle) instead and ignores `pre_hashed`. The binding is
    // marked `mut` so the rustc compat-check branch below can take it
    // by `mem::take` without forcing a clone.
    let HashVerifyOutcome {
        mut hash_map,
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
        client_env: client_env.as_deref(),
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
    record_miss_reason(match &verdict {
        crate::depgraph::CacheVerdict::Hit { .. } => miss_reason::NO_ARTIFACT_FOR_KEY,
        crate::depgraph::CacheVerdict::SourceChanged { .. }
        | crate::depgraph::CacheVerdict::HeadersChanged { .. }
        | crate::depgraph::CacheVerdict::NeedsPreprocessor => {
            miss_reason::INPUT_FINGERPRINT_MISMATCH
        }
        crate::depgraph::CacheVerdict::Cold => miss_reason::CONTEXT_NOT_FOUND,
    });
    match verdict {
        crate::depgraph::CacheVerdict::Hit { artifact_key } => {
            let artifact_key_hex = artifact_key.hash().to_hex();
            let failure = match try_depgraph_cached_hit(DepgraphHitProbe {
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
            })
            .await
            {
                Ok(response) => {
                    record_session_stat(&state.sessions, &sid, |t| {
                        t.record_depgraph_hit_artifact_hit();
                    });
                    return response;
                }
                Err(failure) => failure,
            };
            // Artifact key computed but no artifact stored yet, or payload delivery
            // failed. Fall through to compile.
            record_session_stat(&state.sessions, &sid, |t| {
                t.record_depgraph_hit_artifact_miss();
            });
            let failure_name = match &failure {
                CachedHitFailure::VerdictMissing => {
                    record_miss_reason(miss_reason::NO_ARTIFACT_FOR_KEY);
                    "verdict_not_found"
                }
                CachedHitFailure::CacheBlobMissing(_) => {
                    record_miss_reason(miss_reason::NO_ARTIFACT_FOR_KEY);
                    "artifact_not_found"
                }
                CachedHitFailure::CacheRead => {
                    record_miss_reason(miss_reason::NO_ARTIFACT_FOR_KEY);
                    "artifact_read_failed"
                }
                CachedHitFailure::DestinationWrite => {
                    record_miss_reason(miss_reason::DESTINATION_WRITE_FAILED);
                    "destination_write_failed"
                }
            };
            write_session_log(
                &state.sessions,
                &sid,
                &format!("[DIAG] {failure_name}: key={artifact_key_hex}"),
            );
            // Drop the stale depgraph entry pointing at the missing
            // payload so the next lookup for this source does not
            // re-fire the same wasted-hit dance. `invalidate_missing_
            // depgraph_artifact` logs `cleared=N` so the
            // regression test in `daemon_rustc_restore_test.rs` can
            // assert the expected cleared count of 1; an earlier
            // version of this branch invalidated inline too, racing
            // the helper to `cleared=0` and breaking the test.
            if let CachedHitFailure::CacheBlobMissing(evidence) = &failure {
                invalidate_missing_depgraph_artifact(state, &sid, &artifact_key_hex, evidence);
            }
        }
        crate::depgraph::CacheVerdict::SourceChanged { artifact_key } => {
            let artifact_key_hex = artifact_key.hash().to_hex();
            if let Ok(response) = try_depgraph_cached_hit(DepgraphHitProbe {
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
            })
            .await
            {
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
                // Issue #401: take `hash_map` here (rustc compat branch only)
                // so cc/cpp can still hand the populated map to
                // `StoreOutcomeRequest.pre_hashed`. For rustc we never
                // pass `pre_hashed` (the `pre_hash_task` path is used
                // instead), so leaving an empty map behind is benign.
                let compat_hash_map = std::cell::RefCell::new(std::mem::take(&mut hash_map));
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
                    .check_rustc_metadata_compat_diagnostic_with_env(
                        &compat_key,
                        &rustc_current_externs,
                        is_fresh,
                        get_hash,
                        |name| rustc_env_dep_value(client_env.as_deref(), name).map(str::to_owned),
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
                    let requested_outputs = rustc_expected_output_paths(
                        rustc_args,
                        output_path.as_path(),
                        cwd,
                        client_env.as_deref(),
                    );
                    let dylint_hash = client_env.as_deref().and_then(|env| {
                        env.iter().find_map(|(name, value)| {
                            (name == "ZCCACHE_DYLINT_CACHE_INPUT_HASH").then_some(value.as_str())
                        })
                    });
                    let verdict_key_hex =
                        crate::depgraph::compute_rustc_verdict_key(&artifact_key_hex, dylint_hash)
                            .hash()
                            .to_hex();
                    if let Ok(response) =
                        materialize_cached_compile_hit(CachedHitMaterializeRequest {
                            state,
                            sid: &sid,
                            artifact_key_hex: &artifact_key_hex,
                            verdict_key_hex: Some(&verdict_key_hex),
                            source_path: &source_path,
                            output_path: &output_path,
                            secondary_output_dir: output_path
                                .parent()
                                .unwrap_or(cwd_path.as_path())
                                .into(),
                            current_depfile_dest: crate::daemon::server::rustc_depfile_output_path(
                                rustc_args, cwd,
                            ),
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
                            rustc_archive_hardlink_eligible: Some(
                                rustc_args
                                    .crate_types
                                    .iter()
                                    .any(|kind| matches!(kind.as_str(), "lib" | "rlib")),
                            ),
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
        dependency_mode,
    })
    .await;
    let CompileExecOutcome {
        exit_code,
        stdout,
        stderr,
        depfile_strategy,
        dependency_scan,
        pre_hash_task,
        compiler_priority_decision,
        pre_exec_ns,
        break_outputs_ns,
        compiler_process_ns,
        compiler_exec_ns,
        compiler_prep_ns,
        post_exec_ns,
        staged_plan,
    } = match exec_result {
        CompileExecResult::Ok(outcome) => outcome,
        CompileExecResult::Error(resp) => return resp,
    };
    let (artifact_stdout, artifact_stderr, response_stdout, response_stderr) =
        if let Some(plan) = staged_plan.as_ref() {
            let artifact_stdout = Arc::new(plan.canonicalize_output_bytes(stdout.as_slice()));
            let artifact_stderr = Arc::new(plan.canonicalize_output_bytes(stderr.as_slice()));
            let response_stdout = Arc::new(plan.rehydrate_output_bytes(artifact_stdout.as_slice()));
            let response_stderr = Arc::new(plan.rehydrate_output_bytes(artifact_stderr.as_slice()));
            (
                artifact_stdout,
                artifact_stderr,
                response_stdout,
                response_stderr,
            )
        } else {
            (
                Arc::clone(&stdout),
                Arc::clone(&stderr),
                Arc::clone(&stdout),
                Arc::clone(&stderr),
            )
        };

    if exit_code != 0 {
        if let Some(plan) = staged_plan.as_ref() {
            let _ = plan.cleanup();
        }
    }

    if exit_code != 0 {
        state.stats.record_error();
        record_session_stat(&state.sessions, &sid, |t| t.record_error());
        if let Some(rustc_args) = rustc_args_opt.as_ref() {
            let dylint_input_hash = client_env.as_deref().and_then(|env| {
                env.iter().find_map(|(name, value)| {
                    (name == "ZCCACHE_DYLINT_CACHE_INPUT_HASH").then_some(value.as_str())
                })
            });
            if let Some(artifact_key_hex) = maybe_store_rustc_error_artifact(
                state,
                &context_key,
                &source_path,
                &cwd_path,
                &ctx,
                rustc_args,
                dylint_input_hash,
                &artifact_stdout,
                &artifact_stderr,
                exit_code,
                snap_clock,
            )
            .await
            {
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
        let synchronous_persist = staged_plan.is_some();
        let rustc_dylint_input_hash = client_env.as_deref().and_then(|env| {
            env.iter().find_map(|(name, value)| {
                (name == "ZCCACHE_DYLINT_CACHE_INPUT_HASH").then_some(value.as_str())
            })
        });
        if let Some(response) = store_successful_compile(StoreOutcomeRequest {
            state_arc,
            sid: &sid,
            context_key: &context_key,
            source_path: &source_path,
            output_path: &output_path,
            compiler_output_path: staged_plan
                .as_ref()
                .map_or_else(|| output_path.clone(), |plan| plan.primary_staged().clone()),
            staged_output_paths: staged_plan.as_ref().map(StagedCompilePlan::output_paths),
            staged_plan,
            synchronous_persist,
            cwd_path: &cwd_path,
            ctx: &ctx,
            compilation: &compilation,
            dependency_mode,
            rustc_args_opt: rustc_args_opt.as_deref(),
            rustc_dylint_input_hash,
            rustc_extern_paths: &rustc_extern_paths,
            client_env: client_env.as_deref(),
            is_rustc,
            rust_profile_enabled,
            rust_profile_mode,
            stdout: Arc::clone(&artifact_stdout),
            stderr: Arc::clone(&artifact_stderr),
            exit_code,
            depfile_strategy,
            compiler_dependency_scan: dependency_scan,
            pre_hash_task,
            // Issue #401: hand the cc/cpp miss path the hashes already
            // computed in `hash_and_verify` so `store_outcome.rs` skips
            // re-hashing the same headers in its parallel `t_hash` phase.
            // For rustc the same hashes arrive via `pre_hash_task` and
            // `pre_hashed` is left `None`. If we got here via cold context
            // or fell back to a direct compile, `hash_map` is empty and
            // the store path will hash everything as before.
            pre_hashed: if is_rustc || hash_map.is_empty() {
                None
            } else {
                Some(hash_map)
            },
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
        stdout: response_stdout,
        stderr: response_stderr,
        cached: false,
    }
}
