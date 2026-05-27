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
    let artifact_key_result = state.dep_graph.update(context_key, scan_result, get_hash);
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
                state,
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
    state: &SharedState,
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
    let t_artifact_meta_build = Instant::now();
    let artifact_bytes: u64 = all_outputs.iter().map(|o| o.size).sum();
    let output_names: Vec<String> = all_outputs.iter().map(|o| o.name.clone()).collect();
    let output_sizes: Vec<u64> = all_outputs.iter().map(|o| o.size).collect();
    let payload_paths: Vec<NormalizedPath> = (0..all_outputs.len())
        .map(|i| state.artifact_dir.join(format!("{artifact_key_hex}_{i}")))
        .collect();
    stats.artifact_meta_build_ns = t_artifact_meta_build.elapsed().as_nanos() as u64;

    let mut snapshot_ok = true;
    let t_rust_snapshot = Instant::now();
    for (output, cache_path) in all_outputs.iter().zip(payload_paths.iter()) {
        match persist_artifact_file(cache_path, &output.path) {
            Ok(snapshot_stats) => {
                stats.rust_snapshot_hardlink_count += snapshot_stats.hardlink_count;
                stats.rust_snapshot_copy_count += snapshot_stats.copy_count;
                stats.rust_snapshot_copy_bytes += snapshot_stats.copy_bytes;
            }
            Err(e) => {
                stats.rust_snapshot_error_count += 1;
                snapshot_ok = false;
                tracing::warn!(
                    source = %output.path.display(),
                    cache = %cache_path.display(),
                    "failed to snapshot rustc output: {e}"
                );
                break;
            }
        }
    }
    stats.rust_snapshot_ns = t_rust_snapshot.elapsed().as_nanos() as u64;
    stats.artifact_build_ns = t_artifact_build.elapsed().as_nanos() as u64;

    let t_artifact_insert_stats = Instant::now();
    if snapshot_ok {
        let t_artifact_index_build = Instant::now();
        let meta = ArtifactIndex::new(
            output_names,
            output_sizes,
            Arc::clone(stdout),
            Arc::clone(stderr),
            exit_code,
        );
        stats.artifact_index_build_ns = t_artifact_index_build.elapsed().as_nanos() as u64;
        let t_artifact_index_persist = Instant::now();
        state.artifact_store.insert(artifact_key_hex, &meta);
        stats.artifact_index_persist_ns = t_artifact_index_persist.elapsed().as_nanos() as u64;
        let t_artifact_memory_insert = Instant::now();
        let cached = CachedArtifact::from_file_payloads(meta, payload_paths);
        state.artifacts.insert(artifact_key_hex.to_string(), cached);
        stats.artifact_memory_insert_ns = t_artifact_memory_insert.elapsed().as_nanos() as u64;
    }

    let latency_ns = compile_start.elapsed().as_nanos() as u64;
    let recorded_bytes = if snapshot_ok { artifact_bytes } else { 0 };
    state.stats.record_miss(latency_ns, recorded_bytes);
    let src = source_path.clone();
    record_session_stat(&state.sessions, sid, move |t| {
        t.record_miss(src, recorded_bytes);
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
    artifact_key_hex: &str,
    stdout: &Arc<Vec<u8>>,
    stderr: &Arc<Vec<u8>>,
    exit_code: i32,
    compile_start: Instant,
    stats: &mut MissArtifactStoreStats,
    t_artifact_build: Instant,
) {
    let state = state_arc.as_ref();
    let artifact = ArtifactData {
        outputs: vec![ArtifactOutput {
            name: output_path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned(),
            payload: ArtifactPayload::Bytes(Arc::new(output_data)),
        }],
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
    let source_paths: Vec<NormalizedPath> = vec![output_path.clone()];
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
