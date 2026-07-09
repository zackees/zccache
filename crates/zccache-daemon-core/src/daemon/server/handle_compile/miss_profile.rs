//! Rust miss profile reporting for the compile pipeline.

use crate::daemon::process::CompilePriorityDecision;

pub(super) struct RustMissProfile<'a> {
    pub(super) mode: &'a str,
    pub(super) compiler_priority_decision: CompilePriorityDecision,
    pub(super) total_ns: u64,
    pub(super) pre_exec_ns: u64,
    pub(super) system_includes_ns: u64,
    pub(super) system_watch_ns: u64,
    pub(super) parse_args_ns: u64,
    pub(super) build_context_ns: u64,
    pub(super) hash_source_ns: u64,
    pub(super) hash_headers_ns: u64,
    pub(super) depgraph_check_ns: u64,
    pub(super) break_outputs_ns: u64,
    pub(super) compiler_prep_ns: u64,
    pub(super) compiler_process_ns: u64,
    pub(super) post_exec_ns: u64,
    pub(super) apply_changes_ns: u64,
    pub(super) collect_outputs_ns: u64,
    pub(super) rust_output_count: usize,
    pub(super) rust_output_bytes: u64,
    pub(super) include_scan_ns: u64,
    pub(super) register_tracked_ns: u64,
    pub(super) dep_dirs_ns: u64,
    pub(super) hash_all_ns: u64,
    pub(super) artifact_store_ns: u64,
    pub(super) depgraph_update_ns: u64,
    pub(super) artifact_build_ns: u64,
    pub(super) artifact_meta_build_ns: u64,
    pub(super) rust_snapshot_ns: u64,
    pub(super) rust_snapshot_hardlink_count: u64,
    pub(super) rust_snapshot_copy_count: u64,
    pub(super) rust_snapshot_copy_bytes: u64,
    pub(super) rust_snapshot_error_count: u64,
    pub(super) persist_enqueue_ns: u64,
    pub(super) artifact_insert_stats_ns: u64,
    pub(super) artifact_index_build_ns: u64,
    pub(super) artifact_index_persist_ns: u64,
    pub(super) artifact_memory_insert_ns: u64,
}

pub(super) fn emit_rust_miss_profile(profile: RustMissProfile<'_>) {
    let RustMissProfile {
        mode,
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
    } = profile;

    let pre_exec_measured_ns = system_includes_ns
        .saturating_add(system_watch_ns)
        .saturating_add(parse_args_ns)
        .saturating_add(build_context_ns)
        .saturating_add(hash_source_ns)
        .saturating_add(hash_headers_ns)
        .saturating_add(depgraph_check_ns);
    let pre_exec_other_ns = pre_exec_ns.saturating_sub(pre_exec_measured_ns);
    // Issue ISSUE-501: include every named sub-phase printed below so
    // `artifact_store_other_ns` reflects only the un-named residual
    // (intra-phase glue, write_session_log, record_pch_source_mapping).
    // Prior version omitted `rust_snapshot_ns`, double-counting ~1.4 ms
    // of named persist work as "unaccounted" in cold profiles.
    let artifact_store_measured_ns = depgraph_update_ns
        .saturating_add(artifact_build_ns)
        .saturating_add(artifact_meta_build_ns)
        .saturating_add(rust_snapshot_ns)
        .saturating_add(persist_enqueue_ns)
        .saturating_add(artifact_insert_stats_ns)
        .saturating_add(artifact_index_build_ns)
        .saturating_add(artifact_index_persist_ns)
        .saturating_add(artifact_memory_insert_ns);
    let artifact_store_other_ns = artifact_store_ns.saturating_sub(artifact_store_measured_ns);
    let accounted_ns = pre_exec_ns
        .saturating_add(compiler_prep_ns)
        .saturating_add(compiler_process_ns)
        .saturating_add(post_exec_ns)
        .saturating_add(apply_changes_ns)
        .saturating_add(collect_outputs_ns)
        .saturating_add(include_scan_ns)
        .saturating_add(register_tracked_ns)
        .saturating_add(dep_dirs_ns)
        .saturating_add(hash_all_ns)
        .saturating_add(artifact_store_ns);
    let unaccounted_ns = total_ns.saturating_sub(accounted_ns);
    let compiler_cpu_usage_percent = compiler_priority_decision
        .cpu_usage_percent
        .map(|usage| format!("{usage:.1}"))
        .unwrap_or_else(|| "n/a".to_string());

    eprintln!(
        concat!(
            "zccache_rust_miss_profile ",
            "mode={} compiler_priority={} compiler_effective_priority={} ",
            "compiler_cpu_usage_percent={} total_ns={} pre_exec_ns={} system_includes_ns={} ",
            "system_watch_ns={} parse_args_ns={} build_context_ns={} ",
            "hash_source_ns={} hash_headers_ns={} depgraph_check_ns={} ",
            "pre_exec_other_ns={} break_outputs_ns={} compiler_prep_ns={} compiler_process_ns={} ",
            "post_exec_ns={} apply_changes_ns={} collect_outputs_ns={} ",
            "outputs={} output_bytes={} include_scan_ns={} ",
            "register_tracked_ns={} dep_dirs_ns={} hash_all_ns={} ",
            "artifact_store_ns={} depgraph_update_ns={} artifact_build_ns={} ",
            "artifact_meta_build_ns={} rust_snapshot_ns={} ",
            "rust_snapshot_hardlink_count={} rust_snapshot_copy_count={} ",
            "rust_snapshot_copy_bytes={} rust_snapshot_error_count={} ",
            "persist_enqueue_ns={} artifact_insert_stats_ns={} ",
            "artifact_index_build_ns={} artifact_index_persist_ns={} ",
            "artifact_memory_insert_ns={} ",
            "artifact_store_other_ns={} unaccounted_ns={}"
        ),
        mode,
        compiler_priority_decision.requested.as_str(),
        compiler_priority_decision.effective.as_str(),
        compiler_cpu_usage_percent,
        total_ns,
        pre_exec_ns,
        system_includes_ns,
        system_watch_ns,
        parse_args_ns,
        build_context_ns,
        hash_source_ns,
        hash_headers_ns,
        depgraph_check_ns,
        pre_exec_other_ns,
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
        artifact_store_other_ns,
        unaccounted_ns,
    );
}

/// Cold-miss phase profile for C / C++ / archiver / driver-link compile
/// paths. Issue #535 — the non-rustc cold profile has no published phase
/// breakdown today (only `RustMissProfile` emits, gated on `is_rustc`),
/// so the c-static-library-link / cpp-driver-link cold overhead (each
/// ~13 ms over bare on the Linux 4-core CI runner) can't be attributed
/// to a specific phase without re-running the bench manually.
///
/// Mirrors the shared subset of `RustMissProfile` and drops the
/// rust-specific fields (`rust_snapshot_*`, `rust_output_*`,
/// `hash_source_ns`, `hash_headers_ns`, `depgraph_check_ns`,
/// `compiler_prep_ns`, `apply_changes_ns`, `collect_outputs_ns`,
/// `artifact_meta_build_ns`, etc.) that don't apply to the CC path.
pub(super) struct CcMissProfile<'a> {
    pub(super) family: &'a str,
    pub(super) compiler_priority_decision: CompilePriorityDecision,
    pub(super) total_ns: u64,
    pub(super) pre_exec_ns: u64,
    pub(super) system_includes_ns: u64,
    pub(super) system_watch_ns: u64,
    pub(super) parse_args_ns: u64,
    pub(super) build_context_ns: u64,
    pub(super) break_outputs_ns: u64,
    pub(super) compiler_process_ns: u64,
    pub(super) post_exec_ns: u64,
    pub(super) include_scan_ns: u64,
    pub(super) register_tracked_ns: u64,
    pub(super) dep_dirs_ns: u64,
    pub(super) hash_all_ns: u64,
    pub(super) artifact_store_ns: u64,
    // Issue #548: artifact_store sub-phases. Same values
    // `MissArtifactStoreStats` already computes for the rust path;
    // the cc emit just wires them through. The 7.6 ms / per-compile
    // `artifact_store_ns` on cpp-inline Single-file Cold (post-#546)
    // was opaque without these — now we can see whether it's the
    // depgraph update, the artifact persist, or the index write
    // dominating.
    pub(super) depgraph_update_ns: u64,
    pub(super) artifact_build_ns: u64,
    pub(super) persist_enqueue_ns: u64,
    pub(super) artifact_insert_stats_ns: u64,
    pub(super) artifact_index_build_ns: u64,
    pub(super) artifact_index_persist_ns: u64,
    pub(super) artifact_memory_insert_ns: u64,
}

/// Cold-miss phase profile for `Request::LinkEphemeral` paths
/// (`handle_link_ephemeral` — ar / clang++ link / driver-link).
///
/// Distinct from `CcMissProfile` because link/archive operations don't
/// have system-include discovery, dep-info scanning, or a
/// CompileContext — the dominant phases are tool-hash, input-hash,
/// the tool spawn, and the output read for cache storage. Issue #535
/// needs this to confirm whether `c-static-library-link Cold` /
/// `cpp-driver-link Cold` overhead lives in input hashing (the #533
/// overlap target) or somewhere else.
pub(in crate::daemon::server) struct LinkMissProfile<'a> {
    pub(in crate::daemon::server) family: &'a str,
    pub(in crate::daemon::server) input_count: usize,
    pub(in crate::daemon::server) total_ns: u64,
    pub(in crate::daemon::server) parse_args_ns: u64,
    pub(in crate::daemon::server) tool_hash_ns: u64,
    pub(in crate::daemon::server) input_hash_ns: u64,
    pub(in crate::daemon::server) cache_lookup_ns: u64,
    pub(in crate::daemon::server) compiler_process_ns: u64,
    pub(in crate::daemon::server) output_read_ns: u64,
    pub(in crate::daemon::server) artifact_store_ns: u64,
}

pub(in crate::daemon::server) fn emit_link_miss_profile(profile: LinkMissProfile<'_>) {
    let LinkMissProfile {
        family,
        input_count,
        total_ns,
        parse_args_ns,
        tool_hash_ns,
        input_hash_ns,
        cache_lookup_ns,
        compiler_process_ns,
        output_read_ns,
        artifact_store_ns,
    } = profile;

    let accounted_ns = parse_args_ns
        .saturating_add(tool_hash_ns)
        .saturating_add(input_hash_ns)
        .saturating_add(cache_lookup_ns)
        .saturating_add(compiler_process_ns)
        .saturating_add(output_read_ns)
        .saturating_add(artifact_store_ns);
    let unaccounted_ns = total_ns.saturating_sub(accounted_ns);

    eprintln!(
        concat!(
            "zccache_link_miss_profile ",
            "family={} input_count={} total_ns={} parse_args_ns={} ",
            "tool_hash_ns={} input_hash_ns={} cache_lookup_ns={} ",
            "compiler_process_ns={} output_read_ns={} ",
            "artifact_store_ns={} unaccounted_ns={}",
        ),
        family,
        input_count,
        total_ns,
        parse_args_ns,
        tool_hash_ns,
        input_hash_ns,
        cache_lookup_ns,
        compiler_process_ns,
        output_read_ns,
        artifact_store_ns,
        unaccounted_ns,
    );
}

pub(super) fn emit_cc_miss_profile(profile: CcMissProfile<'_>) {
    let CcMissProfile {
        family,
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
    } = profile;

    let pre_exec_measured_ns = system_includes_ns
        .saturating_add(system_watch_ns)
        .saturating_add(parse_args_ns)
        .saturating_add(build_context_ns);
    let pre_exec_other_ns = pre_exec_ns.saturating_sub(pre_exec_measured_ns);
    // Sub-phase residual: anything in artifact_store that isn't one
    // of the measured slices (e.g. the redb commit on the index path).
    let artifact_store_measured_ns = depgraph_update_ns
        .saturating_add(artifact_build_ns)
        .saturating_add(persist_enqueue_ns)
        .saturating_add(artifact_insert_stats_ns)
        .saturating_add(artifact_index_build_ns)
        .saturating_add(artifact_index_persist_ns)
        .saturating_add(artifact_memory_insert_ns);
    let artifact_store_other_ns = artifact_store_ns.saturating_sub(artifact_store_measured_ns);
    let accounted_ns = pre_exec_ns
        .saturating_add(break_outputs_ns)
        .saturating_add(compiler_process_ns)
        .saturating_add(post_exec_ns)
        .saturating_add(include_scan_ns)
        .saturating_add(register_tracked_ns)
        .saturating_add(dep_dirs_ns)
        .saturating_add(hash_all_ns)
        .saturating_add(artifact_store_ns);
    let unaccounted_ns = total_ns.saturating_sub(accounted_ns);

    eprintln!(
        concat!(
            "zccache_cc_miss_profile ",
            "family={} compiler_priority={} compiler_effective_priority={} ",
            "total_ns={} pre_exec_ns={} system_includes_ns={} system_watch_ns={} ",
            "parse_args_ns={} build_context_ns={} pre_exec_other_ns={} ",
            "break_outputs_ns={} compiler_process_ns={} post_exec_ns={} ",
            "include_scan_ns={} register_tracked_ns={} dep_dirs_ns={} ",
            "hash_all_ns={} artifact_store_ns={} ",
            "depgraph_update_ns={} artifact_build_ns={} persist_enqueue_ns={} ",
            "artifact_insert_stats_ns={} artifact_index_build_ns={} ",
            "artifact_index_persist_ns={} artifact_memory_insert_ns={} ",
            "artifact_store_other_ns={} unaccounted_ns={}",
        ),
        family,
        compiler_priority_decision.requested.as_str(),
        compiler_priority_decision.effective.as_str(),
        total_ns,
        pre_exec_ns,
        system_includes_ns,
        system_watch_ns,
        parse_args_ns,
        build_context_ns,
        pre_exec_other_ns,
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
        artifact_store_other_ns,
        unaccounted_ns,
    );
}
