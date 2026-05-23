//! Perf regression tests for `MetadataCache` disk persistence.
//!
//! These exist because losing `MetadataCache` across daemon restarts
//! is invisible in unit tests yet catastrophic on the warm side of the
//! `cold-tar-untar-warm × medium` perf-rust-cluster cell: without
//! persistence, every fresh daemon (including the one spawned after
//! `soldr load` restores a cache dir) starts with an empty DashMap
//! and pays full stat+blake3 on every header lookup.
//!
//! Two regression-protecting assertions:
//!
//! 1. **`perf_metadata_save_load_roundtrip_under_50ms`** — loading
//!    a 200-entry snapshot stays under 50ms. If a future refactor
//!    accidentally introduces O(n^2) decode or per-entry syscalls,
//!    this fails loudly long before it shows up in a cluster run.
//!
//! 2. **`perf_metadata_load_enables_fast_path_after_restart`** —
//!    after `save → drop → load`, the
//!    `get_cached_hash_if_stat_valid` fast path still returns the
//!    cached hash for a real file. If a refactor breaks the
//!    confidence/clamp semantics so the safety net rejects every
//!    restored entry, this fails — that would silently revert the
//!    warm-side perf win.

use std::fs;
use std::time::{Duration, Instant, SystemTime};
use tempfile::TempDir;
use zccache_monocrate::core::NormalizedPath;
use zccache_fscache::{Confidence, FileMetadata, MetadataCache};

/// Roughly the order of magnitude of headers a `medium` perf fixture
/// touches; large enough to surface algorithmic regressions, small
/// enough that the assertion is robust on CI runners.
const ENTRY_COUNT: usize = 200;

/// Conservative load budget. The actual load time is observed to be in
/// the low single-digit milliseconds on a developer laptop; 50ms gives
/// CI runners headroom while still catching pathological regressions.
const LOAD_BUDGET: Duration = Duration::from_millis(50);

fn populate(cache: &MetadataCache, count: usize) {
    for i in 0..count {
        let key = NormalizedPath::from(format!("/virtual/perf/header_{i:04}.h"));
        cache.insert(
            key,
            FileMetadata {
                mtime: SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000 + i as u64),
                size: 4096 + i as u64,
                confidence: Confidence::High,
                last_verified: Instant::now(),
                // 32-byte hash, deterministic per index so the bytes
                // round-trip predictably and we can spot decode bugs.
                content_hash: Some({
                    let mut h = [0u8; 32];
                    h[0] = (i & 0xff) as u8;
                    h[1] = ((i >> 8) & 0xff) as u8;
                    h
                }),
            },
        );
    }
}

#[test]
// regression test for the cold-tar-untar-warm × medium warm-side fast-path miss
fn perf_metadata_save_load_roundtrip_under_50ms() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("metadata.bin");

    let cache = MetadataCache::new();
    populate(&cache, ENTRY_COUNT);

    cache.save_to_disk(&path).expect("save_to_disk");
    drop(cache);

    let start = Instant::now();
    let loaded = MetadataCache::load_from_disk(&path).expect("load_from_disk");
    let elapsed = start.elapsed();

    eprintln!(
        "perf_metadata_save_load_roundtrip_under_50ms: load of {} entries took {:?}",
        ENTRY_COUNT, elapsed
    );

    assert_eq!(loaded.len(), ENTRY_COUNT);
    assert!(
        elapsed < LOAD_BUDGET,
        "metadata snapshot load took {:?} (budget {:?}); regression vs. the warm-restart fast path",
        elapsed,
        LOAD_BUDGET
    );
}

#[test]
// regression test for the cold-tar-untar-warm × medium warm-side fast-path miss
fn perf_metadata_load_enables_fast_path_after_restart() {
    let dir = TempDir::new().unwrap();
    let metadata_path = dir.path().join("metadata.bin");

    // Create a real file so the stat-verify safety net inside
    // `get_cached_hash_if_stat_valid` has something to compare against.
    let file_path = dir.path().join("header.h");
    fs::write(&file_path, b"#pragma once\nint x = 1;\n").unwrap();
    let normalized = NormalizedPath::from(file_path.as_path());

    // Populate via the real lookup path so (mtime, size, hash) match
    // the on-disk file exactly — the round-trip is testing that the
    // post-load entry survives the stat check.
    let cache = MetadataCache::new();
    let initial_hash = cache.lookup(file_path.as_path()).expect("lookup");

    cache.save_to_disk(&metadata_path).expect("save_to_disk");
    drop(cache);

    // Fresh process boundary: nothing in memory, nothing in the new
    // cache's DashMap until load_from_disk repopulates it.
    let restored = MetadataCache::load_from_disk(&metadata_path).expect("load_from_disk");
    assert_eq!(restored.len(), 1, "exactly one entry restored");

    let recovered = restored.get_cached_hash_if_stat_valid(&normalized).expect(
        "post-restart fast path missed — this is the warm-side regression the perf-cluster catches",
    );
    assert_eq!(
        *recovered.as_bytes(),
        *initial_hash.as_bytes(),
        "restored hash diverged from original — persistence is corrupting content_hash",
    );
}
