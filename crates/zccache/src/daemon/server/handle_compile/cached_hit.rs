//! Shared cached-hit materialization for compile cache branches.

use super::super::*;

pub(super) struct CachedHitPhases {
    pub(super) parse_args_ns: u64,
    pub(super) build_context_ns: u64,
    pub(super) hash_source_ns: u64,
    pub(super) hash_headers_ns: u64,
    pub(super) depgraph_check_ns: u64,
    pub(super) request_cache_lookup_ns: u64,
    pub(super) cross_root_validate_ns: u64,
}

impl CachedHitPhases {
    pub(super) fn request_cache(request_cache_lookup_ns: u64, cross_root_validate_ns: u64) -> Self {
        Self {
            parse_args_ns: 0,
            build_context_ns: 0,
            hash_source_ns: 0,
            hash_headers_ns: 0,
            depgraph_check_ns: 0,
            request_cache_lookup_ns,
            cross_root_validate_ns,
        }
    }
}

pub(super) struct CachedHitMaterializeRequest<'a> {
    pub(super) state: &'a SharedState,
    pub(super) sid: &'a SessionId,
    pub(super) artifact_key_hex: &'a str,
    pub(super) source_path: &'a NormalizedPath,
    pub(super) output_path: &'a NormalizedPath,
    pub(super) secondary_output_dir: PathBuf,
    pub(super) compile_start: Instant,
    pub(super) hit_label: &'static str,
    pub(super) cached_error_label: &'static str,
    pub(super) record_compilation: bool,
    pub(super) downgrade_output_metadata: bool,
    pub(super) phases: CachedHitPhases,
}

pub(super) fn materialize_cached_compile_hit(
    request: CachedHitMaterializeRequest<'_>,
) -> Option<Response> {
    let CachedHitMaterializeRequest {
        state,
        sid,
        artifact_key_hex,
        source_path,
        output_path,
        secondary_output_dir,
        compile_start,
        hit_label,
        cached_error_label,
        record_compilation,
        downgrade_output_metadata,
        phases,
    } = request;

    let t_artifact_lookup = Instant::now();
    let mut cached_ref = lookup_artifact_with_disk_fallback(state, artifact_key_hex)?;
    cached_ref.last_used = Instant::now();
    ensure_payloads(&mut cached_ref, &state.artifact_dir, artifact_key_hex)?;
    let artifact_lookup_ns = t_artifact_lookup.elapsed().as_nanos() as u64;

    let payloads = Arc::clone(cached_ref.payloads.as_ref().unwrap());
    let names = Arc::clone(&cached_ref.meta.output_names);
    let exit_code = cached_ref.meta.exit_code;
    let stdout = cached_ref.stdout.clone();
    let stderr = cached_ref.stderr.clone();
    let artifact_bytes = cached_ref.meta.total_size;
    drop(cached_ref);

    let t_write_output = Instant::now();
    let targets: Vec<(NormalizedPath, NormalizedPath)> = (0..payloads.len())
        .map(|i| {
            let out: NormalizedPath = if i == 0 {
                output_path.clone()
            } else {
                secondary_output_dir.join(&names[i]).into()
            };
            let cache_file = state.artifact_dir.join(format!("{artifact_key_hex}_{i}"));
            (out, cache_file)
        })
        .collect();
    if !write_payloads_par(&targets, &payloads) {
        return None;
    }
    let write_output_ns = t_write_output.elapsed().as_nanos() as u64;

    let cached_error = exit_code != 0;
    if !cached_error && downgrade_output_metadata {
        state.cache_system.metadata().downgrade(output_path);
    }

    let t_bookkeeping = Instant::now();
    if record_compilation {
        state.stats.record_compilation();
    }
    let latency_ns = compile_start.elapsed().as_nanos() as u64;
    if cached_error {
        state.stats.record_cached_error();
        record_session_stat(&state.sessions, sid, |t| {
            t.record_cached_error();
        });
    } else {
        state.stats.record_hit(latency_ns, artifact_bytes);
        let src = source_path.clone();
        record_session_stat(&state.sessions, sid, move |t| {
            t.record_hit(src, latency_ns, artifact_bytes);
        });
    }
    write_session_log(
        &state.sessions,
        sid,
        &format!(
            "[{}] {} -> {}",
            if cached_error {
                cached_error_label
            } else {
                hit_label
            },
            source_path.display(),
            output_path.display()
        ),
    );
    let bookkeeping_ns = t_bookkeeping.elapsed().as_nanos() as u64;

    let total_ns = compile_start.elapsed().as_nanos() as u64;
    if !cached_error {
        state.profiler.record_hit(&HitPhases {
            parse_args_ns: phases.parse_args_ns,
            build_context_ns: phases.build_context_ns,
            hash_source_ns: phases.hash_source_ns,
            hash_headers_ns: phases.hash_headers_ns,
            depgraph_check_ns: phases.depgraph_check_ns,
            request_cache_lookup_ns: phases.request_cache_lookup_ns,
            cross_root_validate_ns: phases.cross_root_validate_ns,
            artifact_lookup_ns,
            write_output_ns,
            bookkeeping_ns,
            total_ns,
        });
    }

    Some(Response::CompileResult {
        exit_code,
        stdout,
        stderr,
        cached: true,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file_time(path: &Path) -> filetime::FileTime {
        filetime::FileTime::from_last_modification_time(&std::fs::metadata(path).unwrap())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn target_paths_keep_cache_mtime_through_shared_materializer() {
        let dir = tempfile::tempdir().unwrap();
        let server = DaemonServer::bind(&crate::ipc::unique_test_endpoint()).unwrap();
        let state = server.state.as_ref();
        let cache_dir = state.artifact_dir.clone();
        let source_path: NormalizedPath = dir.path().join("source.cc").into();
        let output_path: NormalizedPath = dir.path().join("output.o").into();
        let cache_path = cache_dir.join("artifact-key_0");
        let payload = Arc::new(b"compiled object".to_vec());
        std::fs::write(&cache_path, payload.as_slice()).unwrap();

        let old_time = filetime::FileTime::from_unix_time(1_000_000_000, 0);
        filetime::set_file_mtime(&cache_path, old_time).unwrap();

        let sid = state.sessions.create(crate::depgraph::SessionConfig {
            client_pid: std::process::id(),
            working_dir: dir.path().into(),
            log_file: None,
            track_stats: true,
            journal_path: None,
            profile: false,
        });
        let meta = ArtifactIndex::new(
            vec!["output.o".to_string()],
            vec![payload.len() as u64],
            Arc::new(Vec::new()),
            Arc::new(Vec::new()),
            0,
        );
        state.artifacts.insert(
            "artifact-key".to_string(),
            CachedArtifact::from_file_payloads(meta, vec![cache_path]),
        );

        let response = materialize_cached_compile_hit(CachedHitMaterializeRequest {
            state,
            sid: &sid,
            artifact_key_hex: "artifact-key",
            source_path: &source_path,
            output_path: &output_path,
            secondary_output_dir: dir.path().to_path_buf(),
            compile_start: Instant::now(),
            hit_label: "HIT_TEST",
            cached_error_label: "CACHED_ERROR_TEST",
            record_compilation: true,
            downgrade_output_metadata: true,
            phases: CachedHitPhases::request_cache(0, 0),
        })
        .unwrap();

        assert!(matches!(
            response,
            Response::CompileResult {
                cached: true,
                exit_code: 0,
                ..
            }
        ));
        assert_eq!(std::fs::read(&output_path).unwrap(), payload.as_slice());
        assert_eq!(
            file_time(&output_path).unix_seconds(),
            old_time.unix_seconds()
        );
        assert_eq!(state.stats.snapshot().compilations, 1);
        assert_eq!(state.stats.snapshot().hits, 1);
    }
}
