//! Staged link/archive publication and salvage tests.

use super::*;

#[cfg(unix)]
fn write_counting_archiver(dir: &Path) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;

    let tool = dir.join("ar");
    std::fs::write(
        &tool,
        r#"#!/bin/sh
count_file="$ARCHIVE_COUNT_FILE"
count=0
if [ -f "$count_file" ]; then count=$(cat "$count_file"); fi
echo $((count + 1)) > "$count_file"
shift
output="$1"
shift
: > "$output"
for input in "$@"; do cat "$input" >> "$output"; done
"#,
    )
    .unwrap();
    let mut permissions = std::fs::metadata(&tool).unwrap().permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&tool, permissions).unwrap();
    tool
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cold_archive_overlap_skips_warm_hits_and_discards_artifact_hits() {
    let temp = tempfile::tempdir().unwrap();
    let cache_dir: NormalizedPath = temp.path().join("cache").into();
    let server =
        DaemonServer::bind_with_cache_dir(&crate::ipc::unique_test_endpoint(), &cache_dir).unwrap();
    let tool = write_counting_archiver(temp.path());
    let input = temp.path().join("member.o");
    let output = temp.path().join("libmember.a");
    let count_file = temp.path().join("archive-count");
    std::fs::write(&input, b"object payload").unwrap();
    let args = vec![
        "rcsD".to_string(),
        output.to_string_lossy().into_owned(),
        input.to_string_lossy().into_owned(),
    ];
    let env = Some(vec![
        ("PATH".to_string(), std::env::var("PATH").unwrap()),
        (
            "ARCHIVE_COUNT_FILE".to_string(),
            count_file.to_string_lossy().into_owned(),
        ),
    ]);

    let first = handle_link_ephemeral(
        &server.state,
        std::process::id(),
        &tool,
        &args,
        temp.path(),
        env.clone(),
    )
    .await;
    assert!(matches!(
        first,
        Response::LinkResult {
            exit_code: 0,
            cached: false,
            ..
        }
    ));
    assert_eq!(std::fs::read(&output).unwrap(), b"object payload");
    assert_eq!(std::fs::read_to_string(&count_file).unwrap().trim(), "1");

    let warm = handle_link_ephemeral(
        &server.state,
        std::process::id(),
        &tool,
        &args,
        temp.path(),
        env.clone(),
    )
    .await;
    assert!(matches!(warm, Response::LinkResult { cached: true, .. }));
    assert_eq!(
        std::fs::read_to_string(&count_file).unwrap().trim(),
        "1",
        "a metadata-hot artifact hit must not launch the archiver"
    );

    server.state.cache_system.clear();
    std::fs::remove_file(&output).unwrap();
    let cold_metadata_hit = handle_link_ephemeral(
        &server.state,
        std::process::id(),
        &tool,
        &args,
        temp.path(),
        env,
    )
    .await;
    assert!(matches!(
        cold_metadata_hit,
        Response::LinkResult { cached: true, .. }
    ));
    assert_eq!(
        std::fs::read_to_string(&count_file).unwrap().trim(),
        "2",
        "cold metadata should overlap an isolated archiver invocation"
    );
    assert_eq!(
        std::fs::read(&output).unwrap(),
        b"object payload",
        "the artifact hit must restore the canonical cached payload"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn archive_fast_path_preserves_client_env_and_lineage() {
    let temp = tempfile::tempdir().unwrap();
    let lineage = crate::daemon::lineage::Lineage::current(Some(4242), None);
    let env = vec![
        ("PATH".to_string(), std::env::var("PATH").unwrap()),
        (
            "ARCHIVE_FAST_PATH_SENTINEL".to_string(),
            "present".to_string(),
        ),
        ("MAKEFLAGS".to_string(), "--jobserver-auth=3,4".to_string()),
    ];
    let args = vec![
        "-c".to_string(),
        "printf '%s|%s|%s' \"$ARCHIVE_FAST_PATH_SENTINEL\" \"$ZCCACHE_CLIENT_PID\" \"${MAKEFLAGS-unset}\"".to_string(),
    ];

    let response =
        run_archive_tool_passthrough(Path::new("sh"), &args, temp.path(), Some(env), &lineage)
            .await;

    match response {
        Response::LinkResult {
            exit_code,
            stdout,
            stderr,
            ..
        } => {
            assert_eq!(exit_code, 0, "{}", String::from_utf8_lossy(&stderr));
            assert_eq!(&*stdout, b"present|4242|unset");
        }
        other => panic!("expected LinkResult, got {other:?}"),
    }
}

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
            role: StagedOutputRole::Regular,
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
    let metadata = CachedArtifact::from_artifact_data(&artifact).meta.clone();
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
            role: StagedOutputRole::Regular,
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
    let metadata = CachedArtifact::from_artifact_data(&artifact).meta.clone();
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

#[tokio::test]
async fn failed_directory_publication_salvages_complete_tree_without_cache_entry() {
    let temp = tempfile::tempdir().unwrap();
    let cache_dir: NormalizedPath = temp.path().join("cache").into();
    let server =
        DaemonServer::bind_with_cache_dir(&crate::ipc::unique_test_endpoint(), &cache_dir).unwrap();
    let root = temp.path().join("private-directory-link");
    let staged: NormalizedPath = root.join("app.dSYM").into();
    std::fs::create_dir_all(staged.join("Contents/Resources/DWARF")).unwrap();
    std::fs::write(
        staged.join("Contents/Resources/DWARF/app"),
        b"complete debug tree",
    )
    .unwrap();
    let requested: NormalizedPath = temp.path().join("app.dSYM").into();
    let plan = StagedDirectoryPlan::for_test(root, requested.clone(), staged);
    let fault = StagedFaultGuard::arm(&server.state.artifact_dir, [StagedFaultPoint::IndexCommit]);
    let key = "c".repeat(64);

    cache_staged_directory_link(
        &server.state,
        &plan,
        &key,
        &Arc::new(Vec::new()),
        &Arc::new(Vec::new()),
    )
    .unwrap();

    assert!(!server.state.artifacts.contains_key(&key));
    assert_eq!(
        std::fs::read(requested.join("Contents/Resources/DWARF/app")).unwrap(),
        b"complete debug tree"
    );
    let staged = server.state.profiler.staged.snapshot();
    assert_eq!(staged.counters["publication_failure"], 1);
    assert_eq!(staged.counters["salvage_attempt"], 1);
    assert_eq!(staged.counters["salvage_success"], 1);
    fault.assert_all_consumed();
}
