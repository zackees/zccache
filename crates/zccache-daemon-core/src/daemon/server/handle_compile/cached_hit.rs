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
    pub(super) secondary_output_dir: NormalizedPath,
    /// Issue #643: where the current build wants its depfile restored.
    ///
    /// When the user's compile line carries `-MD -MF <path>` (or `-MD` with
    /// an implicit `<output>.d`) and the cached artifact carries the
    /// depfile as its second payload, write payloads[1] to this path
    /// alongside writing payloads[0] to `output_path`. `None` on hits
    /// from compiles without depfile flags, and on artifacts cached before
    /// this fix landed (legacy single-output entries are honoured even
    /// when `Some(_)` is passed).
    pub(super) current_depfile_dest: Option<NormalizedPath>,
    pub(super) compile_start: Instant,
    pub(super) hit_label: &'static str,
    pub(super) cached_error_label: &'static str,
    pub(super) record_compilation: bool,
    pub(super) downgrade_output_metadata: bool,
    pub(super) mtime_floor_paths: Vec<NormalizedPath>,
    pub(super) rustc_metadata_compat_outputs: Option<Vec<NormalizedPath>>,
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
        current_depfile_dest,
        compile_start,
        hit_label,
        cached_error_label,
        record_compilation,
        downgrade_output_metadata,
        mtime_floor_paths,
        rustc_metadata_compat_outputs,
        phases,
    } = request;

    // Issue #460: collapse clock reads on the warm-hit path. Previously this
    // function did 9 `Instant::now()` reads per hit (4 explicit + 5 implicit
    // via `.elapsed()`); each costs ~50ns on Linux and ~500ns on Windows
    // (QueryPerformanceCounter). The phase split below now uses 4 clock
    // reads (`t0..t3`) and derives every `_ns` value by arithmetic — plus
    // drops the `cached_ref.last_used = Instant::now()` write which was a
    // dead store (no readers anywhere in the workspace).
    let t0 = Instant::now();
    let mut cached_ref = lookup_artifact_with_disk_fallback(state, artifact_key_hex)?;
    ensure_payloads(&mut cached_ref, &state.artifact_dir, artifact_key_hex)?;
    let t1 = Instant::now();
    let artifact_lookup_ns = (t1 - t0).as_nanos() as u64;

    // zccache#940: cache-hit "cache_load" sub-phase — the artifact index
    // lookup + payload read that materializes a cached hit. No-op unless this
    // compile runs inside an embedded `inner_trace::scope` with the trace env
    // set.
    crate::daemon::server::inner_trace::record_ns("cache_load", artifact_lookup_ns);

    let payloads = Arc::clone(cached_ref.payloads.as_ref()?);
    let names = Arc::clone(&cached_ref.meta.output_names);
    let exit_code = cached_ref.meta.exit_code;
    let stdout = cached_ref.stdout.clone();
    let stderr = cached_ref.stderr.clone();
    let artifact_bytes = cached_ref.meta.total_size;
    drop(cached_ref);

    // Issue #643: when the miss path stashed the user's depfile bytes as a
    // second output and the current request supplies a `-MF` destination,
    // restore index 1 to *that* destination — not to the cached basename
    // under `secondary_output_dir`. The two paths are deliberately
    // independent: the cached name is just a payload identifier (preserved
    // for legacy / non-depfile multi-output artifacts), while the on-disk
    // destination must come from the current build's args. Restoring to
    // the cached path would write a stale-named depfile that no current
    // build tool is looking for, leaving the user's `-MF` target absent
    // and reproducing the exact stale-incremental-build bug this fix
    // closes.
    let (targets, payloads_to_write): (Vec<(NormalizedPath, NormalizedPath)>, Vec<CachedPayload>) =
        if let Some(requested_outputs) = rustc_metadata_compat_outputs {
            let mut targets = Vec::with_capacity(requested_outputs.len());
            let mut selected_payloads = Vec::with_capacity(requested_outputs.len());
            for requested in requested_outputs {
                let Some(i) = rustc_compat_payload_index_for(&names, &requested) else {
                    write_session_log(
                        &state.sessions,
                        sid,
                        &format!(
                            "[DIAG] rustc_emit_compat_missing_output: {}",
                            requested.display()
                        ),
                    );
                    return None;
                };
                let cache_file = state.artifact_dir.join(format!("{artifact_key_hex}_{i}"));
                targets.push((requested, cache_file));
                selected_payloads.push(payloads[i].clone());
            }
            (targets, selected_payloads)
        } else {
            let targets = (0..payloads.len())
                .map(|i| {
                    let out: NormalizedPath = if i == 0 {
                        output_path.clone()
                    } else if i == 1 && payloads.len() == 2 {
                        current_depfile_dest
                            .clone()
                            .unwrap_or_else(|| secondary_output_dir.join(&names[i]))
                    } else {
                        secondary_output_dir.join(&names[i])
                    };
                    let cache_file = state.artifact_dir.join(format!("{artifact_key_hex}_{i}"));
                    (out, cache_file)
                })
                .collect();
            (targets, payloads.iter().cloned().collect())
        };
    if !write_payloads_par_with_mtime_floor(&targets, &payloads_to_write, &mtime_floor_paths) {
        return None;
    }
    let t2 = Instant::now();
    let write_output_ns = (t2 - t1).as_nanos() as u64;

    let cached_error = exit_code != 0;
    if !cached_error && downgrade_output_metadata {
        state.cache_system.metadata().downgrade(output_path);
    }

    if record_compilation {
        state.stats.record_compilation();
    }
    // `latency_ns` is the cache-hit response latency excluding bookkeeping
    // (record_hit / record_session_stat / write_session_log). Same boundary
    // as before — derived from `t2` instead of a fresh clock read.
    let latency_ns = (t2 - compile_start).as_nanos() as u64;
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
    let t3 = Instant::now();
    let bookkeeping_ns = (t3 - t2).as_nanos() as u64;

    let total_ns = (t3 - compile_start).as_nanos() as u64;
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

fn rustc_compat_payload_index_for(names: &[String], requested: &NormalizedPath) -> Option<usize> {
    let requested_name = requested.file_name()?.to_str()?;
    if let Some(index) = names.iter().position(|name| name == requested_name) {
        return Some(index);
    }
    let wanted = rustc_output_kind(requested)?;
    names
        .iter()
        .position(|name| rustc_output_kind(std::path::Path::new(name)) == Some(wanted))
}

fn rustc_output_kind(path: &std::path::Path) -> Option<&'static str> {
    match path.extension().and_then(|ext| ext.to_str()) {
        Some("rmeta") => Some("metadata"),
        Some("d") => Some("dep-info"),
        Some("o") => Some("obj"),
        Some("s") => Some("asm"),
        Some("ll") => Some("llvm-ir"),
        Some("bc") => Some("llvm-bc"),
        Some("mir") => Some("mir"),
        Some("rlib" | "a" | "exe" | "dll" | "so" | "dylib") | None => Some("link"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file_time(path: &Path) -> filetime::FileTime {
        filetime::FileTime::from_last_modification_time(&std::fs::metadata(path).unwrap())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn target_paths_get_fresh_mtime_through_shared_materializer() {
        let dir = tempfile::tempdir().unwrap();
        let server = DaemonServer::bind(&crate::ipc::unique_test_endpoint()).unwrap();
        let state = server.state.as_ref();
        let cache_dir = state.artifact_dir.clone();
        let source_path: NormalizedPath = dir.path().join("source.cc").into();
        let output_path: NormalizedPath = dir.path().join("output.o").into();
        let cache_path = cache_dir.join("artifact-key_0");
        let payload = Arc::new(b"compiled object".to_vec());
        let _ = make_writable(&cache_path);
        std::fs::write(&cache_path, payload.as_slice()).unwrap();
        write_authoritative_blob_digest(&cache_path).unwrap();

        let old_time = filetime::FileTime::from_unix_time(1_000_000_000, 0);
        filetime::set_file_mtime(&cache_path, old_time).unwrap();

        let sid = state.sessions.create(crate::depgraph::SessionConfig {
            client_pid: std::process::id(),
            working_dir: dir.path().into(),
            log_file: None,
            track_stats: true,
            journal_path: None,
            profile: false,
            private_env: Vec::new(),
            owner_pids: Vec::new(),
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
            secondary_output_dir: dir.path().into(),
            current_depfile_dest: None,
            compile_start: Instant::now(),
            hit_label: "HIT_TEST",
            cached_error_label: "CACHED_ERROR_TEST",
            record_compilation: true,
            downgrade_output_metadata: true,
            mtime_floor_paths: Vec::new(),
            rustc_metadata_compat_outputs: None,
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
        let output_time = file_time(&output_path);
        assert!(
            output_time.unix_seconds() > old_time.unix_seconds(),
            "compile-hit output must be fresher than stale cache artifact; \
             output={output_time:?}, cache={old_time:?}",
        );
        assert_eq!(state.stats.snapshot().compilations, 1);
        assert_eq!(state.stats.snapshot().hits, 1);
    }

    /// Issue #643: when zccache wraps `clang++ -MD -MF <depfile>` the user's
    /// depfile is part of the build-system's incremental-rebuild contract
    /// (e.g. `deps = gcc` in ninja). On a cache hit we currently restore
    /// only the `.obj` — the `.d` is silently absent, the build tool records
    /// zero dependencies for the object, and from then on it never
    /// recompiles when included headers change. Result: stale objects and
    /// mysterious `undefined symbol` link errors after `git pull`.
    ///
    /// This test pins the fix: a cached artifact with two payloads (`.obj`
    /// at index 0, `.d` at index 1) plus an explicit current-build depfile
    /// destination must restore BOTH files. The cached `name` of the
    /// depfile output is just an identifier — the real destination on hit
    /// is supplied by the caller (it comes from the current compile's
    /// `-MF` argument, not from where the depfile happened to live when
    /// the cache miss recorded it).
    #[tokio::test(flavor = "current_thread")]
    async fn cached_hit_restores_user_depfile_alongside_object() {
        let dir = tempfile::tempdir().unwrap();
        let server = DaemonServer::bind(&crate::ipc::unique_test_endpoint()).unwrap();
        let state = server.state.as_ref();
        let cache_dir = state.artifact_dir.clone();

        let source_path: NormalizedPath = dir.path().join("source.cc").into();
        let output_path: NormalizedPath = dir.path().join("source.o").into();
        // Critical: the destination depfile path used by *this* build is
        // not necessarily the cached basename. The caller derives it from
        // the current invocation's `-MF` (or default `<output>.d`) and
        // passes it in. Use a different filename to prove the fix routes
        // bytes by request, not by stored name.
        let depfile_dest: NormalizedPath = dir.path().join("build/out/source.o.d").into();

        let obj_payload = Arc::new(b"compiled object bytes".to_vec());
        let dep_payload = Arc::new(
            b"source.o: source.cc header_a.h header_b.h\n\nheader_a.h:\n\nheader_b.h:\n".to_vec(),
        );
        let obj_cache_path = cache_dir.join("depfile-key_0");
        let dep_cache_path = cache_dir.join("depfile-key_1");
        let _ = make_writable(&obj_cache_path);
        let _ = make_writable(&dep_cache_path);
        std::fs::write(&obj_cache_path, obj_payload.as_slice()).unwrap();
        std::fs::write(&dep_cache_path, dep_payload.as_slice()).unwrap();
        write_authoritative_blob_digest(&obj_cache_path).unwrap();
        write_authoritative_blob_digest(&dep_cache_path).unwrap();

        let sid = state.sessions.create(crate::depgraph::SessionConfig {
            client_pid: std::process::id(),
            working_dir: dir.path().into(),
            log_file: None,
            track_stats: true,
            journal_path: None,
            profile: false,
            private_env: Vec::new(),
            owner_pids: Vec::new(),
        });

        // The cached `name[1]` is the basename from the original miss
        // (e.g. "source.o.d"). The destination on hit is independent and
        // comes from the current request.
        let meta = ArtifactIndex::new(
            vec!["source.o".to_string(), "source.o.d".to_string()],
            vec![obj_payload.len() as u64, dep_payload.len() as u64],
            Arc::new(Vec::new()),
            Arc::new(Vec::new()),
            0,
        );
        state.artifacts.insert(
            "depfile-key".to_string(),
            CachedArtifact::from_file_payloads(meta, vec![obj_cache_path, dep_cache_path]),
        );

        let response = materialize_cached_compile_hit(CachedHitMaterializeRequest {
            state,
            sid: &sid,
            artifact_key_hex: "depfile-key",
            source_path: &source_path,
            output_path: &output_path,
            secondary_output_dir: dir.path().into(),
            current_depfile_dest: Some(depfile_dest.clone()),
            compile_start: Instant::now(),
            hit_label: "HIT_TEST",
            cached_error_label: "CACHED_ERROR_TEST",
            record_compilation: true,
            downgrade_output_metadata: true,
            mtime_floor_paths: Vec::new(),
            rustc_metadata_compat_outputs: None,
            phases: CachedHitPhases::request_cache(0, 0),
        })
        .expect("materialize_cached_compile_hit must succeed");
        assert!(matches!(
            response,
            Response::CompileResult {
                cached: true,
                exit_code: 0,
                ..
            }
        ));

        assert_eq!(
            std::fs::read(&output_path).unwrap(),
            obj_payload.as_slice(),
            "cache hit must restore the object at its destination",
        );
        assert!(
            depfile_dest.as_path().exists(),
            "cache hit must restore the depfile at the *current* build's -MF \
             destination ({}), not the cached basename — this is the #643 \
             stale-incremental-build fix",
            depfile_dest.display(),
        );
        assert_eq!(
            std::fs::read(depfile_dest.as_path()).unwrap(),
            dep_payload.as_slice(),
            "restored depfile bytes must match the cached payload",
        );
    }

    /// Legacy contract: a 1-output cached artifact (no depfile recorded
    /// at miss time, e.g. compiles without `-MD`/`-MF`) must keep working
    /// even when the current request happens to supply a
    /// `current_depfile_dest`. The fix must not regress the
    /// pre-#643-store-format hit path.
    #[tokio::test(flavor = "current_thread")]
    async fn cached_hit_object_only_artifact_ignores_depfile_dest() {
        let dir = tempfile::tempdir().unwrap();
        let server = DaemonServer::bind(&crate::ipc::unique_test_endpoint()).unwrap();
        let state = server.state.as_ref();
        let cache_dir = state.artifact_dir.clone();

        let source_path: NormalizedPath = dir.path().join("source.cc").into();
        let output_path: NormalizedPath = dir.path().join("source.o").into();
        let depfile_dest: NormalizedPath = dir.path().join("source.o.d").into();

        let obj_payload = Arc::new(b"object only".to_vec());
        let cache_path = cache_dir.join("legacy-key_0");
        let _ = make_writable(&cache_path);
        std::fs::write(&cache_path, obj_payload.as_slice()).unwrap();
        write_authoritative_blob_digest(&cache_path).unwrap();

        let sid = state.sessions.create(crate::depgraph::SessionConfig {
            client_pid: std::process::id(),
            working_dir: dir.path().into(),
            log_file: None,
            track_stats: true,
            journal_path: None,
            profile: false,
            private_env: Vec::new(),
            owner_pids: Vec::new(),
        });

        let meta = ArtifactIndex::new(
            vec!["source.o".to_string()],
            vec![obj_payload.len() as u64],
            Arc::new(Vec::new()),
            Arc::new(Vec::new()),
            0,
        );
        state.artifacts.insert(
            "legacy-key".to_string(),
            CachedArtifact::from_file_payloads(meta, vec![cache_path]),
        );

        let response = materialize_cached_compile_hit(CachedHitMaterializeRequest {
            state,
            sid: &sid,
            artifact_key_hex: "legacy-key",
            source_path: &source_path,
            output_path: &output_path,
            secondary_output_dir: dir.path().into(),
            current_depfile_dest: Some(depfile_dest.clone()),
            compile_start: Instant::now(),
            hit_label: "HIT_TEST",
            cached_error_label: "CACHED_ERROR_TEST",
            record_compilation: true,
            downgrade_output_metadata: false,
            mtime_floor_paths: Vec::new(),
            rustc_metadata_compat_outputs: None,
            phases: CachedHitPhases::request_cache(0, 0),
        })
        .expect("legacy single-output hit must still succeed");
        assert!(matches!(
            response,
            Response::CompileResult {
                cached: true,
                exit_code: 0,
                ..
            }
        ));
        assert_eq!(std::fs::read(&output_path).unwrap(), obj_payload.as_slice());
        assert!(
            !depfile_dest.as_path().exists(),
            "legacy single-output artifact must NOT manufacture a depfile",
        );
    }

    /// Issue #460: warm-hit materialization should stay under budget — the
    /// fix collapsed 9 clock reads per hit to 4. A future regression that
    /// reintroduces a syscall-per-phase pattern (or worse, a synchronous I/O
    /// call) on the hit path would bust this budget. 100 iterations / 1 s
    /// gives ~50× headroom on Linux Docker and ~5× on Windows CI (Defender +
    /// shared-runner jitter typically lands warm-hit timings around 2 ms each
    /// on those runners; native Windows hosts measure ~150–250 µs/hit).
    #[tokio::test(flavor = "current_thread")]
    async fn warm_hit_materialization_under_budget() {
        let dir = tempfile::tempdir().unwrap();
        let server = DaemonServer::bind(&crate::ipc::unique_test_endpoint()).unwrap();
        let state = server.state.as_ref();
        let cache_dir = state.artifact_dir.clone();
        let source_path: NormalizedPath = dir.path().join("source.cc").into();
        let cache_path = cache_dir.join("budget-key_0");
        let payload = Arc::new(b"compiled object".to_vec());
        let _ = make_writable(&cache_path);
        std::fs::write(&cache_path, payload.as_slice()).unwrap();
        write_authoritative_blob_digest(&cache_path).unwrap();

        let sid = state.sessions.create(crate::depgraph::SessionConfig {
            client_pid: std::process::id(),
            working_dir: dir.path().into(),
            log_file: None,
            track_stats: true,
            journal_path: None,
            profile: false,
            private_env: Vec::new(),
            owner_pids: Vec::new(),
        });
        let meta = ArtifactIndex::new(
            vec!["output.o".to_string()],
            vec![payload.len() as u64],
            Arc::new(Vec::new()),
            Arc::new(Vec::new()),
            0,
        );
        state.artifacts.insert(
            "budget-key".to_string(),
            CachedArtifact::from_file_payloads(meta, vec![cache_path]),
        );

        const ITERATIONS: u32 = 100;
        let start = Instant::now();
        for i in 0..ITERATIONS {
            let output_path: NormalizedPath = dir.path().join(format!("out-{i}.o")).into();
            let response = materialize_cached_compile_hit(CachedHitMaterializeRequest {
                state,
                sid: &sid,
                artifact_key_hex: "budget-key",
                source_path: &source_path,
                output_path: &output_path,
                secondary_output_dir: dir.path().into(),
                current_depfile_dest: None,
                compile_start: Instant::now(),
                hit_label: "HIT_TEST",
                cached_error_label: "CACHED_ERROR_TEST",
                record_compilation: true,
                downgrade_output_metadata: false,
                mtime_floor_paths: Vec::new(),
                rustc_metadata_compat_outputs: None,
                phases: CachedHitPhases::request_cache(0, 0),
            })
            .expect("materialize_cached_compile_hit must succeed");
            assert!(matches!(
                response,
                Response::CompileResult {
                    cached: true,
                    exit_code: 0,
                    ..
                }
            ));
        }
        let elapsed = start.elapsed();
        let budget = if cfg!(windows) {
            std::time::Duration::from_secs(2)
        } else {
            std::time::Duration::from_secs(1)
        };
        assert!(
            elapsed < budget,
            "warm-hit materialization regressed: {ITERATIONS} hits took {elapsed:?} \
             (budget: {budget:?}; avg {:?}/hit)",
            elapsed / ITERATIONS
        );
        assert_eq!(state.stats.snapshot().hits as u32, ITERATIONS);
    }
}
