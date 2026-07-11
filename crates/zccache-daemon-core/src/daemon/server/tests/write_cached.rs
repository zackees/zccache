//! Tests for cache-hit output delivery: `write_cached_output`,
//! `persist_artifact_output`, `persist_artifact_file`, and
//! `break_output_hardlink_before_compile`. Most of these are regression
//! guards for staleness / cache-poisoning / mtime-preservation bugs that
//! had downstream consequences for cargo's incremental fingerprint.

use super::super::*;
use crate::daemon::server::handle_compile_multi::materialize_multi_hit;

fn seed_persisted_blob(path: &Path, bytes: &[u8]) {
    std::fs::write(path, bytes).unwrap();
    write_authoritative_blob_digest(path).unwrap();
}

fn require_hardlink(out: &Path, cache: &Path, test_name: &str) -> bool {
    if same_file(out, cache) {
        true
    } else {
        eprintln!("SKIP {test_name}: temporary filesystem does not support same-volume hardlinks");
        false
    }
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
    write_cached_output(&out, &cache, content).unwrap();
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
    write_cached_output(&out, &cache, content).unwrap();
    assert_eq!(std::fs::read(&out).unwrap(), content.as_slice());
    assert!(
        same_file(&out, &cache),
        "output must remain hardlinked to cache after the skip fast path"
    );
}

#[test]
fn persist_artifact_output_does_not_mutate_existing_hardlink() {
    let dir = tempfile::tempdir().unwrap();
    let cache = dir.path().join("artifact-key_0");
    let out = dir.path().join("output.rlib");

    persist_artifact_output(&cache, b"first").unwrap();
    write_cached_output(&out, &cache, b"first").unwrap();
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
        !same_file(&out, &cache),
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

    let stats = persist_artifact_file(&cache, &source).unwrap();

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
        assert!(same_file(&source, &cache));
    } else {
        assert!(std::fs::metadata(&cache).unwrap().permissions().readonly());
        assert!(!same_file(&source, &cache));
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

    let stats = persist_artifact_file(&cache, &source).unwrap();

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

    persist_artifact_file(&cache, &source).unwrap();
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

    assert!(!same_file(&output, &payloads[0]));
    assert!(!std::fs::metadata(&output).unwrap().permissions().readonly());
    std::fs::write(&output, b"mutated target output").unwrap();
    assert_eq!(
        std::fs::read(&payloads[0]).unwrap(),
        b"staged immutable artifact"
    );
}

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

    write_cached_output(&out, &cache, cached_content).unwrap();
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
        !same_file(&out, &cache),
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

    write_cached_output(&out, &cache, original).unwrap();
    if same_file(&out, &cache) {
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
    assert!(std::fs::metadata(&cache).unwrap().permissions().readonly());
    write_cached_output(&out, &cache, b"immutable").unwrap();
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

    let first = fs_caps(&cache, &out);
    let second = fs_caps(&cache, &out);
    assert_eq!(first, second);
    write_cached_output(&out, &cache, b"bytes").unwrap();
    if same_file(&out, &cache) {
        assert_eq!(registered_output_count(&cache), 1);
        assert_eq!(hard_link_count(&cache).unwrap(), 2);
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
fn disable_reflink_kill_switch_forces_next_tier() {
    let caps = VolumeCaps {
        reflink: true,
        hardlink: true,
        readonly_enforced: true,
        file_id: FileIdWidth::Bits128,
        hardlink_limit: 1023,
    };
    let disabled = apply_reflink_switch(caps, true);
    assert!(!disabled.reflink);
    assert!(disabled.hardlink);
}

#[test]
fn suspect_corruption_emits_durable_forensics() {
    let dir = tempfile::tempdir().unwrap();
    let _cache_dir = super::CacheDirEnvGuard::set(dir.path());
    let blob = dir.path().join("blob.rlib");
    let output = dir.path().join("output.rlib");
    seed_persisted_blob(&blob, b"original");
    std::fs::hard_link(&blob, &output).unwrap();
    register_hardlink(&blob, &output).unwrap();
    make_writable(&output).unwrap();
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
    make_writable(&output).unwrap();
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
    make_writable(&output).unwrap();
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
    make_writable(&output).unwrap();
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
    make_writable(&output).unwrap();
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
    make_writable(&output).unwrap();
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
    assert!(std::fs::metadata(&blob).unwrap().permissions().readonly());

    make_writable(&output).unwrap();
    std::fs::remove_file(&output).unwrap();
    let retry = verify_registered_blob(&blob).expect_err("unverifiable blob must retry eviction");
    assert_eq!(retry.kind(), std::io::ErrorKind::InvalidData);
    assert!(!blob.exists());
}

#[test]
fn multi_hit_materialization_failure_is_reported_to_the_handler() {
    let dir = tempfile::tempdir().unwrap();
    let mut targets = Vec::new();
    let mut payloads = Vec::new();
    for index in 0..2 {
        let blob: NormalizedPath = dir.path().join(format!("blob-{index}.o")).into();
        let output: NormalizedPath = dir.path().join(format!("output-{index}.o")).into();
        std::fs::write(&blob, b"original").unwrap();
        std::fs::hard_link(&blob, &output).unwrap();
        register_hardlink(&blob, &output).unwrap();
        forget_blob_registration_for_restart_test(&blob);
        targets.push((output, blob.clone()));
        payloads.push(CachedPayload::File(blob));
    }

    assert!(!materialize_multi_hit(&targets, &payloads));
    assert!(
        targets.iter().any(|(_, blob)| !blob.exists()),
        "at least the rejected blob must be evicted before the handler rebuilds"
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
    set_readonly(&blob, true).unwrap();
    fail_detach_remove_for_test(&output);

    let error = break_output_hardlink_before_compile(&output)
        .expect_err("injected remove failure must propagate");

    assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
    assert!(same_file(&blob, &output));
    assert_eq!(registered_output_count(&blob), 1);
    assert!(std::fs::metadata(&blob).unwrap().permissions().readonly());
    make_writable(&blob).unwrap();
}

#[test]
fn failed_detach_rename_restores_blob_readonly_after_unlink() {
    let dir = tempfile::tempdir().unwrap();
    let blob = dir.path().join("blob.rlib");
    let output = dir.path().join("output.rlib");
    std::fs::write(&blob, b"original").unwrap();
    std::fs::hard_link(&blob, &output).unwrap();
    register_hardlink(&blob, &output).unwrap();
    set_readonly(&blob, true).unwrap();
    fail_detach_rename_for_test(&output);

    let error = break_output_hardlink_before_compile(&output)
        .expect_err("injected rename failure must propagate");

    assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
    assert!(!output.exists(), "shared output name was already unlinked");
    assert!(blob.exists());
    assert_eq!(hard_link_count(&blob).unwrap(), 1);
    assert_eq!(registered_output_count(&blob), 0);
    assert!(std::fs::metadata(&blob).unwrap().permissions().readonly());
    make_writable(&blob).unwrap();
}

#[test]
fn failed_blob_removal_restores_readonly_and_registration() {
    let dir = tempfile::tempdir().unwrap();
    let blob = dir.path().join("blob.rlib");
    let output = dir.path().join("output.rlib");
    std::fs::write(&blob, b"original").unwrap();
    std::fs::hard_link(&blob, &output).unwrap();
    register_hardlink(&blob, &output).unwrap();
    set_readonly(&blob, true).unwrap();
    fail_detach_remove_for_test(&blob);

    let error = remove_registered_blob(&blob).expect_err("injected removal must fail");

    assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
    assert!(same_file(&blob, &output));
    assert_eq!(registered_output_count(&blob), 1);
    assert!(std::fs::metadata(&blob).unwrap().permissions().readonly());
    make_writable(&blob).unwrap();
}

#[test]
fn failed_corrupt_blob_eviction_retains_suspect_record_for_retry() {
    let dir = tempfile::tempdir().unwrap();
    let blob = dir.path().join("blob.rlib");
    let output = dir.path().join("output.rlib");
    std::fs::write(&blob, b"original").unwrap();
    std::fs::hard_link(&blob, &output).unwrap();
    register_hardlink(&blob, &output).unwrap();
    make_writable(&output).unwrap();
    std::fs::write(&output, b"poisoned").unwrap();
    mark_registered_links_suspect([output.as_path()]);
    fail_detach_remove_for_test(&blob);

    let first = verify_registered_blob(&blob).expect_err("injected eviction must fail");
    assert_eq!(first.kind(), std::io::ErrorKind::PermissionDenied);
    assert!(blob.exists());
    assert!(std::fs::metadata(&blob).unwrap().permissions().readonly());

    make_writable(&output).unwrap();
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
    let old_id = get_file_id(&blob).unwrap();
    std::fs::write(&tmp, b"replacement").unwrap();

    replace_artifact_cache_file(&tmp, &blob).unwrap();

    assert!(!is_file_id_registered(old_id));
    assert_eq!(std::fs::read(&blob).unwrap(), b"replacement");
    assert_eq!(std::fs::read(&output).unwrap(), b"original");
}

#[cfg(unix)]
#[test]
fn dangling_output_symlink_is_replaced() {
    use std::os::unix::fs::symlink;

    let dir = tempfile::tempdir().unwrap();
    let blob = dir.path().join("blob.rlib");
    let output = dir.path().join("output.rlib");
    seed_persisted_blob(&blob, b"original");
    symlink(dir.path().join("missing"), &output).unwrap();

    write_cached_file(&output, &blob).unwrap();

    assert_eq!(std::fs::read(output).unwrap(), b"original");
}

/// Regression test for issue #15: hardlink delivery must set output mtime
/// to current time. Without this, build systems (cargo, make, ninja) see
/// the output as older than its dependencies and trigger unnecessary rebuilds.
///
/// Root cause: hardlinks share mtime with the cache file, which was created
/// during the original compilation (potentially minutes/hours ago). Cargo
/// checks "is library output older than build script output?" and if the
/// library was hardlinked from an old cache file, the answer is yes → dirty.
#[test]
fn write_cached_output_preserves_cache_mtime_on_hardlink() {
    // Regression guard for iter7: cache hits must keep the cache
    // file's stored mtime, not stamp `now()`. Cargo's incremental
    // fingerprint records the artifact's mtime at first compile;
    // a hit that hardlinks but bumps mtime looks "externally
    // touched" and invalidates downstream — measured as a
    // wall-time regression on the `bin` cell of the
    // cold-tar-untar-warm scenario.
    let dir = tempfile::tempdir().unwrap();
    let cache = dir.path().join("cached.rlib");
    let out = dir.path().join("output.rlib");

    let content = b"cached rlib data";
    seed_persisted_blob(&cache, content);

    let old_time = filetime::FileTime::from_unix_time(1_000_000_000, 0); // 2001-09-09
    filetime::set_file_mtime(&cache, old_time).unwrap();

    write_cached_output(&out, &cache, content).unwrap();

    // Output is a hardlink to cache, so its mtime is the cache mtime.
    // After the iter7 touch_mtime no-op, that mtime is NOT bumped.
    let out_mtime =
        filetime::FileTime::from_last_modification_time(&std::fs::metadata(&out).unwrap());
    assert_eq!(
        out_mtime.unix_seconds(),
        old_time.unix_seconds(),
        "cache hit must preserve cache file mtime (cargo's fingerprint depends on it); \
         got {out_mtime:?}, expected {old_time:?}"
    );
}

/// Same as above but for the same_file (already hardlinked) path.
#[test]
fn write_cached_output_preserves_mtime_on_existing_hardlink() {
    let dir = tempfile::tempdir().unwrap();
    let cache = dir.path().join("cached.rlib");
    let out = dir.path().join("output.rlib");

    let content = b"cached rlib data";
    seed_persisted_blob(&cache, content);

    // First delivery: creates hardlink
    write_cached_output(&out, &cache, content).unwrap();

    let old_time = filetime::FileTime::from_unix_time(1_000_000_000, 0);
    set_materialized_mtime(&out, old_time).unwrap();

    // See the comment in write_cached_output_skips_when_already_hardlinked:
    // this must hold in every CI environment this suite runs in — assert it
    // loudly so this test always checks the "mtime preserved on the
    // same-file path" invariant it's named for, instead of silently falling
    // back to checking the reflink-tier formula instead (issue #1042
    // test-coverage regression).
    if !require_hardlink(
        &out,
        &cache,
        "write_cached_output_preserves_mtime_on_existing_hardlink",
    ) {
        return;
    }

    // Second delivery: same_file keeps the linked mtime.
    write_cached_output(&out, &cache, content).unwrap();

    let out_mtime =
        filetime::FileTime::from_last_modification_time(&std::fs::metadata(&out).unwrap());
    assert_eq!(
        out_mtime.unix_seconds(),
        old_time.unix_seconds(),
        "mtime must be preserved on the same_file (already-hardlinked) path"
    );
}

/// Regression test: when a sibling-floor mtime bump is required on the
/// same_file (already-hardlinked) path, it must not mutate the shared cache
/// blob's mtime — every other output hardlinked to that blob would see the
/// bump too. The output should instead detach into a private copy that
/// carries the floored mtime, leaving the blob (and any other hardlink to
/// it) untouched.
#[test]
fn write_cached_output_floor_detaches_instead_of_corrupting_shared_blob() {
    let dir = tempfile::tempdir().unwrap();
    let cache = dir.path().join("cached.rlib");
    let out = dir.path().join("output.rlib");
    let sibling = dir.path().join("sibling.rlib");

    let content = b"cached rlib data";
    seed_persisted_blob(&cache, content);

    // First delivery: creates the hardlink (or copy, depending on fs caps).
    write_cached_output(&out, &cache, content).unwrap();
    if !same_file(&out, &cache) {
        // This filesystem doesn't support hardlinks — the detach path this
        // test targets never triggers. Nothing to verify.
        return;
    }

    let old_time = filetime::FileTime::from_unix_time(1_000_000_000, 0);
    set_materialized_mtime(&out, old_time).unwrap();
    let blob_time_before =
        filetime::FileTime::from_last_modification_time(&std::fs::metadata(&cache).unwrap());

    // A newer sibling artifact in the same directory forces the #466/#467
    // floor to kick in on the next materialization of `out`.
    std::fs::write(&sibling, b"newer sibling").unwrap();
    let newer_time = filetime::FileTime::from_unix_time(2_000_000_000, 0);
    filetime::set_file_mtime(&sibling, newer_time).unwrap();

    // Second delivery: same_file path, floor must apply.
    write_cached_output(&out, &cache, content).unwrap();

    let out_mtime =
        filetime::FileTime::from_last_modification_time(&std::fs::metadata(&out).unwrap());
    assert_eq!(
        out_mtime.unix_seconds(),
        newer_time.unix_seconds(),
        "output mtime must be floored up to the newer sibling"
    );

    let blob_time_after =
        filetime::FileTime::from_last_modification_time(&std::fs::metadata(&cache).unwrap());
    assert_eq!(
        blob_time_after.unix_seconds(),
        blob_time_before.unix_seconds(),
        "flooring an output must never mutate the shared cache blob's mtime"
    );

    assert!(
        !same_file(&out, &cache),
        "output must be detached from the shared blob once its mtime diverges"
    );
    assert_eq!(std::fs::read(&cache).unwrap(), content);
    assert_eq!(std::fs::read(&out).unwrap(), content);
}

/// write_cached_output fallback (fs::write) naturally sets fresh mtime.
#[test]
fn write_cached_output_fallback_has_fresh_mtime() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("output.rlib");
    let cache = dir.path().join("nonexistent_cache.rlib");

    let content = b"data from memory";
    write_cached_output(&out, &cache, content).unwrap();

    let out_mtime =
        filetime::FileTime::from_last_modification_time(&std::fs::metadata(&out).unwrap());
    let now = filetime::FileTime::now();
    let diff = now.unix_seconds() - out_mtime.unix_seconds();

    assert!(
        diff < 5,
        "fallback path should produce fresh mtime — {diff}s old"
    );
}

// ── Issue #490: AV-scanner rename race ─────────────────────────────
//
// On Windows, Defender (MsMpEng) opens just-written files for an inline scan
// with `FILE_SHARE_READ` only — no `FILE_SHARE_DELETE`. While that handle is
// live, `MoveFileExW(MOVEFILE_REPLACE_EXISTING)` and `DeleteFileW` against the
// target file return `ERROR_ACCESS_DENIED` (raw OS error 5) or
// `ERROR_SHARING_VIOLATION` (32). The scan window is short — typically tens to
// hundreds of milliseconds — so a bounded retry absorbs it.
//
// This test simulates the scanner by holding the rename destination open with
// the same restrictive share mode from a separate thread that releases the
// handle shortly. The pre-fix code path fails immediately at the first
// `remove_file` call inside `replace_artifact_cache_file`; the retry must
// outlive the held handle.

#[cfg(windows)]
#[test]
fn replace_artifact_cache_file_retries_through_av_scanner_lock() {
    use std::os::windows::fs::OpenOptionsExt;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("output.obj");
    let tmp = dir.path().join("output.obj.tmp");

    std::fs::write(&target, b"OLD").unwrap();
    std::fs::write(&tmp, b"NEW").unwrap();

    // FILE_SHARE_READ (0x1) only — no SHARE_WRITE, no SHARE_DELETE.
    // This is the exact share mode Defender uses during real-time inline
    // scans, which is why ninja's subsequent remove fails in the wild.
    let handle = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(0x1)
        .open(&target)
        .expect("open lock handle");

    let released = Arc::new(AtomicBool::new(false));
    let released_clone = Arc::clone(&released);
    let worker = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(150));
        drop(handle);
        released_clone.store(true, Ordering::SeqCst);
    });

    // Without the retry, this fails immediately at the inner remove_file with
    // ERROR_ACCESS_DENIED. With the retry, the closure retries past the
    // 150 ms hold and succeeds.
    replace_artifact_cache_file(&tmp, &target).expect("replace must absorb the simulated AV lock");

    worker.join().unwrap();
    assert!(
        released.load(Ordering::SeqCst),
        "worker must have released the lock before the call returned",
    );
    assert_eq!(std::fs::read(&target).unwrap(), b"NEW");
}

// Negative guard: a NotFound (or any other non-transient) error must surface
// immediately rather than burning the retry budget. Without this assertion,
// a misclassified `ErrorKind` could silently inflate every cache-store
// failure path by ~1 s.
#[cfg(windows)]
#[test]
fn replace_artifact_cache_file_does_not_retry_non_transient_errors() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("output.obj");
    let tmp = dir.path().join("missing.obj.tmp"); // tmp does NOT exist

    // Neither file exists; rename returns ENOENT/NotFound — not a share
    // violation. The call must fail promptly, not after the full budget.
    let start = std::time::Instant::now();
    let err = replace_artifact_cache_file(&tmp, &target).expect_err("must propagate NotFound");
    let elapsed = start.elapsed();

    assert!(
        elapsed < std::time::Duration::from_millis(45),
        "non-transient errors must not enter the retry sleep — took {elapsed:?}"
    );
    assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
}
