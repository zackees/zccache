//! Staged link/archive publication and salvage tests.

use super::*;

#[tokio::test]
async fn failed_link_publication_salvages_output_without_becoming_cacheable() {
    let temp = tempfile::tempdir().unwrap();
    let cache_dir: NormalizedPath = temp.path().join("cache").into();
    let server =
        DaemonServer::bind_with_cache_dir(&crate::ipc::unique_test_endpoint(), &cache_dir).unwrap();
    let root = temp.path().join("private-link");
    std::fs::create_dir_all(&root).unwrap();
    let requested: NormalizedPath = temp.path().join("app.exe").into();
    let staged: NormalizedPath = root.join("app.exe").into();
    std::fs::write(&staged, b"complete linked image").unwrap();
    let plan = StagedCompilePlan::for_test(
        root,
        vec![StagedOutputPlan {
            requested: requested.clone(),
            staged: staged.clone(),
        }],
    );
    let artifact = ArtifactData {
        outputs: vec![ArtifactOutput {
            name: "app.exe".to_string(),
            payload: ArtifactPayload::Bytes(Arc::new(b"complete linked image".to_vec())),
        }],
        stdout: Arc::new(Vec::new()),
        stderr: Arc::new(Vec::new()),
        exit_code: 0,
    };
    let metadata = CachedArtifact::from_artifact_data(&artifact).meta;
    let fault = StagedFaultGuard::arm(&server.state.artifact_dir, [StagedFaultPoint::IndexCommit]);

    let key = "a".repeat(64);
    let cacheable =
        publish_and_materialize_staged_link(&server.state, &plan, &key, metadata, &[staged])
            .unwrap();
    assert!(!cacheable);
    assert!(!server.state.artifacts.contains_key(&key));
    assert_eq!(std::fs::read(&requested).unwrap(), b"complete linked image");
    let staged = server.state.profiler.staged.snapshot();
    assert_eq!(staged.counters["publication_failure"], 1);
    assert_eq!(staged.counters["salvage_attempt"], 1);
    assert_eq!(staged.counters["salvage_success"], 1);
    assert_eq!(staged.failures["index_commit"], 1);
    fault.assert_all_consumed();
}

#[tokio::test]
async fn failed_link_publication_and_salvage_fail_closed() {
    let temp = tempfile::tempdir().unwrap();
    let cache_dir: NormalizedPath = temp.path().join("cache").into();
    let server =
        DaemonServer::bind_with_cache_dir(&crate::ipc::unique_test_endpoint(), &cache_dir).unwrap();
    let root = temp.path().join("private-link");
    std::fs::create_dir_all(&root).unwrap();
    let requested: NormalizedPath = temp.path().join("app.exe").into();
    let staged: NormalizedPath = root.join("app.exe").into();
    std::fs::write(&staged, b"complete linked image").unwrap();
    let plan = StagedCompilePlan::for_test(
        root,
        vec![StagedOutputPlan {
            requested: requested.clone(),
            staged: staged.clone(),
        }],
    );
    let artifact = ArtifactData {
        outputs: vec![ArtifactOutput {
            name: "app.exe".to_string(),
            payload: ArtifactPayload::Bytes(Arc::new(b"complete linked image".to_vec())),
        }],
        stdout: Arc::new(Vec::new()),
        stderr: Arc::new(Vec::new()),
        exit_code: 0,
    };
    let metadata = CachedArtifact::from_artifact_data(&artifact).meta;
    let publish_fault = StagedFaultGuard::arm(
        &server.state.artifact_dir,
        [StagedFaultPoint::PointerCommit],
    );
    let salvage_fault = StagedFaultGuard::arm(&requested, [StagedFaultPoint::MaterializeOutput(0)]);

    publish_and_materialize_staged_link(&server.state, &plan, &"b".repeat(64), metadata, &[staged])
        .unwrap_err();
    assert!(!requested.exists());
    let staged = server.state.profiler.staged.snapshot();
    assert_eq!(staged.counters["salvage_failure"], 1);
    assert_eq!(staged.counters["materialize_failure"], 1);
    publish_fault.assert_all_consumed();
    salvage_fault.assert_all_consumed();
}
