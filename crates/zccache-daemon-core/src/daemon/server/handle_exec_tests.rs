//! Key-composition and exact-exec staging planner tests.
use super::*;
use tempfile::tempdir;

fn h(byte: u8) -> ContentHash {
    ContentHash::from_bytes([byte; 32])
}

fn empty_extra() -> Arc<Vec<u8>> {
    Arc::new(Vec::new())
}

#[tokio::test]
async fn staged_exec_publication_failure_never_inserts_cache_entry() {
    let temp = tempdir().unwrap();
    let endpoint = crate::ipc::unique_test_endpoint();
    let cache_dir: NormalizedPath = temp.path().join("cache").into();
    let server = DaemonServer::bind_with_cache_dir(&endpoint, &cache_dir).unwrap();
    let first: NormalizedPath = temp.path().join("first.out").into();
    let second: NormalizedPath = temp.path().join("second.out").into();
    std::fs::write(&first, b"first exact output").unwrap();
    std::fs::write(&second, b"second exact output").unwrap();
    let artifact = ArtifactData {
        outputs: vec![
            ArtifactOutput {
                name: "first.out".to_string(),
                payload: ArtifactPayload::Bytes(Arc::new(b"first exact output".to_vec())),
            },
            ArtifactOutput {
                name: "second.out".to_string(),
                payload: ArtifactPayload::Bytes(Arc::new(b"second exact output".to_vec())),
            },
        ],
        stdout: Arc::new(Vec::new()),
        stderr: Arc::new(Vec::new()),
        exit_code: 0,
    };
    let key = "e".repeat(64);
    let fault = StagedFaultGuard::arm(
        &server.state.artifact_dir,
        [StagedFaultPoint::PointerCommit],
    );

    let reason = match store_exec_artifact(
        &server.state,
        key.clone(),
        artifact,
        Some(vec![first, second]),
    )
    .await
    {
        Err(reason) => reason,
        Ok(_) => panic!("publication fault must fail the staged store"),
    };
    assert_eq!(reason, StagedPublishFailure::PointerCommit);
    assert!(!server.state.artifacts.contains_key(&key));
    let staged = server.state.profiler.staged.snapshot();
    assert_eq!(staged.counters["publication_failure"], 1);
    assert_eq!(staged.failures["pointer_commit"], 1);
    fault.assert_all_consumed();
}

#[tokio::test]
async fn staged_exec_publication_failure_salvages_requested_output() {
    let temp = tempdir().unwrap();
    let endpoint = crate::ipc::unique_test_endpoint();
    let cache_dir: NormalizedPath = temp.path().join("cache").into();
    let server = DaemonServer::bind_with_cache_dir(&endpoint, &cache_dir).unwrap();
    let root = temp.path().join("private-exec");
    std::fs::create_dir_all(&root).unwrap();
    let requested: NormalizedPath = temp.path().join("result.bin").into();
    let staged: NormalizedPath = root.join("result.bin").into();
    std::fs::write(&staged, b"complete exact output").unwrap();
    let plan = ExecStagedPlan {
        root,
        rewritten_args: Vec::new(),
        outputs: vec![(requested.clone(), staged.clone())],
    };
    let artifact = ArtifactData {
        outputs: vec![ArtifactOutput {
            name: "result.bin".to_string(),
            payload: ArtifactPayload::Bytes(Arc::new(b"complete exact output".to_vec())),
        }],
        stdout: Arc::new(Vec::new()),
        stderr: Arc::new(Vec::new()),
        exit_code: 0,
    };
    let key = "f".repeat(64);
    let fault = StagedFaultGuard::arm(
        &server.state.artifact_dir,
        [StagedFaultPoint::PointerCommit],
    );
    let reason =
        match store_exec_artifact(&server.state, key.clone(), artifact, Some(vec![staged])).await {
            Err(reason) => reason,
            Ok(_) => panic!("publication fault must fail the staged store"),
        };

    materialize_exec_plan_observed(&server.state, &plan, Some(reason.id())).unwrap();
    assert_eq!(std::fs::read(&requested).unwrap(), b"complete exact output");
    assert!(!server.state.artifacts.contains_key(&key));
    let staged = server.state.profiler.staged.snapshot();
    assert_eq!(staged.counters["publication_failure"], 1);
    assert_eq!(staged.counters["salvage_attempt"], 1);
    assert_eq!(staged.counters["salvage_success"], 1);
    assert_eq!(staged.failures["pointer_commit"], 1);
    fault.assert_all_consumed();
}

#[tokio::test]
async fn staged_exec_disk_hit_reports_physical_materialization_tier() {
    let temp = tempdir().unwrap();
    let endpoint = crate::ipc::unique_test_endpoint();
    let cache_dir: NormalizedPath = temp.path().join("cache").into();
    let server = DaemonServer::bind_with_cache_dir(&endpoint, &cache_dir).unwrap();
    let source: NormalizedPath = temp.path().join("private-result.bin").into();
    std::fs::write(&source, b"persisted exact output").unwrap();
    let artifact = ArtifactData {
        outputs: vec![ArtifactOutput {
            name: "result.bin".to_string(),
            payload: ArtifactPayload::Bytes(Arc::new(b"persisted exact output".to_vec())),
        }],
        stdout: Arc::new(Vec::new()),
        stderr: Arc::new(Vec::new()),
        exit_code: 0,
    };
    let metadata = CachedArtifact::from_artifact_data(&artifact).meta;
    let key = "d".repeat(64);
    let cached = store_exec_artifact(&server.state, key.clone(), artifact, Some(vec![source]))
        .await
        .unwrap()
        .expect("staged stores defer memory visibility until materialization");
    server.state.artifacts.insert(key.clone(), cached);
    server.state.artifacts.remove(&key);
    server.state.artifact_store.insert(&key, &metadata);

    let response = try_exec_cache_hit(
        &server.state,
        &key,
        temp.path(),
        &[NormalizedPath::from("result.bin")],
        ExecOutputStreams::default(),
    )
    .await;
    assert!(matches!(
        response,
        Some(Response::GenericToolExecResult { cached: true, .. })
    ));
    assert_eq!(
        std::fs::read(temp.path().join("result.bin")).unwrap(),
        b"persisted exact output"
    );
    let staged = server.state.profiler.staged.snapshot();
    assert_eq!(staged.counters["publication_success"], 1);
    assert_eq!(
        staged.counters["materialize_reflink"]
            + staged.counters["materialize_hardlink_shared"]
            + staged.counters["materialize_copy"],
        1
    );
    assert!(staged.timings_ns.contains_key("hit_materialization"));
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

#[test]
fn exact_exec_planner_rejections_have_stable_reasons() {
    if std::env::var_os(crate::daemon::server::persist::STAGED_ARTIFACTS_ENV).is_none() {
        return;
    }
    let temp = tempdir().unwrap();
    assert!(matches!(
        ExecStagedPlan::build(temp.path(), &[], &[], temp.path()),
        StagedPlanOutcome::Unsupported(StagedPlanReason::NoDeclaredOutputs)
    ));

    let first: NormalizedPath = temp.path().join("one/result.bin").into();
    let second: NormalizedPath = temp.path().join("two/result.bin").into();
    assert!(matches!(
        ExecStagedPlan::build(
            temp.path(),
            &[
                first.to_string_lossy().into_owned(),
                second.to_string_lossy().into_owned(),
            ],
            &[first, second],
            temp.path(),
        ),
        StagedPlanOutcome::Unsupported(StagedPlanReason::OutputNameCollision)
    ));

    let output: NormalizedPath = temp.path().join("result.bin").into();
    assert!(matches!(
        ExecStagedPlan::build(
            temp.path(),
            &["--output=result.bin".into()],
            &[output],
            temp.path(),
        ),
        StagedPlanOutcome::Unsupported(StagedPlanReason::OutputNotInArguments)
    ));
}

#[test]
fn exact_exec_disabled_lane_has_stable_reason() {
    if std::env::var_os(crate::daemon::server::persist::STAGED_ARTIFACTS_ENV).is_some() {
        return;
    }
    let temp = tempdir().unwrap();
    let output: NormalizedPath = temp.path().join("result.bin").into();
    assert!(matches!(
        ExecStagedPlan::build(
            temp.path(),
            &[output.to_string_lossy().into_owned()],
            &[output],
            temp.path(),
        ),
        StagedPlanOutcome::Unsupported(StagedPlanReason::LaneDisabled)
    ));
}
