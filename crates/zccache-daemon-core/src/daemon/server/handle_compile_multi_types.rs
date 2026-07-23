//! Shared result types for multi-source cache checks and staged execution.

use super::*;

pub(in crate::daemon::server) fn materialize_multi_hit(
    targets: &[NormalizedPath],
    payloads: &[CachedPayload],
) -> MaterializationResult<StagedMaterializationStats> {
    write_payloads_par_observed(targets, payloads)
}

fn invalidate_graph_after_blob_loss(
    dep_graph: &crate::depgraph::DepGraph,
    artifact_key: &str,
    _evidence: &CacheBlobMissing,
) -> usize {
    let keys = std::collections::HashSet::from([artifact_key.to_string()]);
    dep_graph.invalidate_artifact_keys(&keys)
}

fn invalidate_graph_after_multi_failure(
    dep_graph: &crate::depgraph::DepGraph,
    artifact_key: &str,
    failure: &MaterializationFailure,
) -> usize {
    let MaterializationFailure::CacheBlobMissing(evidence) = failure else {
        return 0;
    };
    invalidate_graph_after_blob_loss(dep_graph, artifact_key, evidence)
}

pub(in crate::daemon::server) fn invalidate_multi_artifact_after_failure(
    state: &SharedState,
    artifact_key: &str,
    failure: &MaterializationFailure,
) -> usize {
    invalidate_graph_after_multi_failure(state.dep_graph.load().as_ref(), artifact_key, failure)
}

#[derive(Clone, Copy)]
pub(super) struct LegacyDepfilePolicy {
    pub(super) injected_flag: Option<&'static str>,
    pub(super) excludes_system_headers: bool,
}

pub(super) fn legacy_depfile_policy(
    args: &[String],
    dependency_mode: DependencyDiscoveryMode,
) -> LegacyDepfilePolicy {
    let user_excludes_system_headers = args.iter().fold(None, |current, arg| {
        if arg == "-MMD" {
            Some(true)
        } else if arg == "-MD" {
            Some(false)
        } else {
            current
        }
    });
    LegacyDepfilePolicy {
        injected_flag: user_excludes_system_headers
            .is_none()
            .then(|| dependency_mode.injected_depfile_flag()),
        excludes_system_headers: user_excludes_system_headers
            .unwrap_or_else(|| dependency_mode.use_mmd()),
    }
}

pub(super) fn depfile_targets_stdout(args: &[String]) -> bool {
    let mut stdout = false;
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        if arg == "-MF" {
            stdout = args.get(index + 1).is_some_and(|value| value == "-");
            index += 2;
            continue;
        }
        if let Some(path) = arg.strip_prefix("-MF").filter(|path| !path.is_empty()) {
            stdout = path == "-";
        }
        index += 1;
    }
    stdout
}

pub(super) struct UnitCacheCheck<'a> {
    pub(super) cwd_path: &'a Path,
    pub(super) key_root: &'a NormalizedPath,
    pub(super) system_includes: &'a [NormalizedPath],
    pub(super) shared_base: Option<&'a CompileContext>,
    pub(super) shared_dep_flags: Option<&'a UserDepFlags>,
    pub(super) scan_cache: &'a crate::depgraph::scanner::RecursiveScanCache,
    pub(super) cache_now: Instant,
    pub(super) dependency_mode: DependencyDiscoveryMode,
}

pub(super) struct MissOutcome {
    pub(super) dep_dirs: Vec<NormalizedPath>,
    pub(super) output_path: NormalizedPath,
    pub(super) persist: Option<PersistTaskParams>,
}

pub(super) struct PersistTaskParams {
    pub(super) artifact_key_hex: String,
    pub(super) persist_meta: ArtifactIndex,
    pub(super) payloads: Vec<Arc<Vec<u8>>>,
    pub(super) payload_size: usize,
}

pub(super) fn owned_fast_hit_entry(
    cache: &DashMap<ContextKey, FastHitEntry>,
    key: &ContextKey,
) -> Option<FastHitEntry> {
    cache.get(key).map(|entry| entry.clone())
}

pub(in crate::daemon::server) enum UnitCacheResult {
    Hit {
        stdout: Arc<Vec<u8>>,
        stderr: Arc<Vec<u8>>,
        artifact_bytes: u64,
        source_path: NormalizedPath,
    },
    Miss {
        source_path: NormalizedPath,
        output_path: NormalizedPath,
        context_key: ContextKey,
        ctx: Box<CompileContext>,
        input_snapshot: InputSnapshot,
        reason: &'static str,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn content_hash(path: &Path) -> Option<crate::hash::ContentHash> {
        Some(crate::hash::hash_bytes(path.to_string_lossy().as_bytes()))
    }

    fn warm_context(
        graph: &crate::depgraph::DepGraph,
        source: &str,
    ) -> (crate::depgraph::ContextKey, String) {
        let context = CompileContext {
            source_file: source.into(),
            include_search: crate::depgraph::IncludeSearchPaths::default(),
            defines: Vec::new(),
            flags: Vec::new(),
            force_includes: Vec::new(),
            unknown_flags: Vec::new(),
        };
        let context_key = graph.register(context);
        let artifact_key = graph
            .update(
                &context_key,
                crate::depgraph::ScanResult {
                    resolved: Vec::new(),
                    unresolved: Vec::new(),
                    has_computed: false,
                },
                content_hash,
            )
            .unwrap();
        (context_key, artifact_key.hash().to_hex().to_string())
    }

    #[test]
    fn owned_fast_hit_entry_releases_map_guard_before_mutation() {
        let cache = DashMap::new();
        let key = ContextKey::from_raw([7; 32]);
        cache.insert(
            key,
            FastHitEntry {
                clock: Clock::ZERO,
                artifact_key_hex: "artifact".to_string(),
                cached_at: Instant::now(),
            },
        );

        let entry = owned_fast_hit_entry(&cache, &key).unwrap();
        assert_eq!(entry.artifact_key_hex, "artifact");
        assert!(cache.remove(&key).is_some());
    }

    #[test]
    fn poisoned_multi_destination_recompiles_only_that_unit_without_eviction() {
        let dir = tempfile::tempdir().unwrap();
        let graph = crate::depgraph::DepGraph::new();
        let (_failed_context, failed_key) = warm_context(&graph, "/src/failed.c");
        let (_sibling_context, _sibling_key) = warm_context(&graph, "/src/sibling.c");
        assert_eq!(graph.contexts_with_artifact_key(), 2);

        let failed_blob: NormalizedPath = dir.path().join("failed.o.cache").into();
        let sibling_blob: NormalizedPath = dir.path().join("sibling.o.cache").into();
        std::fs::write(&failed_blob, b"failed cached bytes").unwrap();
        std::fs::write(&sibling_blob, b"sibling cached bytes").unwrap();
        write_authoritative_blob_digest(&failed_blob).unwrap();
        write_authoritative_blob_digest(&sibling_blob).unwrap();
        let blocked_parent = dir.path().join("blocked-parent");
        std::fs::write(&blocked_parent, b"not a directory").unwrap();
        let failed_target: NormalizedPath = blocked_parent.join("failed.o").into();
        let sibling_target: NormalizedPath = dir.path().join("sibling.o").into();

        let failure = materialize_multi_hit(
            &[failed_target],
            &[CachedPayload::File(failed_blob.clone())],
        )
        .unwrap_err();
        assert!(matches!(
            failure,
            MaterializationFailure::DestinationWrite(_)
        ));
        assert_eq!(
            invalidate_graph_after_multi_failure(&graph, &failed_key, &failure),
            0
        );
        assert_eq!(
            graph.contexts_with_artifact_key(),
            2,
            "destination failure must preserve both the failed TU and sibling depgraph entries"
        );
        assert!(
            materialize_multi_hit(
                &[sibling_target],
                &[CachedPayload::File(sibling_blob.clone())],
            )
            .is_ok(),
            "the unpoisoned sibling remains a warm hit"
        );

        std::fs::remove_file(&failed_blob).unwrap();
        let missing = materialize_multi_hit(
            &[dir.path().join("retry.o").into()],
            &[CachedPayload::File(failed_blob)],
        )
        .unwrap_err();
        assert!(matches!(
            missing,
            MaterializationFailure::CacheBlobMissing(_)
        ));
        assert_eq!(
            invalidate_graph_after_multi_failure(&graph, &failed_key, &missing),
            1
        );
        assert_eq!(
            graph.contexts_with_artifact_key(),
            1,
            "genuine blob loss invalidates only the matching TU"
        );
    }

    #[test]
    fn legacy_depfile_policy_preserves_explicit_user_mode() {
        let fast_md = legacy_depfile_policy(
            &["-MMD".into(), "-MD".into()],
            DependencyDiscoveryMode::SkipSystemHeaders,
        );
        assert!(fast_md.injected_flag.is_none());
        assert!(!fast_md.excludes_system_headers);

        let safe_mmd = legacy_depfile_policy(
            &["-MD".into(), "-MMD".into()],
            DependencyDiscoveryMode::AllHeaders,
        );
        assert!(safe_mmd.injected_flag.is_none());
        assert!(safe_mmd.excludes_system_headers);
    }

    #[test]
    fn depfile_stdout_detection_uses_last_mf_value() {
        assert!(depfile_targets_stdout(&[
            "-MFdeps.d".into(),
            "-MF".into(),
            "-".into(),
        ]));
        assert!(!depfile_targets_stdout(&[
            "-MF-".into(),
            "-MFdeps.d".into(),
        ]));
    }
}
