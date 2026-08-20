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
    stderr: &[u8],
) -> bool {
    // A failure that explained nothing is not a compile error worth
    // remembering. On Windows a child the watchdog kills exits with exactly
    // code 1 and no stderr (`TerminateProcess(handle, 1)`), which is
    // indistinguishable here from a genuine rustc rejection — see
    // `child_watchdog::deliver_fault_note` and soldr#1857. Caching one of those
    // turns a transient, load-dependent fault into a sticky replayed
    // `cached_error` for every later build of that unit, which is far worse
    // than simply recompiling.
    !stderr.is_empty()
        && exit_code > 0
        && rustc_depinfo_exists(rustc_args, cwd)
        && !rustc_args.emit_types.iter().any(|emit| emit == "link")
}

fn commit_rustc_verdict(
    state: &SharedState,
    artifact_key_hex: &str,
    verdict_key_hex: String,
    verdict: ArtifactVerdict,
) {
    use dashmap::mapref::entry::Entry;
    let durable = super::rustc_index::durable_rustc_index(state, artifact_key_hex);
    let mut incoming = ArtifactIndex::new(
        Vec::new(),
        Vec::new(),
        Arc::new(Vec::new()),
        Arc::new(Vec::new()),
        0,
    );
    incoming.rustc_verdicts.insert(verdict_key_hex, verdict);
    if let Some(durable) = durable {
        incoming = super::rustc_index::merge_rustc_index(incoming, durable);
    }

    match state.artifacts.entry(artifact_key_hex.to_string()) {
        Entry::Occupied(mut entry) => {
            let artifact_meta =
                super::rustc_index::merge_rustc_index(incoming, entry.get().meta.clone());
            enqueue_index_insert(state, artifact_key_hex.to_string(), artifact_meta.clone());
            entry.insert(CachedArtifact::from_index(artifact_meta));
        }
        Entry::Vacant(entry) => {
            enqueue_index_insert(state, artifact_key_hex.to_string(), incoming.clone());
            entry.insert(CachedArtifact::from_index(incoming));
        }
    }
}

#[allow(clippy::too_many_arguments)] // Localized error-cache insertion path.
pub(super) async fn maybe_store_rustc_error_artifact(
    state: &SharedState,
    context_key: &ContextKey,
    source_path: &NormalizedPath,
    cwd_path: &NormalizedPath,
    ctx: &CompileContext,
    rustc_args: &crate::depgraph::RustcParsedArgs,
    dylint_input_hash: Option<&str>,
    stdout: &Arc<Vec<u8>>,
    stderr: &Arc<Vec<u8>>,
    exit_code: i32,
    snap_clock: Clock,
) -> Option<String> {
    if !should_cache_rustc_error(rustc_args, exit_code, cwd_path, stderr) {
        return None;
    }

    // Error artifacts participate in the same Clear/GC barrier and ordered
    // index-writer WAL as successful artifacts. Sharing one writer prevents
    // an older queued success row from overwriting a newer error verdict.
    let _publication_guard = begin_artifact_publication(state).await?;

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
    let verdict_key_hex =
        crate::depgraph::compute_rustc_verdict_key(&artifact_key_hex, dylint_input_hash)
            .hash()
            .to_hex();
    let verdict = ArtifactVerdict {
        stdout: Arc::clone(stdout),
        stderr: Arc::clone(stderr),
        exit_code,
    };
    commit_rustc_verdict(state, &artifact_key_hex, verdict_key_hex, verdict);
    Some(artifact_key_hex)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn cold_error_verdict_merges_durable_outputs_and_sibling_verdicts() {
        let tmp = tempfile::tempdir().unwrap();
        let mut server = crate::daemon::server::tests::bind_isolated_server(tmp.path());
        let mut durable = ArtifactIndex::new(
            vec!["shared.rmeta".to_string()],
            vec![17],
            Arc::new(Vec::new()),
            Arc::new(Vec::new()),
            0,
        );
        durable.rustc_verdicts.insert(
            "plain".to_string(),
            ArtifactVerdict {
                stdout: Arc::new(Vec::new()),
                stderr: Arc::new(Vec::new()),
                exit_code: 0,
            },
        );
        server.state.artifact_store.insert("artifact", &durable);
        assert!(!server.state.artifacts.contains_key("artifact"));

        commit_rustc_verdict(
            &server.state,
            "artifact",
            "dylint".to_string(),
            ArtifactVerdict {
                stdout: Arc::new(Vec::new()),
                stderr: Arc::new(b"lint error".to_vec()),
                exit_code: 1,
            },
        );

        let cached = server.state.artifacts.get("artifact").unwrap();
        assert_eq!(cached.meta.output_names.as_ref(), &["shared.rmeta"]);
        assert_eq!(cached.meta.rustc_verdicts.len(), 2);
        drop(cached);
        let command = server.index_writer_rx.as_mut().unwrap().try_recv().unwrap();
        let IndexWriterCommand::Insert(key, merged) = command else {
            panic!("cold verdict publication must use the ordered index writer");
        };
        assert_eq!(key, "artifact");
        assert_eq!(merged.output_names.as_ref(), &["shared.rmeta"]);
        assert_eq!(merged.rustc_verdicts.len(), 2);
    }

    #[tokio::test]
    async fn error_verdict_preserves_existing_shared_artifact_metadata() {
        let tmp = tempfile::tempdir().unwrap();
        let mut server = crate::daemon::server::tests::bind_isolated_server(tmp.path());
        let mut meta = ArtifactIndex::new(
            vec!["shared.rmeta".to_string()],
            vec![17],
            Arc::new(Vec::new()),
            Arc::new(Vec::new()),
            0,
        );
        meta.rustc_verdicts.insert(
            "plain".to_string(),
            ArtifactVerdict {
                stdout: Arc::new(Vec::new()),
                stderr: Arc::new(Vec::new()),
                exit_code: 0,
            },
        );
        enqueue_index_insert(&server.state, "artifact".to_string(), meta.clone());
        server
            .state
            .artifacts
            .insert("artifact".to_string(), CachedArtifact::from_index(meta));

        commit_rustc_verdict(
            &server.state,
            "artifact",
            "dylint".to_string(),
            ArtifactVerdict {
                stdout: Arc::new(Vec::new()),
                stderr: Arc::new(b"lint error".to_vec()),
                exit_code: 1,
            },
        );

        let cached = server.state.artifacts.get("artifact").unwrap();
        assert_eq!(cached.meta.output_names.as_ref(), &["shared.rmeta"]);
        assert_eq!(cached.meta.output_sizes, vec![17]);
        assert_eq!(cached.meta.rustc_verdicts.len(), 2);
        drop(cached);
        let first = server.index_writer_rx.as_mut().unwrap().try_recv().unwrap();
        let IndexWriterCommand::Insert(_, first) = first else {
            panic!("successful artifact publication must precede the verdict");
        };
        assert_eq!(first.rustc_verdicts.len(), 1);
        let second = server.index_writer_rx.as_mut().unwrap().try_recv().unwrap();
        let IndexWriterCommand::Insert(key, durable) = second else {
            panic!("verdict publication must use the ordered index writer");
        };
        assert_eq!(key, "artifact");
        assert_eq!(durable.output_names.as_ref(), &["shared.rmeta"]);
        assert_eq!(durable.rustc_verdicts.len(), 2);
    }

    #[tokio::test]
    async fn rustc_error_publication_waits_for_clear_barrier() {
        let tmp = tempfile::tempdir().unwrap();
        let source: NormalizedPath = tmp.path().join("probe.rs").into();
        std::fs::write(&source, "fn main() { missing(); }\n").unwrap();
        std::fs::write(
            tmp.path().join("probe.d"),
            format!("probe.d: {}\n", source.display()),
        )
        .unwrap();
        let args = vec![
            "--crate-name".to_string(),
            "probe".to_string(),
            "--emit=dep-info,metadata".to_string(),
            "--out-dir".to_string(),
            tmp.path().to_string_lossy().into_owned(),
            source.to_string_lossy().into_owned(),
        ];
        let rustc_args = crate::depgraph::parse_rustc_args(&args, tmp.path());
        let ctx = CompileContext {
            source_file: source.clone(),
            include_search: crate::depgraph::IncludeSearchPaths::default(),
            defines: Vec::new(),
            flags: Vec::new(),
            force_includes: Vec::new(),
            unknown_flags: Vec::new(),
            compiler_hash: crate::hash::hash_bytes(b"test-fixture"),
        };
        let context_key = ctx.context_key();
        let cache_dir: NormalizedPath = tmp.path().join("cache").into();
        let server =
            DaemonServer::bind_with_cache_dir(&crate::ipc::unique_test_endpoint(), &cache_dir)
                .unwrap();
        let state = Arc::clone(&server.state);
        let publication_write = Arc::clone(&state.artifact_publication).write_owned().await;
        let stdout = Arc::new(Vec::new());
        let stderr = Arc::new(b"cannot find function `missing`".to_vec());
        let mut publish = tokio::spawn(async move {
            maybe_store_rustc_error_artifact(
                &state,
                &context_key,
                &source,
                &NormalizedPath::new(tmp.path()),
                &ctx,
                &rustc_args,
                None,
                &stdout,
                &stderr,
                1,
                state.cache_system.current_clock(),
            )
            .await
        });
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(25), &mut publish)
                .await
                .is_err(),
            "error-cache publication must wait for the Clear/GC write barrier"
        );
        drop(publication_write);
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), publish)
            .await
            .unwrap()
            .unwrap();
    }

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

        assert!(!should_cache_rustc_error(&parsed, 1, tmp.path(), b"boom"));

        std::fs::write(tmp.path().join("probe.d"), "probe.d: probe.rs\n").unwrap();
        assert!(should_cache_rustc_error(&parsed, 1, tmp.path(), b"boom"));
        assert!(!should_cache_rustc_error(&parsed, -1, tmp.path(), b"boom"));
        // soldr#1857: a failure with no diagnostics must never be cached.
        assert!(!should_cache_rustc_error(&parsed, 1, tmp.path(), b""));

        let mut link_args = args.clone();
        link_args[2] = "--emit=dep-info,link".to_string();
        let link_parsed = crate::depgraph::parse_rustc_args(&link_args, tmp.path());
        assert!(!should_cache_rustc_error(
            &link_parsed,
            1,
            tmp.path(),
            b"boom"
        ));
    }
}
