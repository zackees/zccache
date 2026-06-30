//! Perf regression test for the DashMap → on-disk-`ArtifactStore`
//! lookup fallthrough.
//!
//! regression test for the cold-tar-untar-warm warm-side 0% hit rate
//! measured on run 26255457227
//!
//! Background
//! ----------
//! Daemon startup loads the on-disk `index.bin` synchronously inside
//! `ArtifactStore::open`, then *asynchronously* mirrors those entries
//! into `state.artifacts` (a `DashMap`) via a background task. Every
//! artifact-lookup site historically went `state.artifacts.get_mut`
//! and treated `None` as MISS — so the warm-after-restore window
//! (`soldr load` → first compile, before the background task has
//! drained) reported a cache miss on every key, even though the
//! entry was already present in the on-disk store.
//!
//! Measured on perf-cluster run 26255457227 (medium fixture,
//! `cold-tar-untar-warm` scenario): 0 hits / 115 cacheable lookups
//! on the warm-side daemon. Cold and warm cache keys matched
//! byte-for-byte (115/115 ctx + 115/115 artifact_key) — the lookup
//! itself was the bug.
//!
//! What this test asserts (see inline asserts for the details):
//!
//! - A fresh daemon, bound at a `cache_dir` whose `index.bin` has been
//!   pre-populated, sees an empty in-memory DashMap.
//! - The lookup helper returns `Some` anyway — proof that the fallthrough
//!   to the on-disk `ArtifactStore` works.
//! - After the first fallthrough, the DashMap is hydrated, so a
//!   subsequent lookup hits the fast path (DashMap len == 1).
//! - The fallback lookup completes inside a 50 ms budget. The on-disk
//!   store is itself an in-memory hashmap (hydrated by
//!   `ArtifactStore::open`), so the cost is a DashMap miss + a hashmap
//!   hit + a tiny `from_index` conversion + an insert. 50 ms is
//!   generous against the actual single-digit-µs cost; the budget
//!   catches accidentally re-introducing a disk read or a blocking
//!   syscall on the lookup hot path.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::panic_in_result_fn, clippy::unwrap_in_result)]

use std::sync::Arc;
use std::time::{Duration, Instant};

use tempfile::TempDir;
use zccache::artifact::{ArtifactIndex, ArtifactStore};
use zccache::core::NormalizedPath;
use zccache::daemon::DaemonServer;

/// Cache layout helper: pre-populate `index.bin` with one entry
/// keyed `key_hex` and return the cache_dir the daemon should bind
/// against.
fn seed_cache_dir(dir: &TempDir, key_hex: &str) -> NormalizedPath {
    let cache_dir = NormalizedPath::new(dir.path());
    let artifact_dir = zccache::core::config::artifacts_dir_from_cache_dir(&cache_dir);
    std::fs::create_dir_all(&artifact_dir).expect("create artifact dir");

    // Write the payload file the daemon expects at `{artifact_dir}/{key}_0`.
    // The actual byte contents don't matter for this test — we never load
    // the payload — but the file's *size* must match
    // `ArtifactIndex::output_sizes[0]` so `ensure_payloads` would accept
    // it on a real cache-hit codepath. We use a 4-byte payload for clarity.
    let payload: &[u8] = b"abcd";
    let payload_path = artifact_dir.join(format!("{key_hex}_0"));
    std::fs::write(payload_path.as_path(), payload).expect("write payload");

    // Build a one-entry `index.bin` with `ArtifactStore`, then drop the
    // store so the daemon's own `ArtifactStore::open` reads the blob
    // from disk like a fresh restore.
    let index_path = zccache::core::config::index_path_from_cache_dir(&cache_dir);
    {
        let store = ArtifactStore::open(index_path.as_path()).expect("open store");
        let meta = ArtifactIndex::new(
            vec!["foo.o".to_string()],
            vec![payload.len() as u64],
            b"compiler stdout".to_vec(),
            b"compiler stderr".to_vec(),
            0,
        );
        store.insert(key_hex, &meta);
        store.flush().expect("flush index.bin");
    }
    assert!(index_path.exists(), "seeded index.bin should exist on disk");

    cache_dir
}

#[tokio::test]
async fn perf_artifact_lookup_hits_before_background_load_completes() {
    let dir = tempfile::tempdir().expect("tempdir");
    // 32-byte hex string — same shape `ContentHash::to_hex` would
    // produce on a real cache entry. Content is irrelevant; we only
    // need it to round-trip through `index.bin`.
    let key_hex = "abc123abc123abc123abc123abc123abc123abc123abc123abc123abc123abcd";
    let cache_dir = seed_cache_dir(&dir, key_hex);

    let endpoint = zccache::ipc::unique_test_endpoint();
    let server = DaemonServer::bind_with_cache_dir(&endpoint, &cache_dir).expect("bind daemon");

    // Critical invariant 1: the in-memory DashMap is empty immediately
    // after `bind_with_cache_dir`. The background-load task does not
    // run until `server.run(...)` is awaited; this proves the next
    // lookup MUST go through the on-disk fallback to succeed.
    assert_eq!(
        server.test_artifacts_len(),
        0,
        "DashMap should be empty before the background load runs"
    );
    assert!(
        !server.test_artifacts_loaded(),
        "artifacts_loaded flag should still be false (background task hasn't run)"
    );

    // Critical invariant 2: lookup hits via the on-disk fallback,
    // not via the DashMap (which we just asserted is empty).
    let t0 = Instant::now();
    let found = server.test_lookup_artifact(key_hex);
    let lookup_elapsed = t0.elapsed();
    assert!(
        found,
        "lookup must hit via on-disk fallback; got MISS — regression of run 26255457227"
    );

    // Critical invariant 3: the fallback path hydrates the DashMap
    // so subsequent lookups skip the fallback entirely.
    assert_eq!(
        server.test_artifacts_len(),
        1,
        "DashMap should be populated by the fallback's side-effect"
    );

    // Critical invariant 4: budget. The fallback is a DashMap miss +
    // an in-memory hashmap hit + a from_index conversion + an insert.
    // Nothing on this path should hit disk. 50 ms is generous; the
    // measured cost is in single-digit µs. A breach means someone
    // accidentally added a `std::fs::read` or a `spawn_blocking` on
    // the lookup hot path.
    const BUDGET: Duration = Duration::from_millis(50);
    assert!(
        lookup_elapsed < BUDGET,
        "on-disk fallback lookup took {lookup_elapsed:?}, budget is {BUDGET:?}"
    );

    // Sanity: a second lookup also hits, and now goes through the
    // DashMap fast path (we can't directly observe the path taken,
    // but `test_artifacts_len` confirmed the hydration).
    assert!(
        server.test_lookup_artifact(key_hex),
        "second lookup must hit via DashMap fast path"
    );

    // Negative control: an unrelated key returns MISS — neither path
    // makes up an entry that wasn't seeded.
    let other_key = "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff";
    assert!(
        !server.test_lookup_artifact(other_key),
        "unknown keys must still MISS — fallback shouldn't synthesize entries"
    );
    assert_eq!(
        server.test_artifacts_len(),
        1,
        "MISS must not insert into the DashMap"
    );

    // Cleanup: drop the server so its background tasks (none have
    // started, but `Arc` cycles still need releasing) can unwind.
    drop(server);

    // Belt-and-suspenders: keep the temp dir alive for the asserts above.
    let _ = Arc::new(dir);
}
