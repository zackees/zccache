//! Cold-miss artifact storage for compile requests.

use super::super::*;

pub(super) struct MissArtifactStoreRequest<'a> {
    pub(super) state_arc: &'a Arc<SharedState>,
    pub(super) sid: &'a SessionId,
    pub(super) context_key: &'a ContextKey,
    pub(super) source_path: &'a NormalizedPath,
    pub(super) output_path: &'a NormalizedPath,
    pub(super) scan_result: crate::depgraph::ScanResult,
    pub(super) hash_map: &'a HashMap<NormalizedPath, ContentHash>,
    pub(super) output_data: Vec<u8>,
    /// Issue #643: when the user's compile line emitted a depfile that
    /// downstream build tools depend on (`-MD -MF <path>` or `-MD` with
    /// the implicit `<output>.d`), the post-compile depfile bytes are
    /// captured here so the cache hit can restore the depfile alongside
    /// the object. `None` for compiles without user depfile flags, for
    /// MSVC `/showIncludes` (parsed from stderr, not on disk), and for
    /// rustc (separate persist path).
    pub(super) user_depfile: Option<(NormalizedPath, Vec<u8>)>,
    pub(super) rustc_all_outputs: Option<&'a [RustcOutputFile]>,
    pub(super) stdout: &'a Arc<Vec<u8>>,
    pub(super) stderr: &'a Arc<Vec<u8>>,
    pub(super) exit_code: i32,
    pub(super) compile_start: Instant,
}

#[derive(Default)]
pub(super) struct MissArtifactStoreStats {
    pub(super) artifact_store_ns: u64,
    pub(super) depgraph_update_ns: u64,
    pub(super) artifact_build_ns: u64,
    pub(super) persist_enqueue_ns: u64,
    pub(super) artifact_insert_stats_ns: u64,
    pub(super) artifact_meta_build_ns: u64,
    pub(super) rust_snapshot_ns: u64,
    pub(super) rust_snapshot_hardlink_count: u64,
    pub(super) rust_snapshot_copy_count: u64,
    pub(super) rust_snapshot_copy_bytes: u64,
    pub(super) rust_snapshot_error_count: u64,
    pub(super) artifact_index_build_ns: u64,
    pub(super) artifact_index_persist_ns: u64,
    pub(super) artifact_memory_insert_ns: u64,
}

pub(super) fn store_miss_artifact(request: MissArtifactStoreRequest<'_>) -> MissArtifactStoreStats {
    let MissArtifactStoreRequest {
        state_arc,
        sid,
        context_key,
        source_path,
        output_path,
        scan_result,
        hash_map,
        output_data,
        user_depfile,
        rustc_all_outputs,
        stdout,
        stderr,
        exit_code,
        compile_start,
    } = request;
    let state = state_arc.as_ref();
    let t_store = Instant::now();
    let get_hash = |p: &Path| {
        let path = NormalizedPath::new(p);
        hash_map.get(&path).copied()
    };
    let include_count = scan_result.resolved.len();
    let t_depgraph_update = Instant::now();
    let artifact_key_result = state
        .dep_graph
        .load()
        .update(context_key, scan_result, get_hash);
    let mut stats = MissArtifactStoreStats {
        depgraph_update_ns: t_depgraph_update.elapsed().as_nanos() as u64,
        ..MissArtifactStoreStats::default()
    };

    if let Some(artifact_key) = artifact_key_result {
        let artifact_key_hex = artifact_key.hash().to_hex();
        let ctx_hex = &context_key.hash().to_hex()[..8];
        write_session_log(
            &state.sessions,
            sid,
            &format!(
                "[DIAG] update: {} ctx={ctx_hex} artifact_key={} includes={include_count}",
                source_path.display(),
                &artifact_key_hex[..8],
            ),
        );

        record_pch_source_mapping(state, source_path, output_path);

        let t_artifact_build = Instant::now();
        if let Some(all_outputs) = rustc_all_outputs {
            store_rustc_outputs(
                state_arc,
                sid,
                source_path,
                all_outputs,
                &artifact_key_hex,
                stdout,
                stderr,
                exit_code,
                compile_start,
                &mut stats,
                t_artifact_build,
            );
        } else {
            store_single_output(
                state_arc,
                sid,
                source_path,
                output_path,
                output_data,
                user_depfile,
                &artifact_key_hex,
                stdout,
                stderr,
                exit_code,
                compile_start,
                &mut stats,
                t_artifact_build,
            );
        }
    }

    stats.artifact_store_ns = t_store.elapsed().as_nanos() as u64;
    stats
}

fn record_pch_source_mapping(
    state: &SharedState,
    source_path: &NormalizedPath,
    output_path: &NormalizedPath,
) {
    if let Some(ext) = output_path.extension() {
        if ext == "pch" || ext == "gch" {
            state
                .pch_source_map
                .insert(output_path.clone(), source_path.clone());
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn store_rustc_outputs(
    state_arc: &Arc<SharedState>,
    sid: &SessionId,
    source_path: &NormalizedPath,
    all_outputs: &[RustcOutputFile],
    artifact_key_hex: &str,
    stdout: &Arc<Vec<u8>>,
    stderr: &Arc<Vec<u8>>,
    exit_code: i32,
    compile_start: Instant,
    stats: &mut MissArtifactStoreStats,
    t_artifact_build: Instant,
) {
    let state = state_arc.as_ref();
    let t_artifact_meta_build = Instant::now();
    // Issue #629: the prior four-pass shape (`.iter().map().sum()`
    // + three `.iter().map().collect()`s) walks `all_outputs` four
    // times and allocates three Vecs whose capacity wasn't hinted.
    // For the typical rustc miss (2 outputs: `.rmeta` + `.rlib`) the
    // savings are micro, but every µs on the daemon's
    // response-return critical path stacks against the same-job-seed
    // warm gap soldr is chasing in #629. Single pass with
    // `with_capacity` hint and a `saturating_add` accumulator.
    let n = all_outputs.len();
    let mut output_names: Vec<String> = Vec::with_capacity(n);
    let mut output_sizes: Vec<u64> = Vec::with_capacity(n);
    let mut source_paths: Vec<NormalizedPath> = Vec::with_capacity(n);
    let mut artifact_bytes: u64 = 0;
    for output in all_outputs {
        output_names.push(output.name.clone());
        output_sizes.push(output.size);
        source_paths.push(output.path.clone());
        artifact_bytes = artifact_bytes.saturating_add(output.size);
    }
    stats.artifact_meta_build_ns = t_artifact_meta_build.elapsed().as_nanos() as u64;

    // Issue #632: move the rust miss persist OFF the daemon's
    // response-return critical path. Insert a `PendingFile` artifact
    // into the in-memory store immediately so the CLI gets its response
    // as soon as the protocol layer can serialize it; spawn the
    // hardlink + atomic-rename + index-writer work on the daemon's
    // existing persist semaphore + blocking pool. A hit lookup that
    // arrives during the persist window finds `PendingFile` and falls
    // back to `output.path` (the rustc-output path under `target/`);
    // once `persist_artifact_paths_with_stats` completes, both paths
    // are the same inode and the cache_path fast path takes over.
    //
    // Mirrors the `store_single_output` tokio::spawn pattern (C/C++
    // miss path), but uses on-disk source paths instead of in-memory
    // bytes (rustc outputs can be tens of MB; reading them just to
    // re-write them would re-introduce the foreground read this whole
    // module is structured to avoid).
    let t_artifact_index_build = Instant::now();
    let meta = ArtifactIndex::new(
        output_names,
        output_sizes,
        Arc::clone(stdout),
        Arc::clone(stderr),
        exit_code,
    );
    stats.artifact_index_build_ns = t_artifact_index_build.elapsed().as_nanos() as u64;
    stats.artifact_build_ns = t_artifact_build.elapsed().as_nanos() as u64;

    let t_persist_enqueue = Instant::now();
    let artifact_dir = state.artifact_dir.clone();
    let key_hex = artifact_key_hex.to_string();
    let persist_meta = meta.clone();
    let persist_source_paths = source_paths.clone();
    let sem = Arc::clone(&state.persist_semaphore);
    let state_ref = Arc::clone(state_arc);
    let key_for_warn = key_hex.clone();
    tokio::spawn(async move {
        let _permit = sem.acquire().await.unwrap();
        let written = tokio::task::spawn_blocking(move || {
            let persist_result =
                persist_artifact_paths_with_stats(&artifact_dir, &key_hex, &persist_source_paths);
            (key_hex, persist_meta, persist_result)
        })
        .await;
        match written {
            Ok((key_hex, meta, Ok(_snapshot_stats))) => {
                let _ = state_ref.index_writer_tx.send((key_hex, meta));
            }
            Ok((key_hex, _meta, Err(e))) => {
                tracing::warn!(
                    key = %key_hex,
                    "failed to persist rustc artifact outputs: {e}"
                );
                // Drop the in-memory entry so subsequent hits don't
                // chase a half-persisted artifact whose `source_path`
                // fallback may already be stale (cargo clean / target
                // wipe). The next compile re-misses cleanly.
                state_ref.artifacts.remove(&key_hex);
            }
            Err(join_err) => {
                tracing::warn!(
                    key = %key_for_warn,
                    "rustc artifact persist task aborted: {join_err}"
                );
                state_ref.artifacts.remove(&key_for_warn);
            }
        }
    });
    stats.persist_enqueue_ns = t_persist_enqueue.elapsed().as_nanos() as u64;
    // The synchronous-snapshot stats fields are zero in async mode —
    // the per-file hardlink/copy counters are now produced inside the
    // spawned task and not observable on the request path. Leave them
    // at default so RustMissProfile readers see "persist work moved
    // off critical path" rather than stale per-call counts.
    stats.rust_snapshot_ns = 0;

    let t_artifact_insert_stats = Instant::now();
    let t_artifact_memory_insert = Instant::now();
    let cached = CachedArtifact::from_pending_payloads(meta, source_paths);
    state.artifacts.insert(artifact_key_hex.to_string(), cached);
    stats.artifact_memory_insert_ns = t_artifact_memory_insert.elapsed().as_nanos() as u64;

    let latency_ns = compile_start.elapsed().as_nanos() as u64;
    state.stats.record_miss(latency_ns, artifact_bytes);
    let src = source_path.clone();
    record_session_stat(&state.sessions, sid, move |t| {
        t.record_miss(src, artifact_bytes);
    });
    stats.artifact_insert_stats_ns = t_artifact_insert_stats.elapsed().as_nanos() as u64;
}

#[allow(clippy::too_many_arguments)]
fn store_single_output(
    state_arc: &Arc<SharedState>,
    sid: &SessionId,
    source_path: &NormalizedPath,
    output_path: &NormalizedPath,
    output_data: Vec<u8>,
    user_depfile: Option<(NormalizedPath, Vec<u8>)>,
    artifact_key_hex: &str,
    stdout: &Arc<Vec<u8>>,
    stderr: &Arc<Vec<u8>>,
    exit_code: i32,
    compile_start: Instant,
    stats: &mut MissArtifactStoreStats,
    t_artifact_build: Instant,
) {
    let state = state_arc.as_ref();
    // Issue #643: stash the user's depfile as a second output so cache
    // hits can restore it alongside the object. Only `UserSpecified` /
    // `UserDefault` strategies reach this site with `Some(_)` — the
    // pipeline filters out the `Injected` strategy (zccache injected
    // the file purely for its own depgraph use; the user didn't ask
    // for it on disk) and MSVC `/showIncludes` (no on-disk depfile to
    // begin with). The cached `name` is the depfile basename; the
    // destination on hit is supplied independently by the caller (the
    // current build's `-MF` value), so artifacts remain reusable
    // across renamed-output workspaces.
    let mut outputs = vec![ArtifactOutput {
        name: output_path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned(),
        payload: ArtifactPayload::Bytes(Arc::new(output_data)),
    }];
    let depfile_source_path: Option<NormalizedPath> = user_depfile.as_ref().map(|(p, _)| p.clone());
    if let Some((dep_path, dep_bytes)) = user_depfile {
        outputs.push(ArtifactOutput {
            name: dep_path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned(),
            payload: ArtifactPayload::Bytes(Arc::new(dep_bytes)),
        });
    }
    let artifact = ArtifactData {
        outputs,
        stdout: Arc::clone(stdout),
        stderr: Arc::clone(stderr),
        exit_code,
    };

    let artifact_bytes: u64 = artifact
        .outputs
        .iter()
        .map(|o| o.payload.size_bytes())
        .sum();
    let cached = CachedArtifact::from_artifact_data(&artifact);
    stats.artifact_build_ns = t_artifact_build.elapsed().as_nanos() as u64;
    let t_persist_enqueue = Instant::now();

    let artifact_dir = state.artifact_dir.clone();
    let key_hex = artifact_key_hex.to_string();
    let persist_meta = cached.meta.clone();
    let mut source_paths: Vec<NormalizedPath> = vec![output_path.clone()];
    if let Some(dep_path) = depfile_source_path {
        source_paths.push(dep_path);
    }
    let payload_size: usize = artifact
        .outputs
        .iter()
        .map(|o| o.payload.size_bytes() as usize)
        .sum();
    state
        .in_flight_bytes
        .fetch_add(payload_size, Ordering::Relaxed);
    let guard = InFlightGuard {
        state: Arc::clone(state_arc),
        size: payload_size,
    };
    let sem = Arc::clone(&state.persist_semaphore);
    let state_ref = Arc::clone(state_arc);
    tokio::spawn(async move {
        let _permit = sem.acquire().await.unwrap();
        let written = tokio::task::spawn_blocking(move || {
            let _guard = guard;
            if let Err(e) = persist_artifact_paths(&artifact_dir, &key_hex, &source_paths) {
                tracing::warn!(
                    key = %key_hex,
                    "failed to persist artifact output: {e}"
                );
            }
            (key_hex, persist_meta)
        })
        .await;
        if let Ok((key_hex, meta)) = written {
            let _ = state_ref.index_writer_tx.send((key_hex, meta));
        }
    });
    stats.persist_enqueue_ns = t_persist_enqueue.elapsed().as_nanos() as u64;

    let t_artifact_insert_stats = Instant::now();
    state.artifacts.insert(artifact_key_hex.to_string(), cached);

    let latency_ns = compile_start.elapsed().as_nanos() as u64;
    state.stats.record_miss(latency_ns, artifact_bytes);
    let src = source_path.clone();
    record_session_stat(&state.sessions, sid, move |t| {
        t.record_miss(src, artifact_bytes);
    });
    stats.artifact_insert_stats_ns = t_artifact_insert_stats.elapsed().as_nanos() as u64;
}
