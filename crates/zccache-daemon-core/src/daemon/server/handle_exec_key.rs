//! Cache-key, dependency-scan, and depfile-sidecar helpers for generic exec.

use super::*;

pub(super) fn compose_primary_key(
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
pub(super) fn compose_full_key(
    primary: &ContentHash,
    dep_pairs: &[(String, ContentHash)],
) -> ContentHash {
    let mut sorted = dep_pairs.to_vec();
    sorted.sort_by(|a, b| a.0.cmp(&b.0));
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"zccache-exec-full-key-v2");
    hasher.update(primary.as_bytes());
    mix_path_hash_pairs(&mut hasher, b"depfile:", &sorted);
    ContentHash::from_bytes(*hasher.finalize().as_bytes())
}

pub(super) fn mix_path_hash_pairs(
    hasher: &mut blake3::Hasher,
    tag: &[u8],
    pairs: &[(String, ContentHash)],
) {
    hasher.update(tag);
    hasher.update(&(pairs.len() as u64).to_le_bytes());
    for (path, hash) in pairs {
        hasher.update(&(path.len() as u64).to_le_bytes());
        hasher.update(path.as_bytes());
        hasher.update(hash.as_bytes());
    }
}

// ─── Key args filter ─────────────────────────────────────────────────────

pub(super) fn apply_key_args_filter(
    args: &[String],
    patterns: &[String],
) -> Result<Vec<String>, String> {
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

pub(super) fn run_include_scan(
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
pub(super) fn load_depfile_sidecar(
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

pub(super) fn persist_depfile_sidecar(
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

pub(super) fn depfile_sidecar_path(artifact_dir: &Path, primary_hex: &str) -> PathBuf {
    artifact_dir.join(format!("{primary_hex}.deps"))
}

/// Parse the depfile the tool emitted, hash each listed file. `inputs` is
/// the already-declared primary input set; we exclude any path that's
/// already in that set so the dep-set hash doesn't double-count them.
pub(super) fn harvest_depfile(
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
