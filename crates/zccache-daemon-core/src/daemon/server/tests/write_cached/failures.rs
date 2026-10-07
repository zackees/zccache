use super::*;

#[test]
fn multi_hit_materialization_failure_is_reported_to_the_handler() {
    let dir = tempfile::tempdir().unwrap();
    let mut targets = Vec::new();
    let mut blobs = Vec::new();
    let mut payloads = Vec::new();
    for index in 0..2 {
        let blob: NormalizedPath = dir.path().join(format!("blob-{index}.o")).into();
        let output: NormalizedPath = dir.path().join(format!("output-{index}.o")).into();
        std::fs::write(&blob, b"original").unwrap();
        std::fs::hard_link(&blob, &output).unwrap();
        register_hardlink(&blob, &output).unwrap();
        forget_blob_registration_for_restart_test(&blob);
        targets.push(output);
        blobs.push(blob.clone());
        payloads.push(CachedPayload::File(blob));
    }

    assert!(matches!(
        materialize_multi_hit(&targets, &payloads, MaterializationMode::Auto),
        Err(MaterializationFailure::CacheBlobMissing(_))
    ));
    assert!(
        blobs.iter().any(|blob| !blob.exists()),
        "at least the rejected blob must be evicted before the handler rebuilds"
    );
}

#[test]
fn multi_hit_destination_failure_is_not_cache_blob_loss() {
    let dir = tempfile::tempdir().unwrap();
    let blob: NormalizedPath = dir.path().join("blob.o").into();
    let blocked_parent = dir.path().join("not-a-directory");
    std::fs::write(&blob, b"cached").unwrap();
    write_authoritative_blob_digest(&blob).unwrap();
    std::fs::write(&blocked_parent, b"blocker").unwrap();
    let target: NormalizedPath = blocked_parent.join("output.o").into();

    let result = materialize_multi_hit(
        &[target],
        &[CachedPayload::File(blob.clone())],
        MaterializationMode::Auto,
    );

    assert!(matches!(
        result,
        Err(MaterializationFailure::DestinationWrite(_))
    ));
    assert!(
        blob.exists(),
        "destination failures must preserve the cache blob"
    );
}

#[tokio::test]
async fn destination_failure_survives_and_journals_concrete_reason() {
    let dir = tempfile::tempdir().unwrap();
    let blob: NormalizedPath = dir.path().join("artifacts").join("blob.o").into();
    std::fs::create_dir_all(blob.parent().unwrap()).unwrap();
    seed_persisted_blob(&blob, b"cached");
    let blocker = dir.path().join("not-a-directory");
    std::fs::write(&blocker, b"blocker").unwrap();
    let target: NormalizedPath = blocker.join("output.o").into();

    let (failure, reason, _, _) = capture_miss_reason(Box::pin(async {
        let failure = materialize_multi_hit(
            &[target],
            &[CachedPayload::File(blob.clone())],
            MaterializationMode::Auto,
        )
        .unwrap_err();
        report_materialization_failure(dir.path(), "artifact-key", "unit-test", &failure);
        failure
    }))
    .await;

    assert!(matches!(
        failure,
        MaterializationFailure::DestinationWrite(_)
    ));
    assert!(
        blob.exists(),
        "destination failure must preserve cache data"
    );
    assert_eq!(reason, Some(miss_reason::DESTINATION_WRITE_FAILED));
    let entry = JournalEntry::new(
        JournalContext {
            compiler: "clang".to_string(),
            args: vec!["-c".to_string(), "source.c".to_string()],
            cwd: dir.path().to_string_lossy().into_owned(),
            env: None,
            session_id: None,
        },
        "miss",
        0,
        1,
        reason,
    );
    assert_eq!(
        entry.miss_reason.as_deref(),
        Some(miss_reason::DESTINATION_WRITE_FAILED)
    );

    let lifecycle = std::fs::read_to_string(
        dir.path()
            .join("logs")
            .join(crate::core::lifecycle::LIVE_LOG_FILENAME),
    )
    .unwrap();
    let event = lifecycle
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .find(|row| row["event"] == crate::core::lifecycle::EVENT_DESTINATION_WRITE_FAILED)
        .expect("destination lifecycle event");
    assert_eq!(event["evicted"], false);
    assert_eq!(event["artifact_key"], "artifact-key");
    let report = crate::audit::audit_cache_root(
        dir.path(),
        crate::audit::LogAuditContext::Integration,
        &crate::audit::AuditOptions::default(),
    )
    .unwrap();
    assert!(report.passed(), "{}", report.format_human());
}

#[tokio::test]
async fn deleted_cache_blob_invalidates_with_no_artifact_reason() {
    let dir = tempfile::tempdir().unwrap();
    let missing: NormalizedPath = dir.path().join("gone.o").into();
    let target: NormalizedPath = dir.path().join("restored.o").into();

    let (failure, reason, _, _) = capture_miss_reason(Box::pin(async {
        let failure = materialize_multi_hit(
            &[target],
            &[CachedPayload::File(missing)],
            MaterializationMode::Auto,
        )
        .unwrap_err();
        report_materialization_failure(dir.path(), "artifact-key", "unit-test", &failure);
        failure
    }))
    .await;

    assert!(matches!(
        failure,
        MaterializationFailure::CacheBlobMissing(_)
    ));
    assert_eq!(reason, Some(miss_reason::NO_ARTIFACT_FOR_KEY));
    let entry = JournalEntry::new(
        JournalContext {
            compiler: "clang".to_string(),
            args: vec!["-c".to_string(), "source.c".to_string()],
            cwd: dir.path().to_string_lossy().into_owned(),
            env: None,
            session_id: None,
        },
        "miss",
        0,
        1,
        reason,
    );
    assert_eq!(
        entry.miss_reason.as_deref(),
        Some(miss_reason::NO_ARTIFACT_FOR_KEY)
    );
}

#[test]
fn failed_detach_keeps_hardlink_registered_and_readonly() {
    let dir = tempfile::tempdir().unwrap();
    let blob = dir.path().join("blob.rlib");
    let output = dir.path().join("output.rlib");
    std::fs::write(&blob, b"original").unwrap();
    std::fs::hard_link(&blob, &output).unwrap();
    register_hardlink(&blob, &output).unwrap();
    crate::platform::fs::permissions::set_readonly(&blob, true).unwrap();
    fail_detach_remove_for_test(&output);

    let error = break_output_hardlink_before_compile(&output)
        .expect_err("injected remove failure must propagate");

    assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
    assert!(crate::platform::fs::identity::same_file(&blob, &output).unwrap());
    assert_eq!(registered_output_count(&blob), 1);
    assert!(crate::platform::fs::permissions::is_sealed(&blob).unwrap());
    crate::platform::fs::permissions::make_writable(&blob).unwrap();
}

#[test]
fn failed_detach_rename_restores_blob_readonly_after_unlink() {
    let dir = tempfile::tempdir().unwrap();
    let blob = dir.path().join("blob.rlib");
    let output = dir.path().join("output.rlib");
    std::fs::write(&blob, b"original").unwrap();
    std::fs::hard_link(&blob, &output).unwrap();
    register_hardlink(&blob, &output).unwrap();
    crate::platform::fs::permissions::set_readonly(&blob, true).unwrap();
    fail_detach_rename_for_test(&output);

    let error = break_output_hardlink_before_compile(&output)
        .expect_err("injected rename failure must propagate");

    assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
    assert!(!output.exists(), "shared output name was already unlinked");
    assert!(blob.exists());
    assert_eq!(
        crate::platform::fs::links::hard_link_count(&blob).unwrap(),
        1
    );
    assert_eq!(registered_output_count(&blob), 0);
    assert!(crate::platform::fs::permissions::is_sealed(&blob).unwrap());
    crate::platform::fs::permissions::make_writable(&blob).unwrap();
}

#[test]
fn failed_blob_removal_restores_readonly_and_registration() {
    let dir = tempfile::tempdir().unwrap();
    let blob = dir.path().join("blob.rlib");
    let output = dir.path().join("output.rlib");
    std::fs::write(&blob, b"original").unwrap();
    std::fs::hard_link(&blob, &output).unwrap();
    register_hardlink(&blob, &output).unwrap();
    crate::platform::fs::permissions::set_readonly(&blob, true).unwrap();
    fail_detach_remove_for_test(&blob);

    let error = remove_registered_blob(&blob).expect_err("injected removal must fail");

    assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
    assert!(crate::platform::fs::identity::same_file(&blob, &output).unwrap());
    assert_eq!(registered_output_count(&blob), 1);
    assert!(crate::platform::fs::permissions::is_sealed(&blob).unwrap());
    crate::platform::fs::permissions::make_writable(&blob).unwrap();
}

#[test]
fn failed_corrupt_blob_eviction_retains_suspect_record_for_retry() {
    let dir = tempfile::tempdir().unwrap();
    let blob = dir.path().join("blob.rlib");
    let output = dir.path().join("output.rlib");
    std::fs::write(&blob, b"original").unwrap();
    std::fs::hard_link(&blob, &output).unwrap();
    register_hardlink(&blob, &output).unwrap();
    crate::platform::fs::permissions::make_writable(&output).unwrap();
    std::fs::write(&output, b"poisoned").unwrap();
    mark_registered_links_suspect([output.as_path()]);
    fail_detach_remove_for_test(&blob);

    let first = verify_registered_blob(&blob).expect_err("injected eviction must fail");
    assert_eq!(first.kind(), std::io::ErrorKind::PermissionDenied);
    assert!(blob.exists());
    assert!(crate::platform::fs::permissions::is_sealed(&blob).unwrap());

    crate::platform::fs::permissions::make_writable(&output).unwrap();
    std::fs::remove_file(&output).unwrap();
    let retry = verify_registered_blob(&blob).expect_err("known corruption must remain suspect");
    assert_eq!(retry.kind(), std::io::ErrorKind::InvalidData);
    assert!(!blob.exists());
}

#[test]
fn replacing_registered_blob_drops_old_inode_record() {
    let dir = tempfile::tempdir().unwrap();
    let blob = dir.path().join("blob.rlib");
    let output = dir.path().join("output.rlib");
    let tmp = dir.path().join("replacement.tmp");
    std::fs::write(&blob, b"original").unwrap();
    std::fs::hard_link(&blob, &output).unwrap();
    register_hardlink(&blob, &output).unwrap();
    let old_id = crate::platform::fs::identity::file_identity(&blob).unwrap();
    std::fs::write(&tmp, b"replacement").unwrap();

    replace_artifact_cache_file(&tmp, &blob).unwrap();

    assert!(!is_file_id_registered(old_id));
    assert_eq!(std::fs::read(&blob).unwrap(), b"replacement");
    assert_eq!(std::fs::read(&output).unwrap(), b"original");
}
