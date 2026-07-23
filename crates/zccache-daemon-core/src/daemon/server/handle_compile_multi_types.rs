//! Shared result types for multi-source cache checks and staged execution.

use super::*;

pub(in crate::daemon::server) fn materialize_multi_hit(
    targets: &[(NormalizedPath, NormalizedPath)],
    payloads: &[CachedPayload],
) -> bool {
    write_payloads_par(targets, payloads)
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

pub(in crate::daemon::server) struct PendingWrite {
    pub(in crate::daemon::server) out_path: NormalizedPath,
    pub(in crate::daemon::server) cache_file: NormalizedPath,
    pub(in crate::daemon::server) data: Vec<u8>,
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
        pending_writes: Vec<PendingWrite>,
    },
    Miss {
        source_path: NormalizedPath,
        output_path: NormalizedPath,
        context_key: ContextKey,
        ctx: Box<CompileContext>,
        input_snapshot: InputSnapshot,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

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
