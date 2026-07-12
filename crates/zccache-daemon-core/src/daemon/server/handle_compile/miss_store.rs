//! Cold-miss artifact storage for compile requests.

use super::super::*;

pub(super) struct MissArtifactStoreRequest<'a> {
    pub(super) state_arc: &'a Arc<SharedState>,
    pub(super) sid: &'a SessionId,
    pub(super) context_key: &'a ContextKey,
    pub(super) source_path: &'a NormalizedPath,
    pub(super) output_path: &'a NormalizedPath,
    pub(super) scan_result: crate::depgraph::ScanResult,
    /// Rustc env-dep `(name, value)` pairs resolved from the request env
    /// (zccache#1021). Empty for C/C++ and env-free rustc crates.
    pub(super) rustc_env_dep_values: Vec<(String, Option<String>)>,
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
    pub(super) synchronous_persist: bool,
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
        rustc_env_dep_values,
        hash_map,
        output_data,
        user_depfile,
        rustc_all_outputs,
        stdout,
        stderr,
        exit_code,
        compile_start,
        synchronous_persist,
    } = request;
    let state = state_arc.as_ref();
    let t_store = Instant::now();
    let get_hash = |p: &Path| {
        let path = NormalizedPath::new(p);
        hash_map.get(&path).copied()
    };
    let include_count = scan_result.resolved.len();
    let t_depgraph_update = Instant::now();
    let env_dep_names: Vec<String> = rustc_env_dep_values
        .iter()
        .map(|(name, _)| name.clone())
        .collect();
    let artifact_key_result = state.dep_graph.load().update_with_env(
        context_key,
        scan_result,
        get_hash,
        &env_dep_names,
        |name| {
            rustc_env_dep_values
                .iter()
                .find(|(n, _)| n == name)
                .and_then(|(_, v)| v.clone())
        },
    );
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
                synchronous_persist,
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
                synchronous_persist,
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
    synchronous_persist: bool,
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

    // Rustc outputs are already on disk under target/. Persist them before
    // publishing the in-memory artifact so depgraph hits never point at a
    // key whose payload files have not landed yet.
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

    let t_persist_sync = Instant::now();
    let sync_persist_result =
        persist_artifact_paths_with_stats(&state.artifact_dir, artifact_key_hex, &source_paths);
    stats.rust_snapshot_ns = t_persist_sync.elapsed().as_nanos() as u64;
    let persisted = match sync_persist_result {
        Ok(snapshot_stats) => {
            if snapshot_stats.staged {
                use crate::daemon::staged_stats::{StagedBytes, StagedCounter, StagedTiming};
                state
                    .profiler
                    .staged
                    .count(StagedCounter::PublicationSuccess);
                state
                    .profiler
                    .staged
                    .timing(StagedTiming::Hashing, snapshot_stats.staged_hash_ns);
                state.profiler.staged.timing(
                    StagedTiming::Publication,
                    snapshot_stats.staged_publication_ns,
                );
                state
                    .profiler
                    .staged
                    .bytes(StagedBytes::Publication, snapshot_stats.copy_bytes);
            }
            stats.rust_snapshot_hardlink_count = snapshot_stats.hardlink_count;
            stats.rust_snapshot_copy_count = snapshot_stats.copy_count;
            stats.rust_snapshot_copy_bytes = snapshot_stats.copy_bytes;
            let _ = state.index_writer_tx.send(IndexWriterCommand::Insert(
                artifact_key_hex.to_string(),
                meta.clone(),
            ));
            true
        }
        Err(e) => {
            if synchronous_persist {
                use crate::daemon::staged_stats::{StagedCounter, StagedFailure};
                state
                    .profiler
                    .staged
                    .count(StagedCounter::PublicationFailure);
                if e.kind() == std::io::ErrorKind::AlreadyExists {
                    state
                        .profiler
                        .staged
                        .count(StagedCounter::PublicationConflict);
                    state
                        .profiler
                        .staged
                        .failure(StagedFailure::PublicationConflict);
                } else {
                    state.profiler.staged.failure(StagedFailure::Publication);
                }
            }
            stats.rust_snapshot_error_count = stats.rust_snapshot_error_count.saturating_add(1);
            tracing::warn!(
                key = %artifact_key_hex,
                "failed to synchronously persist rustc artifact outputs: {e}"
            );
            write_session_log(
                &state.sessions,
                sid,
                &format!("[DIAG] rustc_persist_failed: key={artifact_key_hex} error={e}"),
            );
            false
        }
    };

    stats.persist_enqueue_ns = 0;

    let t_artifact_insert_stats = Instant::now();
    if persisted {
        let t_artifact_memory_insert = Instant::now();
        let cached = CachedArtifact::from_index(meta);
        state.artifacts.insert(artifact_key_hex.to_string(), cached);
        stats.artifact_memory_insert_ns = t_artifact_memory_insert.elapsed().as_nanos() as u64;
    }

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
    synchronous_persist: bool,
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
    if synchronous_persist {
        use crate::daemon::staged_stats::{
            StagedBytes, StagedCounter, StagedFailure, StagedTiming,
        };
        let staged_publication =
            staged_artifacts_enabled() && staged_key_supported(&key_hex) && !pack_mode_enabled();
        let written =
            match persist_artifact_paths_with_stats(&artifact_dir, &key_hex, &source_paths) {
                Ok(persisted) => {
                    if persisted.staged {
                        state
                            .profiler
                            .staged
                            .count(StagedCounter::PublicationSuccess);
                        state
                            .profiler
                            .staged
                            .timing(StagedTiming::Hashing, persisted.staged_hash_ns);
                        state
                            .profiler
                            .staged
                            .timing(StagedTiming::Publication, persisted.staged_publication_ns);
                        state
                            .profiler
                            .staged
                            .bytes(StagedBytes::Publication, persisted.copy_bytes);
                    }
                    true
                }
                Err(error) => {
                    stats.rust_snapshot_error_count =
                        stats.rust_snapshot_error_count.saturating_add(1);
                    if staged_publication {
                        state
                            .profiler
                            .staged
                            .count(StagedCounter::PublicationFailure);
                        if error.kind() == std::io::ErrorKind::AlreadyExists {
                            state
                                .profiler
                                .staged
                                .count(StagedCounter::PublicationConflict);
                            state
                                .profiler
                                .staged
                                .failure(StagedFailure::PublicationConflict);
                        } else {
                            state.profiler.staged.failure(StagedFailure::Publication);
                        }
                    }
                    false
                }
            };
        if written {
            let _ = state.index_writer_tx.send(IndexWriterCommand::Insert(
                key_hex.clone(),
                persist_meta.clone(),
            ));
        }
        stats.persist_enqueue_ns = t_persist_enqueue.elapsed().as_nanos() as u64;
        state.artifacts.insert(artifact_key_hex.to_string(), cached);
        let latency_ns = compile_start.elapsed().as_nanos() as u64;
        state.stats.record_miss(latency_ns, artifact_bytes);
        let src = source_path.clone();
        record_session_stat(&state.sessions, sid, move |t| {
            t.record_miss(src, artifact_bytes)
        });
        stats.artifact_insert_stats_ns = t_persist_enqueue.elapsed().as_nanos() as u64;
        return;
    }

    let sem = Arc::clone(&state.persist_semaphore);
    let state_ref = Arc::clone(state_arc);
    let completion_key = artifact_key_hex.to_string();
    // Issue #610, DD-025 condition 1: pending-write registration around
    // the C/C++ cold-miss persist spawn. Concurrent lookups can observe
    // that disk publication is in flight and (optionally) wait briefly
    // for it instead of recompiling-on-race. Completion is signalled on
    // both success and failure paths (failure wakes waiters → re-lookup
    // misses → recompile; the DD-025 failure-mode-is-miss invariant).
    let _pending = pending_writes::register(&state.pending_cache_writes, artifact_key_hex);
    tokio::spawn(async move {
        #[expect(
            clippy::expect_used,
            reason = "persist_semaphore is owned by ServerState for the daemon's lifetime; AcquireError here would be a logic bug (semaphore explicitly closed), not a runtime condition"
        )]
        let _permit = sem
            .acquire()
            .await
            .expect("persist_semaphore is owned by ServerState and never closed");
        let written = tokio::task::spawn_blocking(move || {
            let _guard = guard;
            // Issue #728: `gap_ms` = wall-clock between
            // "linker-success-recorded" (immediately before this spawn was
            // scheduled) and "persist-attempt-started" (now, inside the
            // blocking task). Captured *before* the persist call so the
            // measurement excludes the persist work itself; useful for
            // distinguishing "queue starvation under burst load" from
            // "src vanished" / errno-N failure modes (the rest of the
            // diagnostic — src=, dst=, errno=, src_exists_now=,
            // src_size_now= — is baked into the error by
            // `persist::enrich_persist_err`).
            let gap_ms = t_persist_enqueue.elapsed().as_millis() as u64;
            if let Err(e) = persist_artifact_paths(&artifact_dir, &key_hex, &source_paths) {
                tracing::warn!(
                    key = %key_hex,
                    gap_ms,
                    "failed to persist artifact output: {e}"
                );
            }
            (key_hex, persist_meta)
        })
        .await;
        if let Ok((key_hex, meta)) = written {
            let _ = state_ref
                .index_writer_tx
                .send(IndexWriterCommand::Insert(key_hex, meta));
        }
        // Always complete the pending entry, even on JoinError, so
        // waiters cannot hang past the spawn's lifetime.
        pending_writes::complete(&state_ref.pending_cache_writes, &completion_key);
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
