use super::*;

/// Regression test for issue #1042 finding #1: fix #1 (writing the digest
/// sidecar *before* the publishing rename, not after) is what actually
/// closes the "valid blob evicted on restart" gap for freshly-persisted
/// blobs going forward — this is exercised by
/// persist_artifact_output_writes_digest_before_publishing and
/// persist_artifact_file_writes_digest_before_publishing above.
///
/// An earlier version of this fix additionally tried to trust any
/// digest-less blob with a hardlink count <= 1 (reasoning: no *current*
/// alias, so nothing could have poisoned it). That reasoning is unsound: a
/// since-deleted alias could have mutated the shared inode before being
/// removed, and the blob's current bytes would already reflect that
/// poisoning — link count only reflects the present, not whether a risky
/// window existed in the past. This is exactly what
/// failed_restart_eviction_restores_readonly_and_retries_after_alias_delete
/// (below) already covers, so a digest-less blob is always evicted on
/// verification; the remaining cost is a one-time cache miss for blobs
/// written by a pre-#1039 zccache version on upgrade, which is the safer
/// trade-off.
#[test]
fn verify_registered_blob_evicts_undigested_blob_even_when_singly_linked() {
    let dir = tempfile::tempdir().unwrap();
    let cache = dir.path().join("legacy.rlib");
    let content = b"blob written without a digest sidecar";
    // Deliberately skip seed_persisted_blob's digest write.
    std::fs::write(&cache, content).unwrap();

    let error = verify_registered_blob(&cache)
        .expect_err("an undigested, unregistered blob must be evicted, even singly-linked");
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    assert!(!cache.exists(), "unverifiable blob must be evicted");
}

#[test]
fn legacy_digest_migration_preserves_blob_and_is_idempotent() {
    let dir = tempfile::tempdir().unwrap();
    let cache = dir.path().join("legacy.rlib");
    let content = b"blob written by a legacy zccache version";
    std::fs::write(&cache, content).unwrap();

    // Migrate the legacy digest-less blob, then repeat the migration to prove
    // that an already-migrated blob is left unchanged.
    assert_eq!(migrate_legacy_blob_digests(dir.path()).unwrap(), 1);
    let migrated_content = std::fs::read(&cache).unwrap();
    assert_eq!(migrate_legacy_blob_digests(dir.path()).unwrap(), 0);
    assert_eq!(std::fs::read(&cache).unwrap(), migrated_content);

    // Simulate a daemon restart: durable verification must retain the blob.
    forget_blob_registration_for_restart_test(&cache);
    verify_registered_blob(&cache).expect("migrated legacy blob must survive restart verification");
    assert_eq!(std::fs::read(&cache).unwrap(), content);

    // Verify the durable sidecar path again after forgetting the rebuilt
    // in-memory record; migration and restart verification must be repeatable.
    forget_blob_registration_for_restart_test(&cache);
    verify_registered_blob(&cache).unwrap();
    assert_eq!(std::fs::read(&cache).unwrap(), content);
}

/// Regression test for issue #1042 finding #3: a failed identity resolution
/// on a fresh hardlink registration (the output was never actually
/// created — standing in for get_file_id() failing transiently right after
/// a real std::fs::hard_link succeeded) must not mark the shared blob
/// suspect. Marking it suspect on an inconclusive check would force an
/// unnecessary re-hash — and risk of false eviction — for every other
/// legitimate hardlink alias to that same blob.
#[test]
fn commit_hardlink_registration_does_not_poison_blob_on_unresolvable_identity() {
    let dir = tempfile::tempdir().unwrap();
    let cache = dir.path().join("cached.rlib");
    let out = dir.path().join("output.rlib");
    let content = b"shared cache bytes";
    seed_persisted_blob(&cache, content);

    let id = prepare_hardlink_registration(&cache, &out).unwrap();
    // `out` was never actually created at this path.
    let commit_result = commit_hardlink_registration(id, &out);
    assert!(
        commit_result.is_err(),
        "commit must fail when the output identity can't be resolved"
    );
    assert!(
        !is_blob_suspect_for_test(&cache),
        "an unresolvable output identity is not evidence of corruption and must not \
         mark the shared blob suspect"
    );
}

/// Regression test for issue #197: a cache hit hardlinks the target
/// output to the shared artifact file. Before a later cache miss invokes
/// the compiler for that same target path, zccache must detach the output
/// from the shared cache file so an in-place compiler overwrite cannot
/// mutate the cache artifact used by sibling worktrees.
#[test]
fn break_output_hardlink_before_compile_prevents_cache_poisoning() {
    let dir = tempfile::tempdir().unwrap();
    let cache = dir.path().join("cached.rlib");
    let out = dir.path().join("libapp.rlib");

    let cached_content = b"cached artifact from worktree a";
    let rebuilt_content = b"rebuilt artifact in worktree b";
    seed_persisted_blob(&cache, cached_content);

    deliver_linked(&out, &cache);
    // See the comment in write_cached_output_skips_when_already_hardlinked:
    // this must hold in every CI environment this suite runs in. This is
    // the issue #197 regression test — asserting it loudly instead of
    // silently branching restores the deterministic coverage that commit
    // 49dd59c's runtime-conditional rewrite dropped (issue #1042).
    if !require_hardlink(
        &out,
        &cache,
        "break_output_hardlink_before_compile_prevents_cache_poisoning",
    ) {
        return;
    }

    break_output_hardlink_before_compile(&out).unwrap();
    assert!(
        !crate::platform::fs::identity::same_file(&out, &cache).unwrap(),
        "break_output_hardlink_before_compile must detach the output from the shared cache blob"
    );

    std::fs::write(&out, rebuilt_content).unwrap();

    assert_eq!(
        std::fs::read(&cache).unwrap(),
        cached_content,
        "compiler overwrite of output must not mutate shared cache artifact"
    );
    assert_eq!(std::fs::read(&out).unwrap(), rebuilt_content);
}

/// RED characterization for #1039: an unmediated writer must never be able to
/// silently change the shared store blob through a hardlinked output.
#[test]
fn unmediated_mutation_cannot_silently_poison_cache() {
    let dir = tempfile::tempdir().unwrap();
    let cache = dir.path().join("cached.rlib");
    let out = dir.path().join("libapp.rlib");
    let original = b"trusted cache bytes";
    seed_persisted_blob(&cache, original);

    deliver_linked(&out, &cache);
    if crate::platform::fs::identity::same_file(&out, &cache).unwrap() {
        let mutation = std::fs::OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(&out)
            .and_then(|mut file| std::io::Write::write_all(&mut file, b"poison"));
        if mutation.is_ok() {
            // Privileged writers (notably root in containers) can bypass mode
            // bits. The watcher/registry safety net must detect and evict.
            mark_registered_links_suspect([out.as_path()]);
            let error = verify_registered_blob(&cache).expect_err("poison must be detected");
            assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
            assert!(!cache.exists(), "poisoned cache blob must be evicted");
            return;
        }
    } else {
        std::fs::write(&out, b"private mutation").unwrap();
    }
    assert_eq!(std::fs::read(&cache).unwrap(), original);
}

/// Cache blobs are immutable by default; mediated writers detach and clear the
/// attribute only on their private destination.
#[test]
fn persisted_blob_is_readonly_and_detach_is_writable() {
    let dir = tempfile::tempdir().unwrap();
    let cache = dir.path().join("cached.rlib");
    let out = dir.path().join("libapp.rlib");

    persist_artifact_output(&cache, b"immutable").unwrap();
    assert!(crate::platform::fs::permissions::is_sealed(&cache).unwrap());
    deliver_linked(&out, &cache);
    break_output_hardlink_before_compile(&out).unwrap();
    assert!(!std::fs::metadata(&out).unwrap().permissions().readonly());
    std::fs::write(&out, b"rebuilt").unwrap();
    assert_eq!(std::fs::read(&cache).unwrap(), b"immutable");
}

#[test]
fn capability_verdict_is_cached_and_registry_tracks_hardlinks() {
    let dir = tempfile::tempdir().unwrap();
    let cache = dir.path().join("cached.rlib");
    let out = dir.path().join("libapp.rlib");
    seed_persisted_blob(&cache, b"bytes");

    let first = fs_caps_raw(&cache, &out);
    let second = fs_caps_raw(&cache, &out);
    assert_eq!(first, second);
    materialize_cached_file_with_mode(
        &out,
        &cache,
        crate::compiler::DeliveryPolicy::HardlinkEligible,
        MaterializationMode::Link,
        false,
    )
    .unwrap();
    if crate::platform::fs::identity::same_file(&out, &cache).unwrap() {
        assert_eq!(registered_output_count(&cache), 1);
        assert_eq!(
            crate::platform::fs::links::hard_link_count(&cache).unwrap(),
            2
        );
    } else {
        assert!(first.reflink || !first.hardlink);
    }
}

#[test]
fn hardlink_ceiling_degrades_before_os_error() {
    let caps = VolumeCaps {
        reflink: false,
        hardlink: true,
        readonly_enforced: true,
        file_id: FileIdWidth::Bits128,
        hardlink_limit: 1023,
    };
    assert!(hardlink_below_limit(caps, 1022));
    assert!(!hardlink_below_limit(caps, 1023));
    assert!(!hardlink_below_limit(caps, 1024));
}

#[test]
fn suspect_corruption_emits_durable_forensics() {
    let dir = tempfile::tempdir().unwrap();
    let _cache_dir = crate::daemon::server::tests::CacheDirEnvGuard::set(dir.path());
    let blob = dir.path().join("blob.rlib");
    let output = dir.path().join("output.rlib");
    seed_persisted_blob(&blob, b"original");
    std::fs::hard_link(&blob, &output).unwrap();
    register_hardlink(&blob, &output).unwrap();
    crate::platform::fs::permissions::make_writable(&output).unwrap();
    std::fs::write(&output, b"poisoned").unwrap();
    mark_registered_links_suspect([output.as_path()]);

    let error = verify_registered_blob(&blob).expect_err("poisoned blob must be rejected");
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    let log = std::fs::read_to_string(crate::core::lifecycle::log_file_path()).unwrap();
    let event: serde_json::Value = log
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .find(|event: &serde_json::Value| {
            event["event"] == "cow_blob_corruption_detected"
                && event["blob_path"] == blob.to_string_lossy().as_ref()
        })
        .expect("matching corruption event");
    assert_eq!(event["event"], "cow_blob_corruption_detected");
    assert_eq!(event["cache_key"], "blob.rlib");
    assert!(event["expected_hash"].is_string());
    assert!(event["actual_hash"].is_string());
    assert_eq!(event["link_count"], 2);
    assert!(event["registered_outputs"].as_array().unwrap().len() == 1);
    assert!(event["elapsed_ns"].as_u64().is_some());
    assert!(!blob.exists(), "corrupt cache blob must be evicted");
}

#[test]
fn removed_link_event_marks_blob_suspect_before_forgetting_path() {
    let dir = tempfile::tempdir().unwrap();
    let blob = dir.path().join("blob.rlib");
    let output = dir.path().join("output.rlib");
    std::fs::write(&blob, b"original").unwrap();
    std::fs::hard_link(&blob, &output).unwrap();
    register_hardlink(&blob, &output).unwrap();
    crate::platform::fs::permissions::make_writable(&output).unwrap();
    std::fs::write(&output, b"poisoned").unwrap();
    std::fs::remove_file(&output).unwrap();

    mark_removed_links_suspect([output.as_path()]);

    let error = verify_registered_blob(&blob).expect_err("removed poison must be rejected");
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    assert!(!blob.exists());
}

#[test]
fn watcher_overflow_marks_every_blob_suspect() {
    let dir = tempfile::tempdir().unwrap();
    let blob = dir.path().join("blob.rlib");
    let output = dir.path().join("output.rlib");
    std::fs::write(&blob, b"original").unwrap();
    std::fs::hard_link(&blob, &output).unwrap();
    register_hardlink(&blob, &output).unwrap();
    crate::platform::fs::permissions::make_writable(&output).unwrap();
    std::fs::write(&output, b"poisoned").unwrap();

    mark_all_registered_links_suspect();

    let error = verify_registered_blob(&blob).expect_err("overflow poison must be rejected");
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    assert!(!blob.exists());
}

#[test]
fn watcher_event_between_hardlink_publish_and_registry_commit_is_retained() {
    let dir = tempfile::tempdir().unwrap();
    let blob = dir.path().join("blob.rlib");
    let output = dir.path().join("output.rlib");
    std::fs::write(&blob, b"original").unwrap();
    let registration = prepare_hardlink_registration(&blob, &output).unwrap();
    std::fs::hard_link(&blob, &output).unwrap();
    crate::platform::fs::permissions::make_writable(&output).unwrap();
    std::fs::write(&output, b"poisoned during publish").unwrap();
    std::fs::remove_file(&output).unwrap();

    mark_removed_links_suspect([output.as_path()]);
    let commit = commit_hardlink_registration(registration, &output);
    assert_eq!(commit.unwrap_err().kind(), std::io::ErrorKind::NotFound);

    let error = verify_registered_blob(&blob).expect_err("publish race must be detected");
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    assert!(!blob.exists());
}

#[test]
fn daemon_restart_fails_closed_for_unregistered_multilink_blob() {
    let dir = tempfile::tempdir().unwrap();
    let blob = dir.path().join("blob.rlib");
    let output = dir.path().join("output.rlib");
    std::fs::write(&blob, b"original").unwrap();
    std::fs::hard_link(&blob, &output).unwrap();
    register_hardlink(&blob, &output).unwrap();
    forget_blob_registration_for_restart_test(&blob);
    crate::platform::fs::permissions::make_writable(&output).unwrap();
    std::fs::write(&output, b"poisoned while daemon was down").unwrap();

    let error = verify_registered_blob(&blob).expect_err("unknown shared blob must be evicted");
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    assert!(!blob.exists());
    assert!(
        output.exists(),
        "workspace output must survive cache eviction"
    );
}

#[test]
fn restart_digest_detects_poison_after_alias_was_deleted() {
    let dir = tempfile::tempdir().unwrap();
    let blob = dir.path().join("blob.rlib");
    let output = dir.path().join("output.rlib");
    std::fs::write(&blob, b"original").unwrap();
    write_authoritative_blob_digest(&blob).unwrap();
    std::fs::hard_link(&blob, &output).unwrap();
    register_hardlink(&blob, &output).unwrap();
    crate::platform::fs::permissions::make_writable(&output).unwrap();
    std::fs::write(&output, b"poisoned while daemon was down").unwrap();
    std::fs::remove_file(&output).unwrap();
    forget_blob_registration_for_restart_test(&blob);

    let error = verify_registered_blob(&blob).expect_err("durable digest must reject poison");
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    assert!(!blob.exists());
}

#[test]
fn restart_digest_rebuilds_registry_for_clean_blob() {
    let dir = tempfile::tempdir().unwrap();
    let blob = dir.path().join("blob.rlib");
    std::fs::write(&blob, b"original").unwrap();
    write_authoritative_blob_digest(&blob).unwrap();

    verify_registered_blob(&blob).unwrap();

    assert!(registered_blob_id(&blob).is_some());
    assert_eq!(std::fs::read(blob).unwrap(), b"original");
}

#[test]
fn failed_restart_eviction_restores_readonly_and_retries_after_alias_delete() {
    let dir = tempfile::tempdir().unwrap();
    let blob = dir.path().join("blob.rlib");
    let output = dir.path().join("output.rlib");
    std::fs::write(&blob, b"unverifiable").unwrap();
    std::fs::hard_link(&blob, &output).unwrap();
    fail_detach_remove_for_test(&blob);

    let first = verify_registered_blob(&blob).expect_err("injected restart eviction must fail");
    assert_eq!(first.kind(), std::io::ErrorKind::PermissionDenied);
    assert!(crate::platform::fs::permissions::is_sealed(&blob).unwrap());

    crate::platform::fs::permissions::make_writable(&output).unwrap();
    std::fs::remove_file(&output).unwrap();
    let retry = verify_registered_blob(&blob).expect_err("unverifiable blob must retry eviction");
    assert_eq!(retry.kind(), std::io::ErrorKind::InvalidData);
    assert!(!blob.exists());
}
