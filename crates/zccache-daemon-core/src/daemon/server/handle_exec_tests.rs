//! Key-composition and exact-exec staging planner tests.
use super::*;
use tempfile::tempdir;

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

fn exec_artifact(output: &Path, bytes: &[u8]) -> ArtifactData {
    ArtifactData {
        outputs: vec![ArtifactOutput {
            name: output.to_string_lossy().into_owned(),
            payload: ArtifactPayload::Bytes(Arc::new(bytes.to_vec())),
        }],
        stdout: Arc::new(Vec::new()),
        stderr: Arc::new(Vec::new()),
        exit_code: 0,
    }
}

#[cfg(unix)]
fn write_exact_exec_tool(directory: &Path) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;

    let tool = directory.join("exact-exec-tool");
    std::fs::write(
        &tool,
        r#"#!/bin/sh
printf 'tick\n' >> "$(dirname "$0")/exec-ticks.txt"
printf 'exact-exec\n' > "$1"
"#,
    )
    .unwrap();
    let mut permissions = std::fs::metadata(&tool).unwrap().permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&tool, permissions).unwrap();
    tool
}

#[cfg(windows)]
fn write_exact_exec_tool(directory: &Path) -> PathBuf {
    let tool = directory.join("exact-exec-tool.cmd");
    std::fs::write(
        &tool,
        r#"@echo off
>> "%~dp0exec-ticks.txt" echo tick
> "%~1" echo exact-exec
exit /b 0
"#,
    )
    .unwrap();
    tool
}

#[tokio::test]
async fn staged_exec_store_publishes_only_a_v2_generation() {
    if !exec_staging_enabled() {
        return;
    }
    let temp = tempdir().unwrap();
    let cache: NormalizedPath = temp.path().join("cache").into();
    let server =
        DaemonServer::bind_with_cache_dir(&crate::ipc::unique_test_endpoint(), &cache).unwrap();
    let output = temp.path().join("staged.bin");
    std::fs::write(&output, b"exact-exec-v2").unwrap();
    let key = "ab".repeat(32);
    let source: NormalizedPath = output.clone().into();

    let failure = store_exec_artifact(
        &server.state,
        key.clone(),
        exec_artifact(&output, b"exact-exec-v2"),
        Some(std::slice::from_ref(&source)),
    )
    .await;

    assert_eq!(failure, None);
    assert!(server.state.artifacts.contains_key(&key));
    assert!(
        server
            .state
            .artifact_dir
            .join(".staged-v2")
            .join(format!("{key}.current"))
            .is_file(),
        "exact exec did not publish a v2 visibility pointer"
    );
    assert!(
        !server.state.artifact_dir.join(format!("{key}_0")).exists(),
        "exact exec unexpectedly persisted a legacy flat payload"
    );
}

#[tokio::test]
async fn staged_exec_publication_failure_is_counted_and_not_cached() {
    if !exec_staging_enabled() {
        return;
    }
    let temp = tempdir().unwrap();
    let cache: NormalizedPath = temp.path().join("cache").into();
    let server =
        DaemonServer::bind_with_cache_dir(&crate::ipc::unique_test_endpoint(), &cache).unwrap();
    let output = temp.path().join("failed.bin");
    std::fs::write(&output, b"publication-failure").unwrap();
    let key = "cd".repeat(32);
    let source: NormalizedPath = output.clone().into();
    let fault = StagedFaultGuard::arm(
        &server.state.artifact_dir,
        [StagedFaultPoint::PointerCommit],
    );

    let failure = store_exec_artifact(
        &server.state,
        key.clone(),
        exec_artifact(&output, b"publication-failure"),
        Some(std::slice::from_ref(&source)),
    )
    .await;

    assert_eq!(failure, Some(StagedPublishFailure::PointerCommit));
    assert!(!server.state.artifacts.contains_key(&key));
    let staged = server.state.profiler.staged.snapshot();
    assert_eq!(staged.counters["publication_failure"], 1);
    assert_eq!(staged.failures["pointer_commit"], 1);
    fault.assert_all_consumed();
}

#[tokio::test]
async fn staged_exact_exec_handler_salvages_publication_failure_then_hits_v2() {
    if !exec_staging_enabled() {
        return;
    }
    let temp = tempdir().unwrap();
    let cache: NormalizedPath = temp.path().join("cache").into();
    let server =
        DaemonServer::bind_with_cache_dir(&crate::ipc::unique_test_endpoint(), &cache).unwrap();
    let tool = write_exact_exec_tool(temp.path());
    let output: NormalizedPath = temp.path().join("result.bin").into();
    let args = vec![output.to_string_lossy().into_owned()];
    let execute = || {
        handle_generic_tool_exec(
            &server.state,
            &tool,
            &args,
            temp.path(),
            Vec::new(),
            &[],
            Arc::new(Vec::new()),
            ExecOutputStreams::default(),
            std::slice::from_ref(&output),
            None,
            ExecCachePolicy::Normal,
            true,
            &[],
            &[],
            &[],
            &[],
            None,
            false,
            &[],
        )
    };
    let fault = StagedFaultGuard::arm(
        &server.state.artifact_dir,
        [StagedFaultPoint::PointerCommit],
    );

    let first = execute().await;
    assert!(matches!(
        first,
        Response::GenericToolExecResult {
            exit_code: 0,
            cached: false,
            ..
        }
    ));
    assert!(
        output.is_file(),
        "publication failure did not salvage output"
    );
    fault.assert_all_consumed();

    std::fs::remove_file(output.as_path()).unwrap();
    let second = execute().await;
    assert!(matches!(
        second,
        Response::GenericToolExecResult {
            exit_code: 0,
            cached: false,
            ..
        }
    ));

    std::fs::remove_file(output.as_path()).unwrap();
    let third = execute().await;
    assert!(matches!(
        third,
        Response::GenericToolExecResult {
            exit_code: 0,
            cached: true,
            ..
        }
    ));
    assert_eq!(
        std::fs::read_to_string(temp.path().join("exec-ticks.txt"))
            .unwrap()
            .lines()
            .count(),
        2,
        "cache hit unexpectedly reran the exact exec tool"
    );
}
