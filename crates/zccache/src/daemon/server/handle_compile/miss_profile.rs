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
    let artifact_store_measured_ns = depgraph_update_ns
        .saturating_add(artifact_build_ns)
        .saturating_add(persist_enqueue_ns)
        .saturating_add(artifact_insert_stats_ns);
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
