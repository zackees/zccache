//! `Request::GenericToolExec` handler — generic tool caching (issue #272).
//!
//! Lets arbitrary tools that don't speak a compiler-style CLI plug into the
//! daemon's artifact cache. Inputs are declared explicitly by the caller
//! (`input_files`, `input_env`, `input_extra`, optional `tool_hash`,
//! optional `include_scan_files` for Path A, optional `depfile` for Path B).
//! On a cache hit the tool process is NOT spawned; cached stdout, stderr,
//! exit code, and declared `output_files` are replayed. On miss the tool
//! runs and the result is stored under a deterministic cache key.
//!
//! Cache-key composition (domain tag `zccache-exec-key-v2`):
//!   - tool identity (caller-supplied hash, or daemon-hashed binary)
//!   - args in argv order, after `key_args_filter` regex drops
//!   - sorted (name=value) env subset
//!   - cwd (when `cwd_in_key`)
//!   - sorted (path, content-hash) input file pairs
//!   - sorted (path, content-hash) Path A include-scan transitive headers
//!   - sorted (path, content-hash) Path B stored depfile dep set, if any
//!   - declared output_file names (so changing the capture set invalidates)
//!   - input_extra bytes
//!
//! Concurrent callers with the same full cache key coalesce on
//! `state.in_flight_exec` — the first inserter spawns the tool; the rest
//! wait on the shared `Notify` and re-attempt the cache lookup once it
//! fires.

use super::*;
use crate::depgraph::scanner::scan_recursive;
use crate::depgraph::search_paths::IncludeSearchPaths;
use crate::protocol::{ExecCachePolicy, ExecOutputStreams};
use dashmap::mapref::entry::Entry;

/// Domain separation tag for generic-exec cache keys. v2 covers Path A +
/// Path B + filtered args; v1 callers (PROTOCOL_VERSION 10) are no longer
/// wire-compatible since the protocol version itself shifted to 11.
const EXEC_KEY_DOMAIN: &[u8] = b"zccache-exec-key-v2";

/// Cap on per-stream captured bytes. Exceeding this skips caching for the
/// run and emits a diagnostic to stderr; the tool's output still flows
/// through unchanged. The cap matches the IPC frame budget (`MAX_MESSAGE_SIZE`
/// in `protocol::mod`).
const EXEC_STREAM_CAP_BYTES: usize = 16 * 1024 * 1024;

#[allow(clippy::too_many_arguments)]
pub(super) async fn handle_generic_tool_exec(
    state: &Arc<SharedState>,
    tool: &Path,
    args: &[String],
    cwd: &Path,
    env: Vec<(String, String)>,
    input_files: &[NormalizedPath],
    input_extra: Arc<Vec<u8>>,
    output_streams: ExecOutputStreams,
    output_files: &[NormalizedPath],
    tool_hash_override: Option<[u8; 32]>,
    cache_policy: ExecCachePolicy,
    cwd_in_key: bool,
    include_scan_files: &[NormalizedPath],
    include_dirs: &[NormalizedPath],
    system_include_dirs: &[NormalizedPath],
    iquote_dirs: &[NormalizedPath],
    depfile: Option<&Path>,
    non_deterministic: bool,
    key_args_filter: &[String],
) -> Response {
    // 1. Filtered args for the cache key (the tool always sees the raw args).
    let key_args = match apply_key_args_filter(args, key_args_filter) {
        Ok(v) => v,
        Err(e) => {
            return Response::Error {
                message: format!("invalid key-args-filter regex: {e}"),
            };
        }
    };

    // 2. Tool identity hash — caller override wins, otherwise blake3 the
    //    binary (cached by (path, mtime, size) via the compiler-hash cache).
    let tool_id_hash = match tool_hash_override {
        Some(bytes) => ContentHash::from_bytes(bytes),
        None => match hash_file_via_cache(state, tool) {
            Some(h) => h,
            None => {
                return Response::Error {
                    message: format!("cannot hash tool {}", tool.display()),
                };
            }
        },
    };

    // 3. Hash each declared input file via the metadata cache (TwoLayer
    //    mtime+size → blake3 fast path). Paths get absolutized against cwd
    //    so a relative declaration is interpreted the same way it would be
    //    from the tool's perspective.
    let mut input_pairs: Vec<(String, ContentHash)> = Vec::with_capacity(input_files.len());
    for input in input_files {
        let abs: PathBuf = absolutize(input.as_path(), cwd);
        let hash = match hash_file_via_cache(state, &abs) {
            Some(h) => h,
            None => {
                return Response::Error {
                    message: format!("cannot hash input file {}", abs.display()),
                };
            }
        };
        input_pairs.push((normalize_for_key(&abs), hash));
    }
    input_pairs.sort_by(|a, b| a.0.cmp(&b.0));

    // 4. Path A: scan declared seed files for transitive includes, hash each
    //    resolved header. Skipped when no seed files are given.
    let scan_pairs = match run_include_scan(
        state,
        cwd,
        include_scan_files,
        include_dirs,
        system_include_dirs,
        iquote_dirs,
    ) {
        Ok(v) => v,
        Err(e) => return Response::Error { message: e },
    };

    // 5. Compose the primary cache key — everything we know BEFORE we run
    //    the tool. Path B's depfile-derived dep set is mixed in later
    //    (after the tool runs) or — on a warm invocation — read from the
    //    `<primary>.deps` sidecar before lookup.
    let primary_key = compose_primary_key(
        &tool_id_hash,
        &key_args,
        &env,
        cwd,
        cwd_in_key,
        &input_pairs,
        &scan_pairs,
        output_files,
        &input_extra,
    );
    let primary_hex = primary_key.to_hex();

    // 6. Path B: if the caller declared a depfile, try to load a stored dep
    //    set keyed by the primary. On hit we have the *full* key; on miss
    //    we use the primary as the full key for now (first invocation).
    let depfile_path = depfile.map(|p| absolutize(p, cwd));
    let stored_dep_pairs = if depfile_path.is_some() {
        load_depfile_sidecar(state, &primary_hex, cwd)
    } else {
        None
    };
    let full_key = match &stored_dep_pairs {
        Some(deps) => compose_full_key(&primary_key, deps),
        None => primary_key,
    };
    let full_hex = full_key.to_hex();

    let bypass = non_deterministic || matches!(cache_policy, ExecCachePolicy::Bypass);
    let lookup_allowed = !bypass
        && matches!(
            cache_policy,
            ExecCachePolicy::Normal | ExecCachePolicy::ReadOnly
        );
    let store_allowed = !bypass && matches!(cache_policy, ExecCachePolicy::Normal);

    // 7. Cache lookup (Normal + ReadOnly) under the full key.
    if lookup_allowed {
        if let Some(resp) =
            try_exec_cache_hit(state, &full_hex, cwd, output_files, output_streams).await
        {
            return resp;
        }
    }

    // 8. Coalesce concurrent callers with the same full key: insert into
    //    `in_flight_exec`. If someone else is already there, wait for them
    //    and retry the lookup — exactly one tool spawn services the herd.
    let coalesce_guard = if lookup_allowed {
        Some(acquire_in_flight(state, &full_hex).await)
    } else {
        None
    };
    if let Some(InFlight::WokenByPeer) = coalesce_guard.as_ref().map(|g| g.outcome()) {
        if let Some(resp) =
            try_exec_cache_hit(state, &full_hex, cwd, output_files, output_streams).await
        {
            return resp;
        }
        // Fell through: peer ran but didn't store, or store failed. Run it
        // ourselves below; we still hold the guard so further peers wait
        // for us, not the original runner.
    }

    // 9. Cache miss — run the tool.
    let output = match spawn_tool(tool, args, cwd, &env).await {
        Ok(o) => o,
        Err(e) => {
            return Response::Error {
                message: format!("failed to run {}: {e}", tool.display()),
            };
        }
    };

    let exit_code = output.status.code().unwrap_or(1);
    let stdout = Arc::new(if output_streams.stdout {
        output.stdout
    } else {
        Vec::new()
    });
    let stderr = Arc::new(if output_streams.stderr {
        output.stderr
    } else {
        Vec::new()
    });

    // 10. Snapshot declared output files. A missing declared file marks the
    //     run uncacheable — the next request can't replay something we
    //     never captured.
    let (captured_outputs, cache_outputs, all_captured) = snapshot_output_files(output_files, cwd);

    // 11. Path B: after a successful run, parse the depfile the tool wrote,
    //     persist the dep set under the primary key, and re-compose the
    //     full key. Failure modes:
    //     - depfile missing → skip storing (next call is a clean first run)
    //     - parse error    → skip storing
    //     - listed file missing → skip storing
    let mut final_full_hex = full_hex.clone();
    let mut depfile_ok = true;
    if let Some(df_path) = depfile_path.as_deref() {
        if exit_code != 0 {
            // Tool failed → don't trust the depfile, don't store anything
            // depfile-derived. The non-zero exit code is still returned but
            // the caching decision below will skip the store anyway when
            // we mark depfile_ok = false.
            depfile_ok = false;
        } else {
            match harvest_depfile(state, df_path, cwd, &input_pairs) {
                Ok(dep_pairs) => {
                    persist_depfile_sidecar(state, &primary_hex, &dep_pairs);
                    let new_full = compose_full_key(&primary_key, &dep_pairs);
                    final_full_hex = new_full.to_hex();
                }
                Err(e) => {
                    tracing::warn!(
                        depfile = %df_path.display(),
                        err = %e,
                        "depfile parse failed; skipping cache store for this run"
                    );
                    depfile_ok = false;
                }
            }
        }
    }

    // 12. Skip caching when the cap was blown — keeps the cache from
    //     carrying multi-hundred-MB stdout that won't fit in an IPC frame.
    let too_large = stdout.len() > EXEC_STREAM_CAP_BYTES || stderr.len() > EXEC_STREAM_CAP_BYTES;

    let cacheable_exit = true; // exit codes are part of the cached payload — even non-zero
    if store_allowed && all_captured && !too_large && depfile_ok && cacheable_exit {
        let artifact = ArtifactData {
            outputs: cache_outputs,
            stdout: Arc::clone(&stdout),
            stderr: Arc::clone(&stderr),
            exit_code,
        };
        store_exec_artifact(state, final_full_hex.clone(), artifact).await;
    } else if too_large {
        tracing::warn!(
            stdout_len = stdout.len(),
            stderr_len = stderr.len(),
            cap = EXEC_STREAM_CAP_BYTES,
            "exec output exceeded cache cap; not storing this run"
        );
    } else if non_deterministic {
        tracing::debug!(
            key = %final_full_hex,
            "exec marked non-deterministic; not storing"
        );
    }

    drop(coalesce_guard); // Releasing the guard wakes any waiters.

    Response::GenericToolExecResult {
        exit_code,
        stdout,
        stderr,
        output_files: captured_outputs,
        cached: false,
        cache_key_hex: final_full_hex,
    }
}

// ─── Key composition ─────────────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
fn compose_primary_key(
    tool_id: &ContentHash,
    args: &[String],
    env: &[(String, String)],
    cwd: &Path,
    cwd_in_key: bool,
    input_pairs: &[(String, ContentHash)],
    scan_pairs: &[(String, ContentHash)],
    output_files: &[NormalizedPath],
    input_extra: &Arc<Vec<u8>>,
) -> ContentHash {
    let mut hasher = blake3::Hasher::new();
    hasher.update(EXEC_KEY_DOMAIN);
    hasher.update(tool_id.as_bytes());

    hasher.update(b"args:");
    hasher.update(&(args.len() as u64).to_le_bytes());
    for a in args {
        hasher.update(&(a.len() as u64).to_le_bytes());
        hasher.update(a.as_bytes());
    }

    let mut env_sorted: Vec<(&str, &str)> =
        env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    env_sorted.sort_by(|a, b| a.0.cmp(b.0));
    hasher.update(b"env:");
    hasher.update(&(env_sorted.len() as u64).to_le_bytes());
    for (k, v) in &env_sorted {
        hasher.update(&(k.len() as u64).to_le_bytes());
        hasher.update(k.as_bytes());
        hasher.update(&(v.len() as u64).to_le_bytes());
        hasher.update(v.as_bytes());
    }

    if cwd_in_key {
        let cwd_str = normalize_for_key(cwd);
        hasher.update(b"cwd:");
        hasher.update(&(cwd_str.len() as u64).to_le_bytes());
        hasher.update(cwd_str.as_bytes());
    } else {
        hasher.update(b"cwd:omitted");
    }

    mix_path_hash_pairs(&mut hasher, b"inputs:", input_pairs);
    mix_path_hash_pairs(&mut hasher, b"scan:", scan_pairs);

    let mut out_names: Vec<String> = output_files
        .iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect();
    out_names.sort();
    hasher.update(b"outs:");
    hasher.update(&(out_names.len() as u64).to_le_bytes());
    for name in &out_names {
        hasher.update(&(name.len() as u64).to_le_bytes());
        hasher.update(name.as_bytes());
    }

    hasher.update(b"extra:");
    hasher.update(&(input_extra.len() as u64).to_le_bytes());
    hasher.update(input_extra);

    ContentHash::from_bytes(*hasher.finalize().as_bytes())
}

/// Compose the full key by extending the primary with depfile-derived deps.
/// The pairs MUST be sorted by path (the helpers that build them already do
/// this); we re-sort defensively so the function is robust against future
/// callers that forget.
fn compose_full_key(primary: &ContentHash, dep_pairs: &[(String, ContentHash)]) -> ContentHash {
    let mut sorted = dep_pairs.to_vec();
    sorted.sort_by(|a, b| a.0.cmp(&b.0));
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"zccache-exec-full-key-v2");
    hasher.update(primary.as_bytes());
    mix_path_hash_pairs(&mut hasher, b"depfile:", &sorted);
    ContentHash::from_bytes(*hasher.finalize().as_bytes())
}

fn mix_path_hash_pairs(hasher: &mut blake3::Hasher, tag: &[u8], pairs: &[(String, ContentHash)]) {
    hasher.update(tag);
    hasher.update(&(pairs.len() as u64).to_le_bytes());
    for (path, hash) in pairs {
        hasher.update(&(path.len() as u64).to_le_bytes());
        hasher.update(path.as_bytes());
        hasher.update(hash.as_bytes());
    }
}

// ─── Key args filter ─────────────────────────────────────────────────────

fn apply_key_args_filter(args: &[String], patterns: &[String]) -> Result<Vec<String>, String> {
    if patterns.is_empty() {
        return Ok(args.to_vec());
    }
    let regexes: Vec<regex::Regex> = patterns
        .iter()
        .map(|p| regex::Regex::new(p).map_err(|e| format!("{p:?}: {e}")))
        .collect::<Result<_, _>>()?;
    Ok(args
        .iter()
        .filter(|a| !regexes.iter().any(|r| r.is_match(a)))
        .cloned()
        .collect())
}

// ─── Path A: include scan ───────────────────────────────────────────────

fn run_include_scan(
    state: &Arc<SharedState>,
    cwd: &Path,
    seeds: &[NormalizedPath],
    include_dirs: &[NormalizedPath],
    system_include_dirs: &[NormalizedPath],
    iquote_dirs: &[NormalizedPath],
) -> Result<Vec<(String, ContentHash)>, String> {
    if seeds.is_empty() {
        return Ok(Vec::new());
    }
    let search = IncludeSearchPaths {
        iquote: iquote_dirs
            .iter()
            .map(|p| absolutize_norm(p, cwd))
            .collect(),
        user: include_dirs
            .iter()
            .map(|p| absolutize_norm(p, cwd))
            .collect(),
        system: system_include_dirs
            .iter()
            .map(|p| absolutize_norm(p, cwd))
            .collect(),
        after: Vec::new(),
    };

    let mut resolved: Vec<NormalizedPath> = Vec::new();
    for seed in seeds {
        let abs = absolutize_norm(seed, cwd);
        let scan = scan_recursive(abs.as_path(), &search);
        resolved.extend(scan.resolved);
        if scan.has_computed {
            tracing::warn!(
                seed = %abs.display(),
                "include scan encountered #include MACRO (computed include) — key may be over-broad"
            );
        }
    }
    // Dedup + sort by path so the key is stable.
    resolved.sort();
    resolved.dedup();

    let mut pairs: Vec<(String, ContentHash)> = Vec::with_capacity(resolved.len());
    for header in &resolved {
        let abs = header.as_path();
        let hash = hash_file_via_cache(state, abs)
            .ok_or_else(|| format!("include-scan: cannot hash {}", abs.display()))?;
        pairs.push((normalize_for_key(abs), hash));
    }
    Ok(pairs)
}

// ─── Path B: depfile + sidecar ──────────────────────────────────────────

/// On a warm invocation, load the dep set the previous run persisted under
/// `<primary_hex>.deps`. Returns `None` when no sidecar exists (first run)
/// or when any listed dep file no longer exists — that case forces a fresh
/// first-run because the cached dep set is stale.
fn load_depfile_sidecar(
    state: &Arc<SharedState>,
    primary_hex: &str,
    _cwd: &Path,
) -> Option<Vec<(String, ContentHash)>> {
    let path = depfile_sidecar_path(&state.artifact_dir, primary_hex);
    let content = std::fs::read_to_string(&path).ok()?;
    let mut pairs: Vec<(String, ContentHash)> = Vec::new();
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let abs = Path::new(trimmed);
        if !abs.exists() {
            tracing::debug!(
                missing = %abs.display(),
                "depfile sidecar references vanished file; treating as miss"
            );
            return None;
        }
        let hash = hash_file_via_cache(state, abs)?;
        pairs.push((normalize_for_key(abs), hash));
    }
    pairs.sort_by(|a, b| a.0.cmp(&b.0));
    Some(pairs)
}

fn persist_depfile_sidecar(
    state: &Arc<SharedState>,
    primary_hex: &str,
    dep_pairs: &[(String, ContentHash)],
) {
    let path = depfile_sidecar_path(&state.artifact_dir, primary_hex);
    let mut content = String::new();
    for (p, _) in dep_pairs {
        content.push_str(p);
        content.push('\n');
    }
    if let Err(e) = std::fs::write(&path, content) {
        tracing::warn!(path = %path.display(), err = %e, "failed to write depfile sidecar");
    }
}

fn depfile_sidecar_path(artifact_dir: &Path, primary_hex: &str) -> PathBuf {
    artifact_dir.join(format!("{primary_hex}.deps"))
}

/// Parse the depfile the tool emitted, hash each listed file. `inputs` is
/// the already-declared primary input set; we exclude any path that's
/// already in that set so the dep-set hash doesn't double-count them.
fn harvest_depfile(
    state: &Arc<SharedState>,
    depfile_path: &Path,
    cwd: &Path,
    inputs: &[(String, ContentHash)],
) -> Result<Vec<(String, ContentHash)>, String> {
    let content =
        std::fs::read_to_string(depfile_path).map_err(|e| format!("read depfile: {e}"))?;
    // `parse_depfile` takes a `source` path it excludes from results; we
    // pass an empty path so nothing is excluded — exec doesn't have a
    // single "source", any included files get filtered against the
    // declared input set below.
    let scan = crate::depgraph::depfile::parse_depfile(&content, Path::new(""), cwd)
        .map_err(|e| format!("parse depfile: {e}"))?;

    let declared: std::collections::HashSet<&str> =
        inputs.iter().map(|(p, _)| p.as_str()).collect();

    let mut pairs: Vec<(String, ContentHash)> = Vec::new();
    for dep in &scan.resolved {
        let abs = dep.as_path();
        if !abs.exists() {
            return Err(format!(
                "depfile references missing file: {}",
                abs.display()
            ));
        }
        let key = normalize_for_key(abs);
        if declared.contains(key.as_str()) {
            continue;
        }
        let hash = hash_file_via_cache(state, abs)
            .ok_or_else(|| format!("cannot hash dep {}", abs.display()))?;
        pairs.push((key, hash));
    }
    pairs.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(pairs)
}

// ─── Coalescing ─────────────────────────────────────────────────────────

enum InFlight {
    /// We acquired the slot ourselves (no peer was running).
    Owner,
    /// We waited on a peer's `Notify`; caller should re-attempt the cache
    /// lookup before spawning the tool.
    WokenByPeer,
}

struct InFlightGuardExec {
    state: Arc<SharedState>,
    key: String,
    outcome: InFlight,
}

impl InFlightGuardExec {
    fn outcome(&self) -> InFlight {
        match self.outcome {
            InFlight::Owner => InFlight::Owner,
            InFlight::WokenByPeer => InFlight::WokenByPeer,
        }
    }
}

impl Drop for InFlightGuardExec {
    fn drop(&mut self) {
        if matches!(self.outcome, InFlight::Owner) {
            // Remove the slot, then wake any waiters.
            if let Some((_, notify)) = self.state.in_flight_exec.remove(&self.key) {
                notify.notify_waiters();
            }
        }
    }
}

async fn acquire_in_flight(state: &Arc<SharedState>, key_hex: &str) -> InFlightGuardExec {
    let key = key_hex.to_string();
    let notify_arc = {
        match state.in_flight_exec.entry(key.clone()) {
            Entry::Occupied(o) => Arc::clone(o.get()),
            Entry::Vacant(v) => {
                v.insert(Arc::new(Notify::new()));
                return InFlightGuardExec {
                    state: Arc::clone(state),
                    key,
                    outcome: InFlight::Owner,
                };
            }
        }
    };
    notify_arc.notified().await;
    InFlightGuardExec {
        state: Arc::clone(state),
        key,
        outcome: InFlight::WokenByPeer,
    }
}

// ─── Spawn + capture ────────────────────────────────────────────────────

async fn spawn_tool(
    tool: &Path,
    args: &[String],
    cwd: &Path,
    env: &[(String, String)],
) -> std::io::Result<std::process::Output> {
    let tool_owned = tool.to_path_buf();
    let args_owned: Vec<String> = args.to_vec();
    let cwd_owned = cwd.to_path_buf();
    let env_owned = env.to_vec();
    tokio::task::spawn_blocking(move || {
        let mut cmd = std::process::Command::new(&tool_owned);
        cmd.args(&args_owned).current_dir(&cwd_owned);
        // Clear env and apply only the declared subset so the run is
        // reproducible across hosts that may have unrelated env differences.
        // PATH is intentionally NOT auto-injected — callers that need it
        // must declare `--input-env PATH` so it participates in the key.
        cmd.env_clear();
        for (k, v) in &env_owned {
            cmd.env(k, v);
        }
        crate::daemon::process::command_output_with_priority(&mut cmd, CompilePriority::Normal)
    })
    .await
    .unwrap_or_else(|e| Err(std::io::Error::other(format!("join error: {e}"))))
}

fn snapshot_output_files(
    output_files: &[NormalizedPath],
    cwd: &Path,
) -> (Vec<ArtifactOutput>, Vec<ArtifactOutput>, bool) {
    let mut captured_outputs: Vec<ArtifactOutput> = Vec::with_capacity(output_files.len());
    let mut cache_outputs: Vec<ArtifactOutput> = Vec::with_capacity(output_files.len());
    let mut all_captured = true;
    for declared in output_files {
        let abs: PathBuf = absolutize(declared.as_path(), cwd);
        match std::fs::read(&abs) {
            Ok(bytes) => {
                let payload = ArtifactPayload::Bytes(Arc::new(bytes));
                captured_outputs.push(ArtifactOutput {
                    name: declared.to_string_lossy().into_owned(),
                    payload: payload.clone(),
                });
                cache_outputs.push(ArtifactOutput {
                    name: declared.to_string_lossy().into_owned(),
                    payload,
                });
            }
            Err(e) => {
                all_captured = false;
                tracing::warn!(
                    path = %abs.display(),
                    err = %e,
                    "declared output file missing after exec; not caching this output"
                );
            }
        }
    }
    (captured_outputs, cache_outputs, all_captured)
}

// ─── Replay (cache hit) ─────────────────────────────────────────────────

async fn try_exec_cache_hit(
    state: &Arc<SharedState>,
    key_hex: &str,
    cwd: &Path,
    output_files: &[NormalizedPath],
    output_streams: ExecOutputStreams,
) -> Option<Response> {
    let mut entry = lookup_artifact_with_disk_fallback(state, key_hex)?;
    entry.last_used = std::time::Instant::now();

    let exit_code = entry.meta.exit_code;
    let stdout_full = entry.stdout.clone();
    let stderr_full = entry.stderr.clone();
    let names = Arc::clone(&entry.meta.output_names);

    let payloads_loaded = ensure_payloads(&mut entry, &state.artifact_dir, key_hex).is_some();
    if !payloads_loaded {
        return None;
    }
    let payloads = Arc::clone(entry.payloads.as_ref().unwrap());
    drop(entry);

    let mut paired: Vec<(NormalizedPath, &CachedPayload)> = Vec::with_capacity(output_files.len());
    for declared in output_files {
        let declared_name = declared.to_string_lossy().into_owned();
        let idx = names.iter().position(|n| n == &declared_name)?;
        let payload = payloads.get(idx)?;
        let abs: NormalizedPath = if declared.as_path().is_absolute() {
            declared.clone()
        } else {
            cwd.join(declared.as_path()).into()
        };
        paired.push((abs, payload));
    }

    let targets: Vec<(NormalizedPath, NormalizedPath)> = paired
        .iter()
        .enumerate()
        .map(|(i, (abs, _))| {
            let cache_file = state.artifact_dir.join(format!("{key_hex}_{i}"));
            (abs.clone(), cache_file)
        })
        .collect();
    let payloads_for_write: Vec<CachedPayload> =
        paired.into_iter().map(|(_, p)| p.clone()).collect();
    if !write_payloads_par(&targets, &payloads_for_write) {
        return None;
    }

    let response_stdout = if output_streams.stdout {
        stdout_full
    } else {
        Arc::new(Vec::new())
    };
    let response_stderr = if output_streams.stderr {
        stderr_full
    } else {
        Arc::new(Vec::new())
    };

    let mut response_outputs: Vec<ArtifactOutput> = Vec::with_capacity(targets.len());
    for ((abs, _), payload) in targets.iter().zip(payloads_for_write.iter()) {
        let bytes: Arc<Vec<u8>> = match payload {
            CachedPayload::Bytes(b) => Arc::clone(b),
            CachedPayload::File(p) => match std::fs::read(p.as_path()) {
                Ok(b) => Arc::new(b),
                Err(_) => Arc::new(Vec::new()),
            },
            // PendingFile is produced by the rust miss path (issue #632)
            // and shouldn't reach the generic exec route, but keep the
            // match exhaustive: try the source path the way `File` would.
            CachedPayload::PendingFile { source_path } => {
                match std::fs::read(source_path.as_path()) {
                    Ok(b) => Arc::new(b),
                    Err(_) => Arc::new(Vec::new()),
                }
            }
        };
        response_outputs.push(ArtifactOutput {
            name: abs.to_string_lossy().into_owned(),
            payload: ArtifactPayload::Bytes(bytes),
        });
    }

    Some(Response::GenericToolExecResult {
        exit_code,
        stdout: response_stdout,
        stderr: response_stderr,
        output_files: response_outputs,
        cached: true,
        cache_key_hex: key_hex.to_string(),
    })
}

// ─── Store ──────────────────────────────────────────────────────────────

async fn store_exec_artifact(state: &Arc<SharedState>, key_hex: String, artifact: ArtifactData) {
    let cached = CachedArtifact::from_artifact_data(&artifact);
    {
        let artifact_dir = state.artifact_dir.clone();
        let key_for_persist = key_hex.clone();
        let payloads: Vec<Arc<Vec<u8>>> = artifact
            .outputs
            .iter()
            .filter_map(|o| o.payload.as_bytes().cloned())
            .collect();
        let persist_meta = cached.meta.clone();
        if payloads.is_empty() {
            // Stdout/stderr-only artifacts have no payload files to persist,
            // so expose their index entry to the final shutdown flush now.
            state.artifact_store.insert(&key_hex, &persist_meta);
        }
        let state_ref = Arc::clone(state);
        let sem = Arc::clone(&state.persist_semaphore);
        tokio::spawn(async move {
            let _permit = sem.acquire().await.unwrap();
            let written = tokio::task::spawn_blocking(move || {
                let _ = persist_artifact_payloads(&artifact_dir, &key_for_persist, &payloads);
                (key_for_persist, persist_meta)
            })
            .await;
            if let Ok((kh, meta)) = written {
                let _ = state_ref.index_writer_tx.send((kh, meta));
            }
        });
    }
    state.artifacts.insert(key_hex, cached);
}

// ─── Path normalization ─────────────────────────────────────────────────

fn absolutize(p: &Path, cwd: &Path) -> PathBuf {
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        cwd.join(p)
    }
}

fn absolutize_norm(p: &NormalizedPath, cwd: &Path) -> NormalizedPath {
    let ap = absolutize(p.as_path(), cwd);
    NormalizedPath::from(ap.as_path())
}

/// Stable forward-slash-only path representation for use in cache keys.
/// Mirrors what other handlers do via `core::path::normalize_for_key` — kept
/// local so this module has no extra deps to wire.
fn normalize_for_key(path: &Path) -> String {
    let s = path.to_string_lossy().into_owned();
    s.replace('\\', "/")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(byte: u8) -> ContentHash {
        ContentHash::from_bytes([byte; 32])
    }

    fn empty_extra() -> Arc<Vec<u8>> {
        Arc::new(Vec::new())
    }

    #[test]
    fn primary_key_changes_when_input_hash_changes() {
        let k1 = compose_primary_key(
            &h(1),
            &["--json".into()],
            &[("PATH".into(), "/bin".into())],
            Path::new("/p"),
            true,
            &[("src/a.cpp".into(), h(2))],
            &[],
            &[NormalizedPath::from("out.json")],
            &empty_extra(),
        );
        let k2 = compose_primary_key(
            &h(1),
            &["--json".into()],
            &[("PATH".into(), "/bin".into())],
            Path::new("/p"),
            true,
            &[("src/a.cpp".into(), h(3))],
            &[],
            &[NormalizedPath::from("out.json")],
            &empty_extra(),
        );
        assert_ne!(k1, k2);
    }

    #[test]
    fn primary_key_stable_for_env_order() {
        let k1 = compose_primary_key(
            &h(1),
            &[],
            &[
                ("PATH".into(), "/bin".into()),
                ("LINT_VER".into(), "1".into()),
            ],
            Path::new("/p"),
            true,
            &[],
            &[],
            &[],
            &empty_extra(),
        );
        let k2 = compose_primary_key(
            &h(1),
            &[],
            &[
                ("LINT_VER".into(), "1".into()),
                ("PATH".into(), "/bin".into()),
            ],
            Path::new("/p"),
            true,
            &[],
            &[],
            &[],
            &empty_extra(),
        );
        assert_eq!(k1, k2);
    }

    #[test]
    fn full_key_extends_primary_with_depfile_deps() {
        let primary = compose_primary_key(
            &h(1),
            &[],
            &[],
            Path::new("/p"),
            true,
            &[],
            &[],
            &[],
            &empty_extra(),
        );
        let k_no_deps = compose_full_key(&primary, &[]);
        let k_with = compose_full_key(&primary, &[("h.h".into(), h(9))]);
        // Without deps, full key is *not* equal to primary because of the
        // domain tag — but it must differ from a key with deps.
        assert_ne!(k_no_deps, k_with);
    }

    #[test]
    fn full_key_order_independent_for_dep_pairs() {
        let primary = compose_primary_key(
            &h(1),
            &[],
            &[],
            Path::new("/p"),
            true,
            &[],
            &[],
            &[],
            &empty_extra(),
        );
        let a = vec![("a.h".into(), h(2)), ("b.h".into(), h(3))];
        let b = vec![("b.h".into(), h(3)), ("a.h".into(), h(2))];
        assert_eq!(
            compose_full_key(&primary, &a),
            compose_full_key(&primary, &b)
        );
    }

    #[test]
    fn key_args_filter_drops_matching_args() {
        let filtered = apply_key_args_filter(
            &[
                "compile".into(),
                "--verbose".into(),
                "--no-color".into(),
                "src.cpp".into(),
            ],
            &["^--verbose$".into(), "^--no-color$".into()],
        )
        .unwrap();
        assert_eq!(filtered, vec!["compile".to_string(), "src.cpp".to_string()]);
    }

    #[test]
    fn key_args_filter_invalid_regex_errors() {
        let err = apply_key_args_filter(&["a".into()], &["(".into()]).unwrap_err();
        assert!(err.contains('('));
    }

    #[test]
    fn primary_key_differs_when_scan_changes() {
        let k1 = compose_primary_key(
            &h(1),
            &[],
            &[],
            Path::new("/p"),
            true,
            &[],
            &[("hdr.h".into(), h(7))],
            &[],
            &empty_extra(),
        );
        let k2 = compose_primary_key(
            &h(1),
            &[],
            &[],
            Path::new("/p"),
            true,
            &[],
            &[("hdr.h".into(), h(8))],
            &[],
            &empty_extra(),
        );
        assert_ne!(k1, k2);
    }
}
