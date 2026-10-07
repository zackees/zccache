use super::*;

/// zccache#1597: ordinary cache-file hits can restore a Cargo build-script
/// binary. Delivery must not open the output for writing while another daemon
/// child is between fork and exec, because that child inherits the descriptor
/// and Cargo can then receive `ETXTBSY` when it executes the hard-linked alias.
#[cfg(unix)]
#[test]
fn cache_hit_materialization_waits_for_child_spawn() {
    use std::sync::mpsc;
    use std::time::Duration;

    let dir = tempfile::tempdir().unwrap();
    let cache = dir.path().join("build_script_build-cache");
    let destination = dir.path().join("build-script-build");
    seed_persisted_blob(&cache, b"#!/bin/sh\nexit 0\n");

    // A semantic daemon child holds this guard from fork until exec. Keep it
    // held to make direct cache-hit materialization prove it uses the matching
    // exclusive side before opening the requested output for writing.
    let spawn_guard = crate::daemon::spawn_exclusion::spawn_shared();
    let (started_tx, started_rx) = mpsc::sync_channel(1);
    let (done_tx, done_rx) = mpsc::sync_channel(1);
    // The writer owns its path while the parent retains this one for the
    // post-join assertion.
    let destination_for_writer = destination.clone();
    let writer = std::thread::spawn(move || {
        started_tx.send(()).unwrap();
        let result = write_cached_file(&destination_for_writer, &cache);
        done_tx.send(result).unwrap();
    });
    started_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("cache-hit writer did not begin materialization");

    assert!(
        matches!(
            done_rx.recv_timeout(Duration::from_millis(100)),
            Err(mpsc::RecvTimeoutError::Timeout)
        ),
        "cache-hit materialization wrote while a child could inherit its descriptor"
    );
    drop(spawn_guard);

    done_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("cache-hit materialization did not resume after child exec")
        .unwrap();
    writer.join().unwrap();
    assert_eq!(std::fs::read(destination).unwrap(), b"#!/bin/sh\nexit 0\n");
}

/// A cache hit must still deliver the output when native identity inspection
/// cannot prepare a shared hardlink; the independent copy tier remains safe.
#[test]
fn cache_hit_copies_when_hardlink_registration_fails() {
    let dir = tempfile::tempdir().unwrap();
    let cache = dir.path().join("cached.rmeta");
    let destination = dir.path().join("requested.rmeta");
    seed_persisted_blob(&cache, b"immutable rust metadata");
    if !fs_caps_raw(&cache, &destination).hardlink {
        eprintln!("SKIP: test filesystem does not support hardlinks");
        return;
    }
    // #1792: only LINK hardlinks, and it never tries a clone first.
    let fault = StagedFaultGuard::arm(
        &destination,
        [StagedFaultPoint::MaterializeHardlinkRegistration],
    );
    let observed = materialize_cached_file_with_mode(
        &destination,
        &cache,
        crate::compiler::DeliveryPolicy::HardlinkEligible,
        MaterializationMode::Link,
        true,
    )
    .unwrap();
    fault.assert_all_consumed();
    assert_eq!(observed.copy_count, 1);
    assert_eq!(
        std::fs::read(&destination).unwrap(),
        b"immutable rust metadata"
    );
    assert!(!crate::platform::fs::identity::same_file(&destination, &cache).unwrap());
}

/// The native Windows file-identity probe can reject a valid long cache path
/// even when Rust's filesystem calls can read and copy the blob. Such a hit
/// must still deliver the current request's metadata output.
#[cfg(windows)]
#[test]
fn long_cache_blob_path_delivers_requested_metadata_hit() {
    let dir = tempfile::tempdir().unwrap();
    let cache = dir
        .path()
        .join("a".repeat(110))
        .join("b".repeat(110))
        .join("cached.rmeta");
    std::fs::create_dir_all(cache.parent().unwrap()).unwrap();
    seed_persisted_blob(&cache, b"immutable rust metadata");
    assert!(cache.as_os_str().len() > 260);
    let destination = dir.path().join("requested.rmeta");
    assert!(
        fs_caps_raw(&cache, &destination).hardlink,
        "the fixture must exercise the hardlink-registration path"
    );
    let identity_available = crate::platform::fs::identity::file_identity(&cache).is_ok();
    let fault = StagedFaultGuard::arm(&destination, [StagedFaultPoint::MaterializeReflink]);
    let observed = write_cached_file_observed(&destination, &cache).unwrap();
    fault.assert_all_consumed();
    if !identity_available {
        assert_eq!(observed.copy_count, 1);
    }
    assert_eq!(observed.copy_count + observed.hardlink_count, 1);
    assert_eq!(
        std::fs::read(&destination).unwrap(),
        b"immutable rust metadata"
    );
}

// ── write_cached_output staleness tests ────────────────────────────

/// Regression test: write_cached_output must overwrite an existing output
/// file even when the existing file has the same size as the cached data.
///
/// This reproduces the linker staleness bug where a header change produces
/// a .o of the same size but different content — the old size-only check
/// skipped the write, leaving a stale .o on disk with missing symbols.
#[test]
fn write_cached_output_overwrites_same_size_different_content() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("output.o");
    let cache = dir.path().join("cached.o");

    // Simulate: output.o exists from a previous compilation (version A).
    let old_content = b"AAAA_symbols_v1_xxxx";
    std::fs::write(&out, old_content).unwrap();

    // Simulate: cache file has new content (version B) — same size, different bytes.
    let new_content = b"BBBB_symbols_v2_yyyy";
    assert_eq!(
        old_content.len(),
        new_content.len(),
        "test requires same size"
    );
    seed_persisted_blob(&cache, new_content);

    // write_cached_output must replace the stale output with the cached content.
    write_cached_output(&out, &cache, new_content).unwrap();

    let result = std::fs::read(&out).unwrap();
    assert_eq!(
        result, new_content,
        "output must contain new content, not stale old content"
    );
}

/// write_cached_output correctly creates the output when it doesn't exist.
#[test]
fn write_cached_output_creates_new_file() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("output.o");
    let cache = dir.path().join("cached.o");

    let content = b"fresh object file data";
    seed_persisted_blob(&cache, content);

    write_cached_output(&out, &cache, content).unwrap();

    let result = std::fs::read(&out).unwrap();
    assert_eq!(result, content.as_slice());
}

/// write_cached_output falls back to memory copy when cache file is missing.
#[test]
fn write_cached_output_fallback_to_memory_copy() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("output.o");
    let cache = dir.path().join("nonexistent_cache.o");

    let content = b"data from memory";

    write_cached_output(&out, &cache, content).unwrap();

    let result = std::fs::read(&out).unwrap();
    assert_eq!(result, content.as_slice());
}

/// write_cached_output skips the write when output is already a hardlink
/// to the cache file (same file identity). This is the fast path for
/// repeated cache hits with the same artifact key.
#[test]
fn write_cached_output_skips_when_already_hardlinked() {
    let dir = tempfile::tempdir().unwrap();
    let cache = dir.path().join("cached.o");
    let out = dir.path().join("output.o");

    let content = b"cached artifact content";
    seed_persisted_blob(&cache, content);

    // First write: creates hardlink
    deliver_linked(&out, &cache);
    assert_eq!(std::fs::read(&out).unwrap(), content.as_slice());

    // A plain same-volume tempdir supports hardlinks on every CI platform
    // (Windows/Linux/macOS) this suite runs on. Assert that precondition
    // loudly instead of silently branching on it — a silent branch let this
    // test pass without ever exercising the hardlink-skip path it's named
    // for (issue #1042 test-coverage regression).
    if !require_hardlink(
        &out,
        &cache,
        "write_cached_output_skips_when_already_hardlinked",
    ) {
        return;
    }

    // Second write: should detect hardlink and skip.
    // (If it didn't skip, it would still produce correct content,
    //  but the test verifies the optimization path exists.)
    deliver_linked(&out, &cache);
    assert_eq!(std::fs::read(&out).unwrap(), content.as_slice());
    assert!(
        crate::platform::fs::identity::same_file(&out, &cache).unwrap(),
        "output must remain hardlinked to cache after the skip fast path"
    );
}

#[test]
fn persist_artifact_output_does_not_mutate_existing_hardlink() {
    let dir = tempfile::tempdir().unwrap();
    let cache = dir.path().join("artifact-key_0");
    let out = dir.path().join("output.rlib");

    persist_artifact_output(&cache, b"first").unwrap();
    deliver_linked(&out, &cache);
    // See the comment in write_cached_output_skips_when_already_hardlinked:
    // this must hold in every CI environment this suite runs in, so assert
    // it loudly rather than silently skip the invariant this test exists to
    // check (issue #1042 test-coverage regression).
    if !require_hardlink(
        &out,
        &cache,
        "persist_artifact_output_does_not_mutate_existing_hardlink",
    ) {
        return;
    }

    persist_artifact_output(&cache, b"second").unwrap();

    assert_eq!(
        std::fs::read(&out).unwrap(),
        b"first",
        "publishing a later cache payload must not mutate existing target outputs"
    );
    assert_eq!(std::fs::read(&cache).unwrap(), b"second");
    assert!(
        !crate::platform::fs::identity::same_file(&out, &cache).unwrap(),
        "publishing a new cache payload must detach any existing hardlinked output"
    );
}

#[test]
fn persist_artifact_file_creates_independent_immutable_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("libunit.rlib");
    let cache = dir.path().join("artifact-key_0");
    let content = b"compiled rust artifact";
    std::fs::write(&source, content).unwrap();

    let stats = persist_artifact_file(&cache, &source, MaterializationMode::Auto).unwrap();

    assert_eq!(std::fs::read(&cache).unwrap(), content);
    assert_eq!(
        stats.reflink_count + stats.copy_count + stats.hardlink_count,
        1
    );
    if stats.hardlink_count == 1 {
        // Issue #1042 finding #4: the hardlink tier legitimately shares an
        // inode with `source` by design (that's the whole point of the
        // fast path). The "immutable snapshot" guarantee for a hardlinked
        // source comes not from file independence but from
        // break_output_hardlink_before_compile, which unconditionally
        // detaches any shared alias (based on OS-level link count) before
        // a compiler is ever allowed to write to `source` again.
        assert!(crate::platform::fs::identity::same_file(&source, &cache).unwrap());
    } else {
        assert!(crate::platform::fs::permissions::is_sealed(&cache).unwrap());
        assert!(!crate::platform::fs::identity::same_file(&source, &cache).unwrap());
    }
}

/// Regression test for issue #1042 finding #4/#5: persist_artifact_file
/// must attempt a hardlink before falling back to a full byte copy on
/// non-reflink filesystems. Commit 49dd59c replaced the pre-existing
/// hardlink-first strategy with reflink-then-copy and dropped the hardlink
/// attempt entirely; a plain same-volume tempdir doesn't support reflink
/// (that requires a COW-capable filesystem like btrfs/APFS/ReFS), so this
/// exercises exactly the regressed path on Windows/Linux CI.
#[test]
fn persist_artifact_file_uses_hardlink_when_reflink_unavailable() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("libunit.rlib");
    let cache = dir.path().join("artifact-key_0");
    let content = b"compiled rust artifact for hardlink fast path";
    std::fs::write(&source, content).unwrap();

    // #1792: LINK is the mode that stores by hardlink; AUTO never does.
    let stats = persist_artifact_file(&cache, &source, MaterializationMode::Link).unwrap();

    assert_eq!(std::fs::read(&cache).unwrap(), content);
    if stats.reflink_count == 0 {
        // This tempdir doesn't support reflink (the common case for a
        // plain NTFS/ext4 tempdir) — the hardlink tier must have been
        // taken instead of falling all the way through to a full copy.
        assert_eq!(
            stats.hardlink_count, 1,
            "persist_artifact_file must use the hardlink fast path when reflink is \
             unavailable, instead of always falling through to a full copy"
        );
        assert_eq!(stats.copy_count, 0);
    }
}

/// Regression test for issue #1042 finding #1: the digest sidecar for a
/// freshly-persisted blob must be written and named for the *final*
/// cache_path before the blob becomes visible, so a process restart can
/// always durably re-verify it instead of evicting it as unregistered.
#[test]
fn persist_artifact_output_writes_digest_before_publishing() {
    let dir = tempfile::tempdir().unwrap();
    let cache = dir.path().join("artifact-key_0");
    let content = b"freshly persisted artifact";

    persist_artifact_output(&cache, content).unwrap();
    assert!(cache.exists());

    // Simulate a daemon restart: the in-memory registry is gone, so
    // verification must fall back to the durable digest sidecar.
    forget_blob_registration_for_restart_test(&cache);
    verify_registered_blob(&cache).expect(
        "a freshly-persisted blob must have a durable digest sidecar keyed to its \
         final name, surviving a restart without being evicted",
    );
    assert!(cache.exists(), "blob must not have been evicted");
}

/// Same as above, for the persist_artifact_file (hardlink/reflink/copy) path.
#[test]
fn persist_artifact_file_writes_digest_before_publishing() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("libunit.rlib");
    let cache = dir.path().join("artifact-key_0");
    let content = b"compiled rust artifact";
    std::fs::write(&source, content).unwrap();

    persist_artifact_file(&cache, &source, MaterializationMode::Auto).unwrap();
    assert!(cache.exists());

    forget_blob_registration_for_restart_test(&cache);
    verify_registered_blob(&cache).expect(
        "a freshly-persisted blob must have a durable digest sidecar keyed to its \
         final name, surviving a restart without being evicted",
    );
    assert!(cache.exists(), "blob must not have been evicted");
}

#[test]
fn staged_generation_materializes_independently_from_the_backend() {
    let dir = tempfile::tempdir().unwrap();
    let artifact_dir = dir.path().join("artifacts");
    std::fs::create_dir_all(&artifact_dir).unwrap();
    let source = dir.path().join("source.rlib");
    let output = dir.path().join("target.rlib");
    std::fs::write(&source, b"staged immutable artifact").unwrap();

    persist_staged_artifact_paths(&artifact_dir, &"f".repeat(64), &[source.clone().into()])
        .unwrap();
    let payloads = load_staged_artifact_paths(&artifact_dir, &"f".repeat(64), &[25])
        .unwrap()
        .unwrap();
    write_cached_file(&output, &payloads[0]).unwrap();

    assert!(!crate::platform::fs::identity::same_file(&output, &payloads[0]).unwrap());
    assert!(!std::fs::metadata(&output).unwrap().permissions().readonly());
    std::fs::write(&output, b"mutated target output").unwrap();
    assert_eq!(
        std::fs::read(&payloads[0]).unwrap(),
        b"staged immutable artifact"
    );
}

#[test]
fn staged_generation_hardlinks_only_when_semantically_authorized() {
    let dir = tempfile::tempdir().unwrap();
    let artifact_dir = dir.path().join("artifacts");
    std::fs::create_dir_all(&artifact_dir).unwrap();
    let source = dir.path().join("source.rlib");
    let output = dir.path().join("target.rlib");
    std::fs::write(&source, b"semantic rust archive").unwrap();
    let key = "e".repeat(64);
    persist_staged_artifact_paths(&artifact_dir, &key, &[source.into()]).unwrap();
    let payloads = load_staged_artifact_paths(&artifact_dir, &key, &[21])
        .unwrap()
        .unwrap();
    let payload = CachedPayload::File(payloads[0].clone());
    let targets = vec![&output];
    let observed = write_payloads_par_with_mtime_floor_and_policies_observed(
        &targets,
        &[payload],
        &Vec::<NormalizedPath>::new(),
        &[crate::compiler::DeliveryPolicy::HardlinkEligible],
        MaterializationMode::Link,
    )
    .unwrap();
    assert_eq!(
        observed.reflink_count + observed.hardlink_count + observed.copy_count,
        1,
        "a staged file restore reports exactly one successful tier"
    );
    if !require_hardlink(
        &output,
        &payloads[0],
        "staged_generation_hardlinks_only_when_semantically_authorized",
    ) {
        return;
    }

    crate::platform::fs::permissions::make_writable(&output).unwrap();
    std::fs::write(&output, b"poisoned rust archive").unwrap();
    mark_registered_links_suspect([output.as_path()]);
    assert!(
        verify_registered_blob(&payloads[0]).is_err(),
        "a hardlink contract violation must be rejected before serve"
    );
}

#[test]
fn staged_hit_tier_faults_fall_through_without_misattribution() {
    let dir = tempfile::tempdir().unwrap();
    let artifact_dir = dir.path().join("artifacts");
    std::fs::create_dir_all(&artifact_dir).unwrap();
    let source = dir.path().join("source.rlib");
    std::fs::write(&source, b"tier fault payload").unwrap();
    let key = "b".repeat(64);
    persist_staged_artifact_paths(&artifact_dir, &key, &[source.into()]).unwrap();
    let payloads = load_staged_artifact_paths(&artifact_dir, &key, &[18])
        .unwrap()
        .unwrap();
    let output = dir.path().join("target.rlib");
    if !fs_caps_raw(&payloads[0], &output).hardlink {
        eprintln!("SKIP staged_hit_tier_faults: fixture has no hardlink capability");
        return;
    }
    // #1792: LINK is the only mode with a hardlink tier to fault.
    let faults = StagedFaultGuard::arm(&output, [StagedFaultPoint::MaterializeHardlink]);
    let payload = CachedPayload::File(payloads[0].clone());
    let targets = vec![&output];
    let observed = write_payloads_par_with_mtime_floor_and_policies_observed(
        &targets,
        &[payload],
        &Vec::<NormalizedPath>::new(),
        &[crate::compiler::DeliveryPolicy::HardlinkEligible],
        MaterializationMode::Link,
    )
    .unwrap();
    assert_eq!(observed.reflink_count, 0);
    assert_eq!(observed.hardlink_count, 0);
    assert_eq!(observed.copy_count, 1);
    assert_eq!(observed.copy_bytes, 18);
    faults.assert_all_consumed();
}

#[test]
fn staged_hit_copy_fault_leaves_no_partial_destination() {
    let dir = tempfile::tempdir().unwrap();
    let artifact_dir = dir.path().join("artifacts");
    std::fs::create_dir_all(&artifact_dir).unwrap();
    let source = dir.path().join("source.rmeta");
    std::fs::write(&source, b"copy failure payload").unwrap();
    let key = "c".repeat(64);
    persist_staged_artifact_paths(&artifact_dir, &key, &[source.into()]).unwrap();
    let payload = load_staged_artifact_paths(&artifact_dir, &key, &[20])
        .unwrap()
        .unwrap()
        .remove(0);
    let output = dir.path().join("target.rmeta");
    let faults = StagedFaultGuard::arm(
        &output,
        [
            StagedFaultPoint::MaterializeReflink,
            StagedFaultPoint::MaterializeCopy,
        ],
    );
    write_cached_file(&output, &payload).unwrap_err();
    assert!(!output.exists());
    faults.assert_all_consumed();
}

#[test]
fn staged_generation_digest_survives_registry_restart() {
    let dir = tempfile::tempdir().unwrap();
    let artifact_dir = dir.path().join("artifacts");
    std::fs::create_dir_all(&artifact_dir).unwrap();
    let source = dir.path().join("source.rmeta");
    std::fs::write(&source, b"semantic rust metadata").unwrap();
    let key = "d".repeat(64);
    persist_staged_artifact_paths(&artifact_dir, &key, &[source.into()]).unwrap();
    let payloads = load_staged_artifact_paths(&artifact_dir, &key, &[22])
        .unwrap()
        .unwrap();
    forget_blob_registration_for_restart_test(&payloads[0]);
    verify_registered_blob(&payloads[0])
        .expect("the v2 generation digest must rebuild registry trust after restart");
}
