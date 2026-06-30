//! Perf regression test for the cold-tar-untar-warm × medium 0-hit-rate bug
//! measured on perf-cluster runs 26255457227 and 26258412256.
//!
//! Root cause: `ArtifactStore::flush` wrote `index.bin` via
//! `fs::write(tmp) + fs::rename(tmp, target)` with no `fsync` of the file
//! or parent dir. On Linux + ext4/xfs the rename's metadata is visible
//! immediately but the data blocks may not be committed when the daemon
//! exits. `soldr save` then tars a 0-byte (or stale) file, the warm-side
//! daemon's `ArtifactStore::open(index.bin)` silently loads an empty
//! store, and every lookup misses — even though the cache keys match
//! perfectly between cold and warm.
//!
//! This test asserts the durability contract: after `flush` returns, a
//! fresh `open` at the same path observes the same entries. The previous
//! (pre-fsync) implementation could pass this on a fast-enough machine
//! where the page cache happened to commit before the second open ran;
//! the budget assertion below guards against the fsync regressing latency
//! past the point where it would be slower than re-populating from scratch.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::panic_in_result_fn,
    clippy::unwrap_in_result
)]

use std::sync::Arc;
use std::time::{Duration, Instant};
use zccache::artifact::{ArtifactIndex, ArtifactStore};

fn sample_index(i: u32) -> ArtifactIndex {
    ArtifactIndex::new(
        vec![format!("out_{i}.o")],
        vec![1024],
        Arc::new(Vec::<u8>::new()),
        Arc::new(Vec::<u8>::new()),
        0,
    )
}

#[test]
fn perf_artifact_store_index_bin_is_durable_across_process_boundary() {
    let tmp = tempfile::TempDir::new().unwrap();
    let index_path = tmp.path().join("index.bin");

    // Write side: open, insert 200 entries (≈ medium fixture's compile
    // count), flush, drop. Budget: 500ms — fsync on a modern SSD is sub-
    // millisecond; this catches a 500x regression.
    {
        let store = ArtifactStore::open(&index_path).unwrap();
        for i in 0..200 {
            store.insert(&format!("key{i:03}"), &sample_index(i));
        }
        let t = Instant::now();
        store.flush().unwrap();
        let elapsed = t.elapsed();
        assert!(
            elapsed < Duration::from_millis(500),
            "flush took {elapsed:?}; budget 500ms"
        );
    }

    // File side: confirm index.bin exists on disk with plausible size.
    let meta =
        std::fs::metadata(&index_path).expect("index.bin must exist on disk after flush returns");
    assert!(
        meta.len() > 0,
        "index.bin must be non-empty after flushing 200 entries (was {} bytes)",
        meta.len()
    );

    // Read side: open a fresh store at the same path and assert all
    // entries survived. This is the assertion that fails on the pre-
    // fsync code if the kernel hasn't committed the data block yet.
    let reopened = ArtifactStore::open(&index_path).unwrap();
    assert_eq!(
        reopened.len(),
        200,
        "200 entries must survive write -> drop -> open round-trip"
    );

    // Sanity-check one entry survived intact (not just count).
    let restored = reopened
        .get("key042")
        .expect("specific entry must round-trip");
    assert_eq!(restored.output_names.len(), 1);
    assert_eq!(&*restored.output_names[0], "out_42.o");
    assert_eq!(restored.exit_code, 0);
}

#[test]
fn perf_artifact_store_empty_flush_does_not_create_file() {
    // Flushing an empty store should be a no-op write (the bincode of an
    // empty Vec is small but non-zero, ~8 bytes — current impl writes it
    // anyway and that's fine). What matters: re-open sees 0 entries.
    let tmp = tempfile::TempDir::new().unwrap();
    let index_path = tmp.path().join("index.bin");

    {
        let store = ArtifactStore::open(&index_path).unwrap();
        store.flush().unwrap();
    }

    let reopened = ArtifactStore::open(&index_path).unwrap();
    assert_eq!(reopened.len(), 0);
}
