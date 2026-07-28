//! Watcher-lifecycle regressions for issue #1156.
//!
//! Two silent degradations used to strand the daemon on its slowest paths:
//! a failed watcher init was a lifetime sentence (no re-arm), and a transient
//! event-queue overflow condemned the entire hardlink registry to a blake3
//! re-hash storm. These tests pin the recovery behavior of both.
//!
//! The link registry is a process-global static, so every assertion here is
//! scoped to the test's own record by `FileId` — the aggregate sweep counters
//! also see records registered by tests running in parallel.

use super::super::run::{arm_watcher_pipeline, record_watcher_degradation};
use super::super::*;

/// Registers a blob/output hardlink pair and returns the registry identity,
/// or `None` when the temp filesystem cannot make same-volume hardlinks.
fn seed_registered_link(blob: &Path, output: &Path, bytes: &[u8]) -> Option<FileId> {
    std::fs::write(blob, bytes).unwrap();
    if std::fs::hard_link(blob, output).is_err() {
        return None;
    }
    register_hardlink(blob, output).unwrap();
    registered_blob_id(blob)
}

#[test]
fn overflow_keeps_stat_unchanged_blobs_trusted() {
    let dir = tempfile::tempdir().unwrap();
    let blob = dir.path().join("blob.rlib");
    let output = dir.path().join("output.rlib");
    let Some(id) = seed_registered_link(&blob, &output, b"original") else {
        eprintln!("SKIP overflow_keeps_stat_unchanged_blobs_trusted: no hardlink support");
        return;
    };

    // Overflow means "the event queue saturated", not "every file changed".
    // An untouched blob must survive the sweep still trusted, so its next read
    // does not pay a full blake3 re-hash.
    mark_changed_registered_links_suspect();
    assert_eq!(
        registered_link_suspect(id),
        Some(false),
        "stat-unchanged blob must stay trusted across a watcher overflow"
    );
    verify_registered_blob(&blob).expect("untouched blob must still verify");
    assert!(blob.exists(), "a trusted blob must not be evicted");
}

#[test]
fn overflow_still_rejects_a_blob_poisoned_through_its_alias() {
    let dir = tempfile::tempdir().unwrap();
    let blob = dir.path().join("blob.rlib");
    let output = dir.path().join("output.rlib");
    let Some(id) = seed_registered_link(&blob, &output, b"original") else {
        eprintln!("SKIP overflow_still_rejects_a_blob_poisoned_through_its_alias: no hardlinks");
        return;
    };

    // Writing through the alias mutates the shared inode, so the blob's own
    // (mtime, size) signature changes. The cheap sweep must notice and fall
    // back to the full re-hash, which then rejects and evicts the blob.
    make_writable(&output).unwrap();
    std::fs::write(&output, b"poisoned with a different length").unwrap();

    mark_changed_registered_links_suspect();
    assert_eq!(
        registered_link_suspect(id),
        Some(true),
        "a blob mutated through its alias must be marked suspect by the sweep"
    );

    let error = verify_registered_blob(&blob).expect_err("overflow poison must be rejected");
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    assert!(!blob.exists(), "a poisoned blob must be evicted");
}

#[test]
fn overflow_marks_an_unstattable_blob_suspect() {
    let dir = tempfile::tempdir().unwrap();
    let blob = dir.path().join("blob.rlib");
    let output = dir.path().join("output.rlib");
    let Some(id) = seed_registered_link(&blob, &output, b"original") else {
        eprintln!("SKIP overflow_marks_an_unstattable_blob_suspect: no hardlink support");
        return;
    };

    // A blob that cannot be stat'd yields no cheap evidence, so the sweep must
    // fall back to suspect rather than silently vouching for it.
    make_writable(&blob).unwrap();
    std::fs::remove_file(&blob).unwrap();

    mark_changed_registered_links_suspect();
    assert_eq!(
        registered_link_suspect(id),
        Some(true),
        "an unstattable blob has no cheap evidence and must be marked suspect"
    );
}

#[tokio::test]
async fn watcher_degradation_is_recorded_and_recoverable() {
    let dir = tempfile::tempdir().unwrap();
    let cache_dir: NormalizedPath = dir.path().join("cache").into();
    let server =
        DaemonServer::bind_with_cache_dir(&crate::ipc::unique_test_endpoint(), &cache_dir).unwrap();
    let state = server.test_state_arc();

    // A failed watcher init must be visible, not a single startup warn line.
    record_watcher_degradation(&state, "injected watcher init failure");
    assert!(
        !state.watcher_active.load(Ordering::Acquire),
        "degradation must disable the fast hit tiers"
    );
    assert_eq!(
        state.watcher_degradations.load(Ordering::Relaxed),
        1,
        "degradation must bump the status-visible counter"
    );

    // ...and it must be a temporary condition: re-arming restores the pipeline
    // rather than leaving the daemon degraded for its whole lifetime.
    assert!(
        arm_watcher_pipeline(&state).await,
        "re-arm must succeed once the injected failure clears"
    );
    assert!(
        state.watcher_active.load(Ordering::Acquire),
        "a successful re-arm must restore the fast hit tiers"
    );
    assert_eq!(
        state.watcher_degradations.load(Ordering::Relaxed),
        1,
        "recovery must preserve the degradation count for post-hoc diagnosis"
    );

    state.shutdown_requested.store(true, Ordering::Release);
    state.shutdown.notify_waiters();
}
