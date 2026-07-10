//! Rustc error-cache helpers for the compile pipeline.

use super::super::*;

pub(super) fn compile_failure_stderr(message: String) -> Response {
    let mut stderr = message.into_bytes();
    stderr.push(b'\n');
    Response::CompileResult {
        exit_code: 1,
        stdout: Arc::new(Vec::new()),
        stderr: Arc::new(stderr),
        cached: false,
    }
}

fn rustc_depinfo_exists(rustc_args: &crate::depgraph::RustcParsedArgs, cwd: &Path) -> bool {
    if !rustc_args.emit_types.iter().any(|emit| emit == "dep-info") {
        return false;
    }
    let name = rustc_args.crate_name.as_deref().unwrap_or("unknown");
    let ext_suffix = rustc_args.extra_filename.as_deref().unwrap_or("");
    let dir = rustc_args.out_dir.as_deref().unwrap_or(cwd);
    dir.join(format!("{name}{ext_suffix}.d")).exists()
}

fn should_cache_rustc_error(
    rustc_args: &crate::depgraph::RustcParsedArgs,
    exit_code: i32,
    cwd: &Path,
) -> bool {
    exit_code > 0
        && rustc_depinfo_exists(rustc_args, cwd)
        && !rustc_args.emit_types.iter().any(|emit| emit == "link")
}

#[allow(clippy::too_many_arguments)] // Localized error-cache insertion path.
pub(super) fn maybe_store_rustc_error_artifact(
    state: &SharedState,
    context_key: &ContextKey,
    source_path: &NormalizedPath,
    cwd_path: &NormalizedPath,
    ctx: &CompileContext,
    rustc_args: &crate::depgraph::RustcParsedArgs,
    stdout: &Arc<Vec<u8>>,
    stderr: &Arc<Vec<u8>>,
    exit_code: i32,
    snap_clock: Clock,
) -> Option<String> {
    if !should_cache_rustc_error(rustc_args, exit_code, cwd_path) {
        return None;
    }

    // Error-cache path ignores env-dep names: failed compiles are keyed
    // conservatively and re-validated on every replay (zccache#1021).
    let scan_result = scan_rustc_deps(rustc_args, source_path, cwd_path).scan;
    let tracked_paths: Vec<NormalizedPath> = std::iter::once(source_path.clone())
        .chain(scan_result.resolved.iter().cloned())
        .chain(ctx.force_includes.iter().cloned())
        .collect();
    state.cache_system.register_tracked(&tracked_paths);

    let mut hash_map: HashMap<NormalizedPath, ContentHash> = HashMap::new();
    for path in &tracked_paths {
        let hash_path =
            resolve_pch_source(path, &state.pch_source_map).unwrap_or_else(|| path.clone());
        let hash = hash_file(&state.cache_system, &hash_path, snap_clock).ok()?;
        hash_map.insert(path.clone(), hash);
    }

    let get_hash = |p: &Path| {
        let path = NormalizedPath::new(p);
        hash_map.get(&path).copied()
    };
    let artifact_key = state
        .dep_graph
        .load()
        .update(context_key, scan_result, get_hash)?;
    let artifact_key_hex = artifact_key.hash().to_hex();
    let meta = ArtifactIndex::new(
        Vec::new(),
        Vec::new(),
        Arc::clone(stdout),
        Arc::clone(stderr),
        exit_code,
    );
    state.artifact_store.insert(&artifact_key_hex, &meta);
    state.artifacts.insert(
        artifact_key_hex.clone(),
        CachedArtifact::from_file_payloads(meta, Vec::new()),
    );
    Some(artifact_key_hex)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rustc_error_cache_requires_depinfo_and_no_link_emit() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("probe.rs");
        std::fs::write(&src, "fn main() {}\n").unwrap();
        let args = vec![
            "--crate-name".to_string(),
            "probe".to_string(),
            "--emit=dep-info,metadata".to_string(),
            "--out-dir".to_string(),
            tmp.path().to_string_lossy().into_owned(),
            src.to_string_lossy().into_owned(),
        ];
        let parsed = crate::depgraph::parse_rustc_args(&args, tmp.path());

        assert!(!should_cache_rustc_error(&parsed, 1, tmp.path()));

        std::fs::write(tmp.path().join("probe.d"), "probe.d: probe.rs\n").unwrap();
        assert!(should_cache_rustc_error(&parsed, 1, tmp.path()));
        assert!(!should_cache_rustc_error(&parsed, -1, tmp.path()));

        let mut link_args = args.clone();
        link_args[2] = "--emit=dep-info,link".to_string();
        let link_parsed = crate::depgraph::parse_rustc_args(&link_args, tmp.path());
        assert!(!should_cache_rustc_error(&link_parsed, 1, tmp.path()));
    }
}
