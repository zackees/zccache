//! Cached-hit branch controllers for the compile pipeline.

use super::super::*;
use super::cached_hit::{
    materialize_cached_compile_hit, CachedHitMaterializeRequest, CachedHitPhases,
};
use crate::depgraph::depfile::user_depfile_destination;
use crate::depgraph::UserDepFlags;

pub(super) struct RequestCacheHitProbe<'a> {
    pub(super) state: &'a SharedState,
    pub(super) sid: &'a SessionId,
    pub(super) compiler_path: &'a Path,
    pub(super) effective_args: &'a [String],
    pub(super) cwd: &'a Path,
    pub(super) request_cache_key_root: &'a Option<NormalizedPath>,
    pub(super) client_env: Option<&'a [(String, String)]>,
    pub(super) compile_start: Instant,
    pub(super) snap_clock: Clock,
}

pub(super) fn try_request_cache_hit(probe: RequestCacheHitProbe<'_>) -> Option<Response> {
    let RequestCacheHitProbe {
        state,
        sid,
        compiler_path,
        effective_args,
        cwd,
        request_cache_key_root,
        client_env,
        compile_start,
        snap_clock,
    } = probe;

    if !state.watcher_active.load(Ordering::Acquire) {
        return None;
    }

    let t_request_cache_lookup = Instant::now();
    let request_fp = request_fingerprint(
        compiler_path,
        effective_args,
        cwd,
        request_cache_key_root.as_deref(),
        client_env,
    );
    let req_entry = state.request_cache.get(&request_fp)?;
    let request_cache_lookup_ns = t_request_cache_lookup.elapsed().as_nanos() as u64;
    if !request_cache_entry_matches_root(&req_entry, request_cache_key_root.as_ref()) {
        return None;
    }
    let fh_entry = state.fast_hit_cache.get(&req_entry.context_key)?;
    let artifact_key_hex = &fh_entry.artifact_key_hex;
    let source_path = req_entry
        .source_path
        .resolve(request_cache_key_root.as_deref());
    let output_path = req_entry
        .output_path
        .resolve(request_cache_key_root.as_deref());
    // Issue #643: rebase the cached depfile destination to the current
    // request's key root. For cross-worktree (`HIT_WORKTREE_REQUEST`)
    // hits, this routes the depfile to worktree B's path even though the
    // entry was created from worktree A — same semantics as
    // `source_path` / `output_path`.
    let current_depfile_dest: Option<NormalizedPath> = req_entry
        .depfile_path
        .as_ref()
        .map(|path| path.resolve(request_cache_key_root.as_deref()));
    let mtime_floor_paths: Vec<NormalizedPath> = req_entry
        .input_paths
        .iter()
        .map(|path| path.resolve(request_cache_key_root.as_deref()))
        .collect();
    let same_root = req_entry.root.as_ref() == request_cache_key_root.as_ref();
    let t_cross_root_validate = Instant::now();
    let inputs_match = if same_root {
        context_files_fresh(state, &req_entry.context_key, &source_path, fh_entry.clock)
    } else {
        request_cache_artifact_matches(
            state,
            &req_entry,
            request_fp,
            request_cache_key_root.as_ref(),
            artifact_key_hex,
            compile_start,
            snap_clock,
        )
    };
    let cross_root_validate_ns = if same_root {
        0
    } else {
        t_cross_root_validate.elapsed().as_nanos() as u64
    };
    if !cache_entry_fresh_at(compile_start, fh_entry.cached_at, FAST_HIT_MAX_AGE)
        || !cache_entry_fresh_at(compile_start, req_entry.cached_at, EPHEMERAL_CACHE_MAX_AGE)
        || !inputs_match
    {
        return None;
    }

    let hit_label = if same_root {
        "HIT_REQUEST"
    } else {
        "HIT_WORKTREE_REQUEST"
    };
    materialize_cached_compile_hit(CachedHitMaterializeRequest {
        state,
        sid,
        artifact_key_hex,
        source_path: &source_path,
        output_path: &output_path,
        secondary_output_dir: output_path.parent().unwrap_or(cwd).into(),
        current_depfile_dest,
        compile_start,
        hit_label,
        cached_error_label: "CACHED_ERROR_REQUEST",
        record_compilation: true,
        downgrade_output_metadata: false,
        mtime_floor_paths,
        phases: CachedHitPhases::request_cache(request_cache_lookup_ns, cross_root_validate_ns),
    })
}

pub(super) struct FastHitProbe<'a> {
    pub(super) state: &'a SharedState,
    pub(super) sid: &'a SessionId,
    pub(super) context_key: ContextKey,
    pub(super) source_path: &'a NormalizedPath,
    pub(super) output_path: &'a NormalizedPath,
    pub(super) cwd_path: &'a NormalizedPath,
    pub(super) ctx: &'a CompileContext,
    pub(super) compiler_path: &'a Path,
    pub(super) effective_args: &'a [String],
    pub(super) cwd: &'a Path,
    pub(super) request_cache_key_root: &'a Option<NormalizedPath>,
    pub(super) client_env: Option<&'a [(String, String)]>,
    /// Issue #643: the user's parsed depfile flags. `None` for rustc (which
    /// uses its own dep-info mechanism); for C/C++ it carries the user's
    /// `-MD` / `-MF` so the hit can restore the depfile to the current
    /// build's destination.
    pub(super) dep_flags: Option<&'a UserDepFlags>,
    pub(super) is_rustc: bool,
    pub(super) worktree_equivalent_context: bool,
    pub(super) worktree_bound: bool,
    pub(super) compile_start: Instant,
    pub(super) parse_args_ns: u64,
    pub(super) build_context_ns: u64,
}

pub(super) fn try_fast_hit(probe: FastHitProbe<'_>) -> Option<Response> {
    let FastHitProbe {
        state,
        sid,
        context_key,
        source_path,
        output_path,
        cwd_path,
        ctx,
        compiler_path,
        effective_args,
        cwd,
        request_cache_key_root,
        client_env,
        dep_flags,
        is_rustc,
        worktree_equivalent_context,
        worktree_bound,
        compile_start,
        parse_args_ns,
        build_context_ns,
    } = probe;

    if !state.watcher_active.load(Ordering::Acquire) {
        return None;
    }
    if worktree_equivalent_context {
        // Cross-worktree hits may be the first time this daemon has seen the
        // current root's source paths. Until that root has gone through a
        // miss path and installed directory watches, keep using the hashed
        // depgraph path instead of the zero-hash fast-hit cache.
        return None;
    }
    let entry = state.fast_hit_cache.get(&context_key)?;
    if !cache_entry_fresh_at(compile_start, entry.cached_at, FAST_HIT_MAX_AGE)
        || !context_files_fresh(state, &context_key, source_path, entry.clock)
    {
        return None;
    }

    let secondary_output_dir = if is_rustc {
        output_path.parent().unwrap_or(cwd_path).into()
    } else {
        cwd_path.clone()
    };
    let hit_label = if worktree_equivalent_context {
        "HIT_WORKTREE_FAST"
    } else {
        "HIT_FAST"
    };
    let input_paths = request_cache_input_paths(state, &context_key, source_path, ctx);
    let current_depfile_dest: Option<NormalizedPath> =
        dep_flags.and_then(|flags| user_depfile_destination(flags, output_path.as_path()));
    let response = materialize_cached_compile_hit(CachedHitMaterializeRequest {
        state,
        sid,
        artifact_key_hex: &entry.artifact_key_hex,
        source_path,
        output_path,
        secondary_output_dir,
        current_depfile_dest: current_depfile_dest.clone(),
        compile_start,
        hit_label,
        cached_error_label: "CACHED_ERROR_FAST",
        record_compilation: false,
        downgrade_output_metadata: true,
        mtime_floor_paths: input_paths.clone(),
        phases: CachedHitPhases {
            parse_args_ns,
            build_context_ns,
            hash_source_ns: 0,
            hash_headers_ns: 0,
            depgraph_check_ns: 0,
            request_cache_lookup_ns: 0,
            cross_root_validate_ns: 0,
        },
    })?;

    let rfp = request_fingerprint(
        compiler_path,
        effective_args,
        cwd,
        request_cache_key_root.as_deref(),
        client_env,
    );
    state.request_cache.insert(
        rfp,
        request_cache_entry(
            context_key,
            source_path,
            output_path,
            current_depfile_dest.as_ref(),
            input_paths,
            request_cache_key_root.as_ref(),
            worktree_bound,
        ),
    );
    Some(response)
}

pub(super) struct DepgraphHitProbe<'a> {
    pub(super) state: &'a SharedState,
    pub(super) sid: &'a SessionId,
    pub(super) context_key: ContextKey,
    pub(super) artifact_key_hex: &'a str,
    pub(super) source_path: &'a NormalizedPath,
    pub(super) output_path: &'a NormalizedPath,
    pub(super) cwd_path: &'a NormalizedPath,
    pub(super) ctx: &'a CompileContext,
    pub(super) compiler_path: &'a Path,
    pub(super) effective_args: &'a [String],
    pub(super) cwd: &'a Path,
    pub(super) request_cache_key_root: &'a Option<NormalizedPath>,
    pub(super) client_env: Option<&'a [(String, String)]>,
    /// Issue #643: the user's parsed depfile flags. `None` for rustc.
    /// Used to derive `current_depfile_dest` for the cache hit and to
    /// stamp the request-cache entry so subsequent fast-path hits also
    /// restore the depfile.
    pub(super) dep_flags: Option<&'a UserDepFlags>,
    pub(super) is_rustc: bool,
    pub(super) worktree_equivalent_context: bool,
    pub(super) worktree_bound: bool,
    pub(super) compile_start: Instant,
    pub(super) parse_args_ns: u64,
    pub(super) build_context_ns: u64,
    pub(super) hash_source_ns: u64,
    pub(super) hash_headers_ns: u64,
    pub(super) depgraph_check_ns: u64,
}

pub(super) fn try_depgraph_cached_hit(probe: DepgraphHitProbe<'_>) -> Option<Response> {
    let DepgraphHitProbe {
        state,
        sid,
        context_key,
        artifact_key_hex,
        source_path,
        output_path,
        cwd_path,
        ctx,
        compiler_path,
        effective_args,
        cwd,
        request_cache_key_root,
        client_env,
        dep_flags,
        is_rustc,
        worktree_equivalent_context,
        worktree_bound,
        compile_start,
        parse_args_ns,
        build_context_ns,
        hash_source_ns,
        hash_headers_ns,
        depgraph_check_ns,
    } = probe;

    let secondary_output_dir = if is_rustc {
        output_path.parent().unwrap_or(cwd_path).into()
    } else {
        cwd_path.clone()
    };
    let hit_label = if worktree_equivalent_context {
        "HIT_WORKTREE"
    } else {
        "HIT"
    };
    let input_paths = request_cache_input_paths(state, &context_key, source_path, ctx);
    let current_depfile_dest: Option<NormalizedPath> =
        dep_flags.and_then(|flags| user_depfile_destination(flags, output_path.as_path()));
    let response = materialize_cached_compile_hit(CachedHitMaterializeRequest {
        state,
        sid,
        artifact_key_hex,
        source_path,
        output_path,
        secondary_output_dir,
        current_depfile_dest: current_depfile_dest.clone(),
        compile_start,
        hit_label,
        cached_error_label: "CACHED_ERROR",
        record_compilation: false,
        downgrade_output_metadata: true,
        mtime_floor_paths: input_paths.clone(),
        phases: CachedHitPhases {
            parse_args_ns,
            build_context_ns,
            hash_source_ns,
            hash_headers_ns,
            depgraph_check_ns,
            request_cache_lookup_ns: 0,
            cross_root_validate_ns: 0,
        },
    })?;

    if !worktree_equivalent_context {
        state.cache_system.register_tracked(&input_paths);
        let current_clock = state.cache_system.current_clock();
        state.fast_hit_cache.insert(
            context_key,
            FastHitEntry {
                clock: current_clock,
                artifact_key_hex: artifact_key_hex.to_string(),
                cached_at: Instant::now(),
            },
        );

        let rfp = request_fingerprint(
            compiler_path,
            effective_args,
            cwd,
            request_cache_key_root.as_deref(),
            client_env,
        );
        state.request_cache.insert(
            rfp,
            request_cache_entry(
                context_key,
                source_path,
                output_path,
                current_depfile_dest.as_ref(),
                input_paths,
                request_cache_key_root.as_ref(),
                worktree_bound,
            ),
        );
    }
    Some(response)
}
