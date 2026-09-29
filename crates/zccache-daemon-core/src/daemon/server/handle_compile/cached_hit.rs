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
    /// Rustc diagnostics/exit-status entry paired with the output artifact.
    /// `None` keeps the legacy single-layer path for non-rust compilers.
    pub(super) verdict_key_hex: Option<&'a str>,
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
    /// Physical OUT_DIR of the current rustc request; used only to rebase
    /// dep-info after a cross-worktree hit, never to choose artifact bytes.
    pub(super) current_rustc_out_dir: Option<CertifiedOutDir>,
    /// The requesting compile's key root, which replaces the logical root
    /// in a delivered C/C++ user depfile. `None` for rustc.
    pub(super) depfile_key_root: Option<NormalizedPath>,
    pub(super) compile_start: Instant,
    pub(super) hit_label: &'static str,
    pub(super) cached_error_label: &'static str,
    pub(super) record_compilation: bool,
    pub(super) downgrade_output_metadata: bool,
    pub(super) mtime_floor_paths: Vec<NormalizedPath>,
    pub(super) rustc_metadata_compat_outputs: Option<Vec<NormalizedPath>>,
    /// `Some` identifies a parsed rustc invocation and records whether its
    /// crate type authorizes `.rlib` delivery. `.rmeta` remains eligible for
    /// any parsed rustc invocation; `None` keeps staged outputs independent.
    pub(super) rustc_archive_hardlink_eligible: Option<bool>,
    /// This request's resolved `ZCCACHE_MODE` (#1683).
    pub(super) materialization_mode: MaterializationMode,
    pub(super) phases: CachedHitPhases,
}

pub(super) struct OwnedCachedHitMaterializeRequest {
    pub(super) state: Arc<SharedState>,
    pub(super) sid: SessionId,
    pub(super) artifact_key_hex: String,
    pub(super) verdict_key_hex: Option<String>,
    pub(super) source_path: NormalizedPath,
    pub(super) output_path: NormalizedPath,
    pub(super) secondary_output_dir: NormalizedPath,
    pub(super) current_depfile_dest: Option<NormalizedPath>,
    pub(super) current_rustc_out_dir: Option<CertifiedOutDir>,
    pub(super) depfile_key_root: Option<NormalizedPath>,
    pub(super) compile_start: Instant,
    pub(super) hit_label: &'static str,
    pub(super) cached_error_label: &'static str,
    pub(super) record_compilation: bool,
    pub(super) downgrade_output_metadata: bool,
    pub(super) mtime_floor_paths: Vec<NormalizedPath>,
    pub(super) rustc_metadata_compat_outputs: Option<Vec<NormalizedPath>>,
    pub(super) rustc_archive_hardlink_eligible: Option<bool>,
    /// This request's resolved `ZCCACHE_MODE` (#1683).
    pub(super) materialization_mode: MaterializationMode,
    pub(super) phases: CachedHitPhases,
}

pub(super) async fn materialize_cached_compile_hit_offloaded(
    request: OwnedCachedHitMaterializeRequest,
) -> Result<Response, CachedHitFailure> {
    let launcher = Arc::clone(&request.state);
    match launcher
        .launch_blocking(move || {
            materialize_cached_compile_hit(CachedHitMaterializeRequest {
                state: &request.state,
                sid: &request.sid,
                artifact_key_hex: &request.artifact_key_hex,
                verdict_key_hex: request.verdict_key_hex.as_deref(),
                source_path: &request.source_path,
                output_path: &request.output_path,
                secondary_output_dir: request.secondary_output_dir,
                current_depfile_dest: request.current_depfile_dest,
                current_rustc_out_dir: request.current_rustc_out_dir,
                depfile_key_root: request.depfile_key_root,
                compile_start: request.compile_start,
                hit_label: request.hit_label,
                cached_error_label: request.cached_error_label,
                record_compilation: request.record_compilation,
                downgrade_output_metadata: request.downgrade_output_metadata,
                mtime_floor_paths: request.mtime_floor_paths,
                rustc_metadata_compat_outputs: request.rustc_metadata_compat_outputs,
                rustc_archive_hardlink_eligible: request.rustc_archive_hardlink_eligible,
                materialization_mode: request.materialization_mode,
                phases: request.phases,
            })
        })
        .await
    {
        Ok(result) => result,
        Err(error) => {
            tracing::error!(%error, "cache-hit materialization task failed");
            Err(CachedHitFailure::CacheRead)
        }
    }
}

#[derive(Debug)]
pub(super) enum CachedHitFailure {
    /// The shared rustc output exists, but this plain/Dylint identity has not
    /// produced diagnostics and an exit status yet. This is a soft miss and
    /// is not evidence that the artifact payload disappeared.
    VerdictMissing,
    CacheBlobMissing(CacheBlobMissing),
    CacheRead,
    DestinationWrite,
}

impl From<MaterializationFailure> for CachedHitFailure {
    fn from(failure: MaterializationFailure) -> Self {
        match failure {
            MaterializationFailure::CacheBlobMissing(missing) => Self::CacheBlobMissing(missing),
            MaterializationFailure::CacheRead(_) => Self::CacheRead,
            MaterializationFailure::DestinationWrite(_) => Self::DestinationWrite,
        }
    }
}

pub(super) fn materialize_cached_compile_hit(
    request: CachedHitMaterializeRequest<'_>,
) -> Result<Response, CachedHitFailure> {
    let CachedHitMaterializeRequest {
        state,
        sid,
        artifact_key_hex,
        verdict_key_hex,
        source_path,
        output_path,
        secondary_output_dir,
        current_depfile_dest,
        current_rustc_out_dir,
        depfile_key_root,
        compile_start,
        hit_label,
        cached_error_label,
        record_compilation,
        downgrade_output_metadata,
        mtime_floor_paths,
        rustc_metadata_compat_outputs,
        rustc_archive_hardlink_eligible,
        materialization_mode,
        phases,
    } = request;

    // Issue #460: collapse clock reads on the warm-hit path. Previously this
    // function did 9 `Instant::now()` reads per hit (4 explicit + 5 implicit
    // via `.elapsed()`); each costs ~50ns on Linux and ~500ns on Windows
    // (QueryPerformanceCounter). The phase split below now uses 4 clock
    // reads (`t0..t3`) and derives every `_ns` value by arithmetic — plus
    // reuses `t0` for the maintenance-visible `last_used` write without an
    // additional clock read (issue #1148).
    let t0 = Instant::now();
    let missing_entry = |key: &str| {
        let failure = cache_blob_missing(
            &state.artifact_dir.join(key),
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "cached artifact metadata or payload is unavailable",
            ),
        );
        report_materialization_failure(&state.cache_dir, key, "compile-hit", &failure);
        failure.into()
    };
    let cached = lookup_artifact_with_disk_fallback(state, artifact_key_hex)
        .ok_or_else(|| missing_entry(artifact_key_hex))?;
    let verdict = verdict_key_hex
        .map(|key| {
            cached
                .meta
                .rustc_verdicts
                .get(key)
                .cloned()
                .ok_or(CachedHitFailure::VerdictMissing)
        })
        .transpose()?;
    let (exit_code, mut stdout, mut stderr) = verdict.as_ref().map_or_else(
        || {
            (
                cached.meta.exit_code,
                cached.stdout.clone(),
                cached.stderr.clone(),
            )
        },
        |verdict| {
            (
                verdict.exit_code,
                verdict.stdout.clone(),
                verdict.stderr.clone(),
            )
        },
    );
    if exit_code != 0 {
        if let Some(requested_outputs) = rustc_metadata_compat_outputs.as_deref() {
            stdout = Arc::new(rehydrate_staged_output_bytes(
                stdout.as_slice(),
                requested_outputs,
            ));
            stderr = Arc::new(rehydrate_staged_output_bytes(
                stderr.as_slice(),
                requested_outputs,
            ));
        }
        // A verdict is diagnostics and status, not an authorization to write
        // shared output bytes. In particular, a Dylint error may share an
        // artifact key with a successful plain-rustc compile whose outputs
        // must remain absent from this failed invocation.
        record_artifact_access(state, artifact_key_hex, &cached, t0);
        let t1 = Instant::now();
        let artifact_lookup_ns = (t1 - t0).as_nanos() as u64;
        crate::daemon::server::inner_trace::record_ns("cache_load", artifact_lookup_ns);
        drop(cached);

        if record_compilation {
            state.stats.record_compilation();
        }
        state.stats.record_cached_error();
        record_session_stat(&state.sessions, sid, |t| {
            t.record_cached_error();
        });
        write_session_log(
            &state.sessions,
            sid,
            &format!(
                "[{}] {} -> {}",
                cached_error_label,
                source_path.display(),
                output_path.display()
            ),
        );
        return Ok(Response::CompileResult {
            exit_code,
            stdout,
            stderr,
            cached: true,
        });
    }
    let payloads = ensure_payloads_for_materialization_for_state(state, &cached, artifact_key_hex)
        .map_err(|error| {
            report_materialization_failure(
                &state.cache_dir,
                artifact_key_hex,
                "compile-hit",
                &error,
            );
            CachedHitFailure::from(error)
        })?;
    record_artifact_access(state, artifact_key_hex, &cached, t0);
    let t1 = Instant::now();
    let artifact_lookup_ns = (t1 - t0).as_nanos() as u64;

    // zccache#940: cache-hit "cache_load" sub-phase — the artifact index
    // lookup + payload read that materializes a cached hit. No-op unless this
    // compile runs inside an embedded `inner_trace::scope` with the trace env
    // set.
    crate::daemon::server::inner_trace::record_ns("cache_load", artifact_lookup_ns);

    let names = Arc::clone(&cached.meta.output_names);
    let artifact_bytes = cached.meta.total_size;
    drop(cached);

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
    let provisional_staged = payloads.is_provisional_staged();
    let (targets, payloads_to_write): (Vec<NormalizedPath>, Vec<CachedPayload>) =
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
                    return Err(missing_entry(artifact_key_hex));
                };
                // The cached name identifies the matching payload, while a
                // metadata-compatible request owns its current Cargo output
                // destination.  That is safe when the requested destination
                // has the payload's extension (the Dylint `.rmeta` case), but
                // Rustc's Wasm plan declares an extensionless primary path
                // and physically emits `<crate>.wasm`.  Preserve that observed
                // suffix beneath the current request's directory instead of
                // turning a Wasm artifact back into an extensionless file.
                targets.push(rustc_compat_materialization_target(&requested, &names[i]));
                selected_payloads.push(payloads[i].clone());
            }
            (targets, selected_payloads)
        } else {
            let targets = (0..payloads.len())
                .map(|i| {
                    let out: NormalizedPath = if i == 0 {
                        // Rustc may have materialized a physical name
                        // different from its declared primary path (for
                        // example by appending a suffix). The cold staged
                        // plan records that observed filename in `names`; use
                        // it on a fresh-root hit rather than replaying the
                        // extensionless declaration (#1522). A C/C++ compiler
                        // writes exactly its `-o` path, and the cache key
                        // excludes that path, so `names[0]` is only the cold
                        // miss's name: always honour the current request
                        // (#1648). `Some` here identifies a rustc request.
                        let is_rustc = rustc_archive_hardlink_eligible.is_some();
                        if !is_rustc
                            || output_path
                                .file_name()
                                .is_some_and(|name| name == std::ffi::OsStr::new(names[i].as_str()))
                        {
                            output_path.clone()
                        } else {
                            secondary_output_dir.join(&names[i])
                        }
                    } else if i == 1 && payloads.len() == 2 {
                        current_depfile_dest
                            .clone()
                            .unwrap_or_else(|| secondary_output_dir.join(&names[i]))
                    } else {
                        secondary_output_dir.join(&names[i])
                    };
                    out
                })
                .collect();
            (targets, payloads.iter().cloned().collect())
        };
    let delivery_policies = targets
        .iter()
        .map(|target| {
            rustc_archive_hardlink_eligible.map_or_else(
                // C/C++ outputs follow their output classification; only
                // `ZCCACHE_MODE=LINK` promotes objects and PCH (#1764).
                || native_output_delivery(materialization_mode, target.as_path()),
                |archive_eligible| {
                    crate::compiler::rustc_output_delivery(archive_eligible, target.as_path())
                },
            )
        })
        .collect::<Vec<_>>();
    let has_staged_payload = provisional_staged
        || payloads_to_write.iter().any(
            |payload| matches!(payload, CachedPayload::File(path) if is_staged_artifact_path(path)),
        );
    // The batch floor is seeded with now(), which already puts a C/C++
    // object at least as new as every source and header, as a bare compiler
    // would; statting each recorded input for a larger mtime only matters for
    // future-dated files and cost ~2.4 us per header on every warm hit (165 us
    // of a ~0.4 ms hit with 70 headers). Rustc keeps its inputs: cargo treats
    // an extern newer than the output as stale (#599), and the set is small.
    // `Some` here identifies a rustc request.
    let floor_paths: &[NormalizedPath] = if rustc_archive_hardlink_eligible.is_some() {
        &mtime_floor_paths
    } else {
        &[]
    };
    payloads.record_staged_pre_materialization(&state.profiler.staged);
    let observed_result = if provisional_staged {
        write_provisional_payloads_par_with_mtime_floor_observed(
            &targets,
            &payloads_to_write,
            floor_paths,
            &delivery_policies,
            materialization_mode,
        )
    } else {
        write_payloads_par_with_mtime_floor_and_policies_observed(
            &targets,
            &payloads_to_write,
            floor_paths,
            &delivery_policies,
            materialization_mode,
        )
    };
    payloads.record_staged_lock_timings(&state.profiler.staged);
    drop(payloads);
    let observed = match observed_result {
        Ok(observed) => observed,
        Err(error) => {
            report_materialization_failure(
                &state.cache_dir,
                artifact_key_hex,
                "compile-hit",
                &error,
            );
            if has_staged_payload {
                use crate::daemon::staged_stats::{StagedCounter, StagedFailure, StagedTiming};
                let elapsed_ns = t1.elapsed().as_nanos() as u64;
                state
                    .profiler
                    .staged
                    .count(StagedCounter::MaterializeFailure);
                state
                    .profiler
                    .staged
                    .failure(StagedFailure::RequestedMaterialization);
                state
                    .profiler
                    .staged
                    .timing(StagedTiming::HitMaterialization, elapsed_ns);
                crate::core::lifecycle::write_event(
                    crate::core::lifecycle::EVENT_STAGED_MATERIALIZATION_FAILED,
                    serde_json::json!({
                        "reason": "requested_materialization",
                        "output_count": targets.len(),
                        "copied_bytes": 0,
                        "elapsed_ns": elapsed_ns,
                    }),
                );
            }
            return Err(error.into());
        }
    };
    let depfile_targets = targets
        .iter()
        .filter(|target| current_depfile_dest.as_ref() == Some(*target))
        .collect::<Vec<_>>();
    let rehydrate_stdout = contains_staged_output_marker(stdout.as_slice());
    let rehydrate_stderr = contains_staged_output_marker(stderr.as_slice());
    if !depfile_targets.is_empty() || rehydrate_stdout || rehydrate_stderr {
        for target in depfile_targets {
            if let Err(error) =
                rehydrate_delivered_depfile(target, &targets, depfile_key_root.as_ref())
            {
                write_session_log(
                    &state.sessions,
                    sid,
                    &format!(
                        "[DIAG] cached_depfile_rehydrate_failed: {}: {error}",
                        target.display()
                    ),
                );
                // The destination depfile could not be rewritten with real
                // output paths; treat it like any destination write failure
                // (soft miss — the shared artifact stays valid).
                return Err(CachedHitFailure::DestinationWrite);
            }
            if let Some(current_out_dir) = current_rustc_out_dir.as_ref() {
                if let Err(error) = rehydrate_rustc_out_dir_depfile(
                    target.as_path(),
                    Path::new(&current_out_dir.path),
                    current_out_dir.generated_name,
                ) {
                    write_session_log(
                        &state.sessions,
                        sid,
                        &format!(
                            "[DIAG] rustc_out_dir_depinfo_rehydrate_failed: {}: {error}",
                            target.display()
                        ),
                    );
                    return Err(CachedHitFailure::DestinationWrite);
                }
            }
        }
        if rehydrate_stdout {
            stdout = Arc::new(rehydrate_staged_output_bytes(stdout.as_slice(), &targets));
        }
        if rehydrate_stderr {
            stderr = Arc::new(rehydrate_staged_output_bytes(stderr.as_slice(), &targets));
        }
    }
    let t2 = Instant::now();
    let write_output_ns = (t2 - t1).as_nanos() as u64;
    if has_staged_payload {
        use crate::daemon::staged_stats::{StagedBytes, StagedCounter, StagedTiming};
        state
            .profiler
            .staged
            .add_count(StagedCounter::MaterializeReflink, observed.reflink_count);
        state
            .profiler
            .staged
            .add_count(StagedCounter::MaterializeHardlink, observed.hardlink_count);
        state
            .profiler
            .staged
            .add_count(StagedCounter::MaterializeCopy, observed.copy_count);
        state
            .profiler
            .staged
            .bytes(StagedBytes::Materialization, observed.copy_bytes);
        state
            .profiler
            .staged
            .timing(StagedTiming::HitMaterialization, write_output_ns);
    }

    if downgrade_output_metadata {
        state.cache_system.metadata().downgrade(output_path);
    }

    if record_compilation {
        state.stats.record_compilation();
    }
    // `latency_ns` is the cache-hit response latency excluding bookkeeping
    // (record_hit / record_session_stat / write_session_log). Same boundary
    // as before — derived from `t2` instead of a fresh clock read.
    let latency_ns = (t2 - compile_start).as_nanos() as u64;
    state.stats.record_hit(latency_ns, artifact_bytes);
    let src = source_path.clone();
    record_session_stat(&state.sessions, sid, move |t| {
        t.record_hit(src, latency_ns, artifact_bytes);
    });
    write_session_log(
        &state.sessions,
        sid,
        &format!(
            "[{}] {} -> {}",
            hit_label,
            source_path.display(),
            output_path.display()
        ),
    );
    let t3 = Instant::now();
    let bookkeeping_ns = (t3 - t2).as_nanos() as u64;

    let total_ns = (t3 - compile_start).as_nanos() as u64;
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

    Ok(Response::CompileResult {
        exit_code,
        stdout,
        stderr,
        cached: true,
    })
}

pub(super) fn rustc_compat_payload_index_for(
    names: &[String],
    requested: &NormalizedPath,
) -> Option<usize> {
    let requested_name = requested.file_name()?.to_str()?;
    if let Some(index) = names.iter().position(|name| name == requested_name) {
        return Some(index);
    }
    let prefixed_observed = format!("{requested_name}.");
    let mut observed = names.iter().enumerate().filter(|(_, name)| {
        name.starts_with(&prefixed_observed)
            && rustc_output_kind(std::path::Path::new(name)).is_none()
    });
    if let (Some((index, _)), None) = (observed.next(), observed.next()) {
        return Some(index);
    }
    let wanted = rustc_output_kind(requested)?;
    names
        .iter()
        .position(|name| rustc_output_kind(std::path::Path::new(name)) == Some(wanted))
}

fn rustc_compat_materialization_target(
    requested: &NormalizedPath,
    observed_name: &str,
) -> NormalizedPath {
    let observed = std::path::Path::new(observed_name);
    if requested.extension() == observed.extension() {
        return requested.clone();
    }
    requested.parent().map_or_else(
        || requested.clone(),
        |parent| parent.join(observed_name).into(),
    )
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
#[path = "cached_hit_output_path_tests.rs"]
mod output_path_tests;

#[cfg(test)]
#[path = "cached_hit_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "cached_hit_link_tests.rs"]
mod link_tests;
