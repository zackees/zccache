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
    /// Owns the private canonical depfile source until detached persistence
    /// has copied it. `None` for non-staged compiles.
    pub(super) user_depfile_persist_temp: Option<tempfile::TempDir>,
    /// Retains private Rust compiler outputs until detached staged publication
    /// has durably consumed them.
    pub(super) staged_persist_plan: Option<StagedCompilePlan>,
    pub(super) rustc_all_outputs: Option<&'a [RustcOutputFile]>,
    /// Dylint input identity for the rustc verdict layer. `None` selects the
    /// plain-rustc verdict while non-rust compilers never call the rust path.
    pub(super) rustc_dylint_input_hash: Option<&'a str>,
    pub(super) stdout: &'a Arc<Vec<u8>>,
    pub(super) stderr: &'a Arc<Vec<u8>>,
    pub(super) exit_code: i32,
    pub(super) compile_start: Instant,
    pub(super) synchronous_persist: bool,
    pub(super) publication_guard: tokio::sync::OwnedRwLockReadGuard<()>,
    pub(super) resource_admission:
        crate::daemon::server::compile_resource_gate::CompileResourcePermit,
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
    pub(super) rust_snapshot_digest_ns: u64,
    pub(super) rust_snapshot_publication_ns: u64,
    pub(super) rust_snapshot_hardlink_count: u64,
    pub(super) rust_snapshot_copy_count: u64,
    pub(super) rust_snapshot_copy_bytes: u64,
    pub(super) rust_snapshot_error_count: u64,
    pub(super) staged_failure_reason: Option<&'static str>,
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
        user_depfile_persist_temp,
        staged_persist_plan,
        rustc_all_outputs,
        rustc_dylint_input_hash,
        stdout,
        stderr,
        exit_code,
        compile_start,
        synchronous_persist,
        publication_guard,
        resource_admission,
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
                rustc_dylint_input_hash,
                stdout,
                stderr,
                exit_code,
                compile_start,
                &mut stats,
                t_artifact_build,
                synchronous_persist,
                staged_persist_plan,
                publication_guard,
                resource_admission,
            );
        } else {
            store_single_output(
                state_arc,
                sid,
                source_path,
                output_path,
                output_data,
                user_depfile,
                user_depfile_persist_temp,
                &artifact_key_hex,
                stdout,
                stderr,
                exit_code,
                compile_start,
                &mut stats,
                t_artifact_build,
                synchronous_persist,
                publication_guard,
                resource_admission,
            );
        }
    }

    stats.artifact_store_ns = t_store.elapsed().as_nanos() as u64;
    stats
}

const MAX_STAGED_PUBLICATION_ERROR_CHARS: usize = 512;

/// Result of the detached single-output persist operation.
///
/// The background task is deliberately required to hand this back to its
/// async parent: an index row is only valid after the artifact files have
/// been published.  Keeping this as a named, must-use outcome prevents a
/// future logging-only error branch from silently turning a failed persist
/// into a durable dangling index entry.
#[derive(Debug)]
#[must_use = "a persist outcome must gate index publication"]
struct PersistOutcome {
    published: bool,
    meta: Option<ArtifactIndex>,
}

impl PersistOutcome {
    const fn published(meta: ArtifactIndex) -> Self {
        Self {
            published: true,
            meta: Some(meta),
        }
    }

    const fn failed() -> Self {
        Self {
            published: false,
            meta: None,
        }
    }
}

/// The only async single-output index-publication gate.
///
/// Keep the outcome consumption next to the channel send: a failed persist
/// must remain a clean miss rather than becoming an index row whose payload
/// never existed.
fn enqueue_persisted_index(
    outcome: PersistOutcome,
    index_writer_tx: &tokio::sync::mpsc::UnboundedSender<IndexWriterCommand>,
    key_hex: String,
) -> bool {
    if !outcome.published {
        return false;
    }
    let Some(meta) = outcome.meta else {
        return false;
    };
    // #1177: this send only fails when the index writer is gone, which means
    // the daemon has silently stopped recording what it caches. `enqueue`
    // needs the state to bound the report, so the one call site that has only
    // the sender keeps the raw send and its own report.
    if index_writer_tx
        .send(IndexWriterCommand::Insert(key_hex.clone(), meta))
        .is_err()
    {
        tracing::error!(
            artifact_key = %key_hex,
            "index writer is gone; persisted artifact will not survive a daemon restart"
        );
    }
    true
}

/// Record the full persistence failure at the stage where publication is
/// abandoned. The profiler retains the compact reason ID; the lifecycle and
/// session logs retain a bounded OS error so an operator can identify the
/// failed operation and path after the compiler output has been salvaged.
fn report_staged_publication_failure(
    state: &SharedState,
    sid: &SessionId,
    artifact_key_hex: &str,
    reason: StagedPublishFailure,
    error: impl std::fmt::Display,
) {
    let error = truncate_staged_publication_error(&error.to_string());
    let reason_id = reason.id();
    tracing::warn!(
        key = %artifact_key_hex,
        reason = reason_id,
        error = %error,
        "staged artifact publication failed"
    );
    crate::core::lifecycle::write_event(
        crate::core::lifecycle::EVENT_STAGED_PUBLICATION_FAILED,
        serde_json::json!({
            "reason": reason_id,
            "error": error,
            "artifact_key": artifact_key_hex,
        }),
    );
    write_session_log(
        &state.sessions,
        sid,
        &format!(
            "[DIAG] staged_publication_failed: reason={reason_id} key={artifact_key_hex} error={error}"
        ),
    );
}

fn truncate_staged_publication_error(error: &str) -> String {
    let mut chars = error.chars();
    let truncated: String = chars
        .by_ref()
        .take(MAX_STAGED_PUBLICATION_ERROR_CHARS)
        .collect();
    if chars.next().is_some() {
        format!("{truncated}…")
    } else {
        truncated
    }
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
    dylint_input_hash: Option<&str>,
    stdout: &Arc<Vec<u8>>,
    stderr: &Arc<Vec<u8>>,
    exit_code: i32,
    compile_start: Instant,
    stats: &mut MissArtifactStoreStats,
    t_artifact_build: Instant,
    synchronous_persist: bool,
    staged_persist_plan: Option<StagedCompilePlan>,
    publication_guard: tokio::sync::OwnedRwLockReadGuard<()>,
    resource_admission: crate::daemon::server::compile_resource_gate::CompileResourcePermit,
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

    // Rustc outputs already exist in the private compile plan. Publish those
    // owned paths provisionally for immediate in-process hits; the detached
    // publisher later replaces that entry with the durable generation/index.
    let t_artifact_index_build = Instant::now();
    let mut meta = ArtifactIndex::new(
        output_names,
        output_sizes,
        Arc::new(Vec::new()),
        Arc::new(Vec::new()),
        0,
    );
    let verdict_key_hex =
        crate::depgraph::compute_rustc_verdict_key(artifact_key_hex, dylint_input_hash)
            .hash()
            .to_hex();
    meta.rustc_verdicts.insert(
        verdict_key_hex,
        ArtifactVerdict {
            stdout: Arc::clone(stdout),
            stderr: Arc::clone(stderr),
            exit_code,
        },
    );
    stats.artifact_index_build_ns = t_artifact_index_build.elapsed().as_nanos() as u64;
    stats.artifact_build_ns = t_artifact_build.elapsed().as_nanos() as u64;

    if let Some(staged_plan) = staged_persist_plan {
        let _pending = pending_writes::register(&state.pending_cache_writes, artifact_key_hex);
        let staged_plan = Arc::new(staged_plan);
        // The publisher and provisional hit need the same metadata, paths,
        // and private-plan lifetime. These clones deliberately split that
        // ownership before the publisher moves its copies into the task.
        let provisional = CachedArtifact::from_provisional_staged(
            meta.clone(),
            source_paths.clone(),
            Arc::clone(&staged_plan),
        );
        let provisional = match state.artifacts.entry(artifact_key_hex.to_string()) {
            dashmap::mapref::entry::Entry::Vacant(entry) => {
                // Retain one identity clone so a failed older publisher can
                // remove only its own provisional row, never a replacement.
                entry.insert(provisional.clone());
                Some(provisional)
            }
            dashmap::mapref::entry::Entry::Occupied(_) => None,
        };
        let state_ref = Arc::clone(state_arc);
        let state_for_publish = Arc::clone(state_arc);
        let artifact_dir = state.artifact_dir.clone();
        let key_hex = artifact_key_hex.to_string();
        let sid = *sid;
        let completion_key = artifact_key_hex.to_string();
        let t_persist_enqueue = Instant::now();
        tokio::spawn(async move {
            let plan_for_publish = Arc::clone(&staged_plan);
            #[expect(
                clippy::expect_used,
                reason = "persist_semaphore is owned by ServerState for the daemon's lifetime; AcquireError here would be a logic bug"
            )]
            let _permit = state_ref
                .persist_semaphore
                .acquire()
                .await
                .expect("persist_semaphore is owned by ServerState and never closed");
            let published = tokio::task::spawn_blocking(move || {
                let _publication_guard = publication_guard;
                let _resource_admission = resource_admission;
                let _staged_plan = plan_for_publish;
                let snapshot =
                    persist_artifact_paths_with_stats(&artifact_dir, &key_hex, &source_paths)?;
                commit_rustc_artifact_index(
                    state_for_publish.as_ref(),
                    key_hex,
                    meta,
                    snapshot.staged,
                )
                .map_err(|reason| {
                    std::io::Error::other(format!(
                        "staged artifact index commit failed: {}",
                        reason.id()
                    ))
                })?;
                Ok::<_, std::io::Error>(snapshot)
            })
            .await;
            match published {
                Ok(Ok(snapshot)) => {
                    use crate::daemon::staged_stats::{StagedBytes, StagedCounter, StagedTiming};
                    if snapshot.staged {
                        state_ref
                            .profiler
                            .staged
                            .count(StagedCounter::PublicationSuccess);
                        state_ref
                            .profiler
                            .staged
                            .timing(StagedTiming::Hashing, snapshot.staged_hash_ns);
                        state_ref
                            .profiler
                            .staged
                            .timing(StagedTiming::Publication, snapshot.staged_publication_ns);
                        state_ref
                            .profiler
                            .staged
                            .bytes(StagedBytes::Publication, snapshot.copy_bytes);
                    }
                }
                Ok(Err(error)) => {
                    let reason =
                        staged_publish_failure(&error).unwrap_or(StagedPublishFailure::StoreSetup);
                    record_staged_publication_failure(state_ref.as_ref(), reason);
                    report_staged_publication_failure(
                        state_ref.as_ref(),
                        &sid,
                        &completion_key,
                        reason,
                        &error,
                    );
                    // #1244: the on-disk generation and pointer are already
                    // gone. Drop the index row too, or lookups keep resolving
                    // a key whose payload no longer exists.
                    if reason == StagedPublishFailure::Conflict {
                        if let Err(error) = state_ref
                            .index_writer_tx
                            .send(IndexWriterCommand::Remove(vec![completion_key.clone()]))
                        {
                            tracing::warn!(
                                %error,
                                key = %completion_key,
                                "failed to enqueue index removal for evicted conflicting key"
                            );
                        }
                    }
                    if reason == StagedPublishFailure::Conflict {
                        state_ref.artifacts.remove(&completion_key);
                    } else if let Some(provisional) = provisional.as_ref() {
                        remove_provisional_artifact(
                            state_ref.as_ref(),
                            &completion_key,
                            provisional,
                        );
                    }
                }
                Err(error) => {
                    tracing::warn!(%error, key = %completion_key, "staged artifact persistence task failed to join");
                    if let Some(provisional) = provisional.as_ref() {
                        remove_provisional_artifact(
                            state_ref.as_ref(),
                            &completion_key,
                            provisional,
                        );
                    }
                }
            }
            drop(staged_plan);
            pending_writes::complete(&state_ref.pending_cache_writes, &completion_key);
        });
        stats.persist_enqueue_ns = t_persist_enqueue.elapsed().as_nanos() as u64;
        let latency_ns = compile_start.elapsed().as_nanos() as u64;
        state.stats.record_miss(latency_ns, artifact_bytes);
        let src = source_path.clone();
        record_session_stat(&state.sessions, &sid, move |t| {
            t.record_miss(src, artifact_bytes);
        });
        drop(_pending);
        return;
    }

    let t_persist_sync = Instant::now();
    let sync_persist_result =
        persist_artifact_paths_with_stats(&state.artifact_dir, artifact_key_hex, &source_paths);
    stats.rust_snapshot_ns = t_persist_sync.elapsed().as_nanos() as u64;
    let persisted = match sync_persist_result {
        Ok(snapshot_stats) => {
            stats.rust_snapshot_hardlink_count = snapshot_stats.hardlink_count;
            stats.rust_snapshot_copy_count = snapshot_stats.copy_count;
            stats.rust_snapshot_copy_bytes = snapshot_stats.copy_bytes;
            stats.rust_snapshot_digest_ns = snapshot_stats.staged_hash_ns;
            stats.rust_snapshot_publication_ns = snapshot_stats.staged_publication_ns;
            let (index_failure, index_commit_ns) = match commit_rustc_artifact_index(
                state,
                artifact_key_hex.to_string(),
                meta,
                snapshot_stats.staged,
            ) {
                Ok(elapsed_ns) => (None, elapsed_ns),
                Err(reason) => (Some(reason), 0),
            };
            if let Some(reason) = index_failure {
                record_staged_publication_failure(state, reason);
                stats.staged_failure_reason = Some(reason.id());
                stats.rust_snapshot_error_count = stats.rust_snapshot_error_count.saturating_add(1);
                report_staged_publication_failure(
                    state,
                    sid,
                    artifact_key_hex,
                    reason,
                    "staged artifact generation published but index commit failed",
                );
                false
            } else {
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
                        snapshot_stats
                            .staged_publication_ns
                            .saturating_add(index_commit_ns),
                    );
                    state
                        .profiler
                        .staged
                        .bytes(StagedBytes::Publication, snapshot_stats.copy_bytes);
                }
                true
            }
        }
        Err(e) => {
            let failure_reason =
                staged_publish_failure(&e).unwrap_or(StagedPublishFailure::StoreSetup);
            if synchronous_persist {
                record_staged_publication_failure(state, failure_reason);
            }
            stats.staged_failure_reason = Some(failure_reason.id());
            stats.rust_snapshot_error_count = stats.rust_snapshot_error_count.saturating_add(1);
            report_staged_publication_failure(state, sid, artifact_key_hex, failure_reason, &e);
            false
        }
    };

    stats.persist_enqueue_ns = 0;

    let t_artifact_insert_stats = Instant::now();
    if persisted {
        stats.artifact_memory_insert_ns = t_artifact_insert_stats.elapsed().as_nanos() as u64;
    }

    let latency_ns = compile_start.elapsed().as_nanos() as u64;
    state.stats.record_miss(latency_ns, artifact_bytes);
    let src = source_path.clone();
    record_session_stat(&state.sessions, sid, move |t| {
        t.record_miss(src, artifact_bytes);
    });
    stats.artifact_insert_stats_ns = t_artifact_insert_stats.elapsed().as_nanos() as u64;
    drop(publication_guard);
    drop(resource_admission);
}

fn remove_provisional_artifact(state: &SharedState, key_hex: &str, provisional: &CachedArtifact) {
    if let dashmap::mapref::entry::Entry::Occupied(entry) =
        state.artifacts.entry(key_hex.to_owned())
    {
        if entry.get().same_instance(provisional) {
            entry.remove();
        }
    }
}

fn commit_rustc_artifact_index(
    state: &SharedState,
    key_hex: String,
    mut meta: ArtifactIndex,
    staged: bool,
) -> Result<u64, StagedPublishFailure> {
    use dashmap::mapref::entry::Entry;

    if let Some(durable) = super::rustc_index::durable_rustc_index(state, &key_hex) {
        meta = super::rustc_index::merge_rustc_index(meta, durable);
    }

    let commit = |metadata: ArtifactIndex| {
        if staged {
            send_staged_index_insert(state, key_hex.clone(), metadata)
        } else {
            enqueue_index_insert(state, key_hex.clone(), metadata);
            Ok(0)
        }
    };

    match state.artifacts.entry(key_hex.clone()) {
        Entry::Occupied(mut entry) => {
            meta = super::rustc_index::merge_rustc_index(meta, entry.get().meta.clone());
            let commit_ns = commit(meta.clone())?;
            entry.insert(CachedArtifact::from_index(meta));
            Ok(commit_ns)
        }
        Entry::Vacant(entry) => {
            let commit_ns = commit(meta.clone())?;
            entry.insert(CachedArtifact::from_index(meta));
            Ok(commit_ns)
        }
    }
}

/// Preserve canonical staged depfile bytes after requested-output
/// materialization removes the compiler's private staging root.
///
/// The returned directory is an ownership guard: callers must keep it alive
/// through synchronous persistence or move it into the detached persist task.
pub(super) fn preserve_staged_depfile_for_persistence(
    capture: &mut Option<(NormalizedPath, Vec<u8>)>,
    requested_path: Option<&NormalizedPath>,
    temp_root: &Path,
) -> std::io::Result<Option<tempfile::TempDir>> {
    let (Some((source_path, bytes)), Some(requested_path)) = (capture.as_mut(), requested_path)
    else {
        return Ok(None);
    };
    let file_name = requested_path.file_name().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "staged user depfile has no file name",
        )
    })?;
    std::fs::create_dir_all(temp_root)?;
    let temp = tempfile::Builder::new()
        .prefix("staged-depfile-persist-")
        .tempdir_in(temp_root)?;
    let persisted_source = temp.path().join(file_name);
    std::fs::write(&persisted_source, bytes)?;
    *source_path = persisted_source.into();
    Ok(Some(temp))
}

#[allow(clippy::too_many_arguments)]
fn store_single_output(
    state_arc: &Arc<SharedState>,
    sid: &SessionId,
    source_path: &NormalizedPath,
    output_path: &NormalizedPath,
    output_data: Vec<u8>,
    user_depfile: Option<(NormalizedPath, Vec<u8>)>,
    user_depfile_persist_temp: Option<tempfile::TempDir>,
    artifact_key_hex: &str,
    stdout: &Arc<Vec<u8>>,
    stderr: &Arc<Vec<u8>>,
    exit_code: i32,
    compile_start: Instant,
    stats: &mut MissArtifactStoreStats,
    t_artifact_build: Instant,
    synchronous_persist: bool,
    publication_guard: tokio::sync::OwnedRwLockReadGuard<()>,
    resource_admission: crate::daemon::server::compile_resource_gate::CompileResourcePermit,
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
    let persist_payloads: Vec<Arc<Vec<u8>>> = artifact
        .outputs
        .iter()
        .filter_map(|output| output.payload.as_bytes().cloned())
        .collect();
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
        use crate::daemon::staged_stats::{StagedBytes, StagedCounter, StagedTiming};
        let staged_publication =
            staged_artifacts_enabled() && staged_key_supported(&key_hex) && !pack_mode_enabled();
        let written =
            match persist_artifact_paths_with_stats(&artifact_dir, &key_hex, &source_paths) {
                Ok(persisted) => {
                    let (index_failure, index_commit_ns) = if persisted.staged {
                        match send_staged_index_insert(state, key_hex.clone(), persist_meta.clone())
                        {
                            Ok(elapsed_ns) => (None, elapsed_ns),
                            Err(reason) => (Some(reason), 0),
                        }
                    } else {
                        (None, 0)
                    };
                    if let Some(reason) = index_failure {
                        stats.staged_failure_reason = Some(reason.id());
                        stats.rust_snapshot_error_count =
                            stats.rust_snapshot_error_count.saturating_add(1);
                        record_staged_publication_failure(state, reason);
                        false
                    } else {
                        if persisted.staged {
                            state
                                .profiler
                                .staged
                                .count(StagedCounter::PublicationSuccess);
                            state
                                .profiler
                                .staged
                                .timing(StagedTiming::Hashing, persisted.staged_hash_ns);
                            state.profiler.staged.timing(
                                StagedTiming::Publication,
                                persisted
                                    .staged_publication_ns
                                    .saturating_add(index_commit_ns),
                            );
                            state
                                .profiler
                                .staged
                                .bytes(StagedBytes::Publication, persisted.copy_bytes);
                        }
                        true
                    }
                }
                Err(error) => {
                    let failure_reason =
                        staged_publish_failure(&error).unwrap_or(StagedPublishFailure::StoreSetup);
                    stats.rust_snapshot_error_count =
                        stats.rust_snapshot_error_count.saturating_add(1);
                    stats.staged_failure_reason = Some(failure_reason.id());
                    if staged_publication {
                        record_staged_publication_failure(state, failure_reason);
                    }
                    false
                }
            };
        if written && !staged_publication {
            enqueue_index_insert(state, key_hex.clone(), persist_meta.clone());
        }
        stats.persist_enqueue_ns = t_persist_enqueue.elapsed().as_nanos() as u64;
        if written {
            state.artifacts.insert(artifact_key_hex.to_string(), cached);
        }
        let latency_ns = compile_start.elapsed().as_nanos() as u64;
        state.stats.record_miss(latency_ns, artifact_bytes);
        let src = source_path.clone();
        record_session_stat(&state.sessions, sid, move |t| {
            t.record_miss(src, artifact_bytes)
        });
        stats.artifact_insert_stats_ns = t_persist_enqueue.elapsed().as_nanos() as u64;
        drop(publication_guard);
        drop(resource_admission);
        return;
    }

    // Issue #610, DD-025 condition 1: pending-write registration around
    // the C/C++ cold-miss persist spawn. Concurrent lookups can observe
    // that disk publication is in flight and (optionally) wait briefly
    // for it instead of recompiling-on-race. Completion is signalled on
    // both success and failure paths (failure wakes waiters → re-lookup
    // misses → recompile; the DD-025 failure-mode-is-miss invariant).
    // Publish the in-process payload before handing persistence to the
    // detached task. A failed persist removes this provisional entry, while
    // the typed outcome below prevents the durable index publication.
    state.artifacts.insert(artifact_key_hex.to_string(), cached);
    let _pending = pending_writes::register(&state.pending_cache_writes, artifact_key_hex);
    let sem = Arc::clone(&state.persist_semaphore);
    let state_ref = Arc::clone(state_arc);
    let index_writer_tx = state.index_writer_tx.clone();
    let lifecycle_cache_root = state.cache_dir.clone();
    let completion_key = artifact_key_hex.to_string();
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
            let _publication_guard = publication_guard;
            let _resource_admission = resource_admission;
            let _user_depfile_persist_temp = user_depfile_persist_temp;
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
            match persist_artifact_payloads(&artifact_dir, &key_hex, &persist_payloads) {
                Ok(()) => PersistOutcome::published(persist_meta),
                Err(e) => {
                    tracing::warn!(
                        key = %key_hex,
                        paths = ?source_paths,
                        errno = ?e.raw_os_error(),
                        gap_ms,
                        "failed to persist artifact output: {e}"
                    );
                    crate::core::lifecycle::write_event_in_cache_root(
                        lifecycle_cache_root.as_path(),
                        crate::core::lifecycle::EVENT_PERSIST_FAILED,
                        serde_json::json!({
                            "artifact_key": key_hex,
                            "paths": source_paths.iter().map(|path| path.as_path().display().to_string()).collect::<Vec<_>>(),
                            "errno": e.raw_os_error(),
                            "error": e.to_string(),
                            "gap_ms": gap_ms,
                        }),
                    );
                    PersistOutcome::failed()
                }
            }
        })
        .await;
        match written {
            Ok(outcome) => {
                if !enqueue_persisted_index(outcome, &index_writer_tx, completion_key.clone()) {
                    // This entry was only provisional while the filesystem
                    // publish ran. Do not retain a lookup row for a failed
                    // persist, or a later hit could point at missing payloads.
                    state_ref.artifacts.remove(&completion_key);
                }
            }
            Err(error) => {
                tracing::warn!(%error, "artifact persistence task failed to join");
                state_ref.artifacts.remove(&completion_key);
            }
        }
        // Always complete the pending entry, even on JoinError, so
        // waiters cannot hang past the spawn's lifetime.
        pending_writes::complete(&state_ref.pending_cache_writes, &completion_key);
    });
    stats.persist_enqueue_ns = t_persist_enqueue.elapsed().as_nanos() as u64;

    let t_artifact_insert_stats = Instant::now();
    let latency_ns = compile_start.elapsed().as_nanos() as u64;
    state.stats.record_miss(latency_ns, artifact_bytes);
    let src = source_path.clone();
    record_session_stat(&state.sessions, sid, move |t| {
        t.record_miss(src, artifact_bytes);
    });
    stats.artifact_insert_stats_ns = t_artifact_insert_stats.elapsed().as_nanos() as u64;
}

#[cfg(test)]
#[path = "miss_store_tests.rs"]
mod staged_publication_diagnostic_tests;
