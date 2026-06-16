//! Source + header hashing and depgraph verdict.
//!
//! Runs after the fast-hit miss to decide whether the cached artifact is
//! still valid. Returns the verdict plus per-phase timings consumed by the
//! miss-profile emitters.

use super::super::super::*;

pub(super) struct HashVerifyOutcome {
    pub(super) hash_map: HashMap<NormalizedPath, ContentHash>,
    pub(super) hash_source_ns: u64,
    pub(super) hash_headers_ns: u64,
    pub(super) depgraph_check_ns: u64,
    pub(super) verdict: crate::depgraph::CacheVerdict,
    pub(super) diag_reason: String,
}

pub(super) enum HashSourceOutcome {
    Ready(HashVerifyOutcome),
    /// Source hash failed; caller should fall back to a direct compile.
    Fallback,
}

pub(super) struct HashVerifyInput<'a> {
    pub(super) state: &'a SharedState,
    pub(super) sid: &'a SessionId,
    pub(super) context_key: ContextKey,
    pub(super) source_path: &'a NormalizedPath,
    pub(super) ctx: &'a CompileContext,
    pub(super) rustc_extern_paths: &'a [NormalizedPath],
    pub(super) snap_clock: Clock,
}

/// Hash the source file (skipped on cold context), then hash the depgraph's
/// stored include set in parallel and consult the depgraph for a verdict.
///
/// Returns `HashSourceOutcome::Fallback` only when the *source* hash itself
/// errored — header hash failures are logged and the depgraph check still
/// runs (mirroring the pre-split behaviour).
pub(super) fn hash_and_verify(input: HashVerifyInput<'_>) -> HashSourceOutcome {
    let HashVerifyInput {
        state,
        sid,
        context_key,
        source_path,
        ctx,
        rustc_extern_paths,
        snap_clock,
    } = input;

    // Skip pre-compile hashing for cold contexts — the depgraph would
    // return Cold without examining any hashes, so the work is wasted.
    // Jump straight to compiler exec.
    let context_is_cold = state.dep_graph.load().is_cold(&context_key);

    // ── Phase: hash source ───────────────────────────────────────────
    // Issue #468: env-gated sub-phase trace. When ZCCACHE_HIT_TRACE=1, the
    // daemon dumps per-compile sub-phase counts to stderr so the perf
    // harness can break down the dominant "metadata cache (source+hdrs)"
    // phase into source vs headers vs metadata-hit-rate components.
    let hit_trace = std::env::var_os("ZCCACHE_HIT_TRACE").is_some();
    let t2 = std::time::Instant::now();
    let mut hash_map: HashMap<NormalizedPath, ContentHash> = HashMap::new();
    if !context_is_cold {
        match hash_file(&state.cache_system, source_path, snap_clock) {
            Ok(h) => {
                hash_map.insert(source_path.clone(), h);
            }
            Err(e) => {
                write_session_log(
                    &state.sessions,
                    sid,
                    &format!("cache key error: {e}, falling back to direct compile"),
                );
                return HashSourceOutcome::Fallback;
            }
        }
    }
    let hash_source_ns = t2.elapsed().as_nanos() as u64;

    // ── Phase: hash headers + depgraph check ────────────────────────
    let t3 = std::time::Instant::now();
    let hash_headers_ns;
    let depgraph_check_ns;
    let verdict;
    let diag_reason;

    if context_is_cold {
        // Cold context — skip hashing and depgraph check entirely.
        hash_headers_ns = 0;
        depgraph_check_ns = 0;
        verdict = crate::depgraph::CacheVerdict::Cold;
        diag_reason = "cold_skip".to_string();
    } else {
        // Hash includes + force-includes in parallel (PCH-aware).
        let headers_count: usize;
        {
            use rayon::prelude::*;
            let includes = state.dep_graph.load().get_includes(&context_key);
            let include_iter = includes
                .iter()
                .flat_map(|v| v.iter().map(|h| (h, "header_hash_fail")));
            let force_iter = ctx
                .force_includes
                .iter()
                .map(|h| (h, "force_include_hash_fail"));
            let extern_iter = rustc_extern_paths
                .iter()
                .map(|h| (h, "rustc_extern_hash_fail"));
            let all_paths: Vec<_> = include_iter.chain(force_iter).chain(extern_iter).collect();
            headers_count = all_paths.len();

            let results: Vec<_> = all_paths
                .par_iter()
                .map(|(header, label)| {
                    let hash_path = resolve_pch_source(header, &state.pch_source_map)
                        .unwrap_or_else(|| (*header).clone());
                    let result = hash_file(&state.cache_system, &hash_path, snap_clock);
                    ((*header).clone(), hash_path, result, *label)
                })
                .collect();

            for (header, hash_path, result, label) in results {
                match result {
                    Ok(h) => {
                        hash_map.insert(header, h);
                    }
                    Err(e) => {
                        write_session_log(
                            &state.sessions,
                            sid,
                            &format!("[DIAG] {label}: {} error={e}", hash_path.display()),
                        );
                    }
                }
            }
        }
        hash_headers_ns = t3.elapsed().as_nanos() as u64;

        // Issue #468: ZCCACHE_HIT_TRACE=1 dumps per-compile sub-phase breakdown
        // so the perf harness can decompose the dominant metadata-cache phase.
        // Format is a single line per compile, easy to grep/awk over a session.
        if hit_trace {
            let hdr_avg_us = if headers_count > 0 {
                hash_headers_ns / headers_count as u64 / 1_000
            } else {
                0
            };
            eprintln!(
                "ZCCACHE_HIT_TRACE source_us={} headers_count={} headers_us={} hdr_avg_us={} \
                 source_path={}",
                hash_source_ns / 1_000,
                headers_count,
                hash_headers_ns / 1_000,
                hdr_avg_us,
                source_path.display()
            );
        }

        // ── Phase: depgraph check ────────────────────────────────────
        // Fast path: recompute artifact key from fresh hashes and compare
        // with the stored key.  Skips redundant journal freshness checks
        // and path clones that check_diagnostic performs.
        if let Some(artifact_key) = state.dep_graph.load().try_fast_hit(&context_key, |p| {
            let path = NormalizedPath::new(p);
            hash_map.get(&path).copied()
        }) {
            depgraph_check_ns = 0;
            verdict = crate::depgraph::CacheVerdict::Hit { artifact_key };
            diag_reason = "fast_key_match".to_string();
        } else {
            let t4 = std::time::Instant::now();
            let result = {
                let is_fresh = |p: &Path| {
                    let path = NormalizedPath::new(p);
                    !state
                        .cache_system
                        .journal()
                        .changed_since(&path, snap_clock)
                };
                let get_hash = |p: &Path| {
                    let path = NormalizedPath::new(p);
                    hash_map.get(&path).copied()
                };
                state
                    .dep_graph
                    .load()
                    .check_diagnostic(&context_key, is_fresh, get_hash)
            };
            depgraph_check_ns = t4.elapsed().as_nanos() as u64;
            verdict = result.0;
            diag_reason = result.1;
        }
    }

    HashSourceOutcome::Ready(HashVerifyOutcome {
        hash_map,
        hash_source_ns,
        hash_headers_ns,
        depgraph_check_ns,
        verdict,
        diag_reason,
    })
}
