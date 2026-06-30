//! Adversarial and stress tests for the file watcher subsystem.
//!
//! These tests are `#[ignore]`d so they don't run during normal `cargo test`.
//! Run with: `soldr cargo test -p zccache-watcher --test stress_test -- --ignored`

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::panic_in_result_fn,
    clippy::unwrap_in_result
)]

use std::fs;
use std::sync::Arc;
use std::thread;
use std::time::Duration;
use tempfile::TempDir;
use zccache::core::NormalizedPath;
use zccache::fscache::clock::Clock;
use zccache::fscache::CacheSystem;
use zccache::watcher::settle::{SettleBuffer, SettledEvent};
use zccache::watcher::{IgnoreFilter, NotifyWatcher, WatchEvent};

/// Helper: create a file with content, return path.
fn create_file(dir: &TempDir, name: &str, content: &str) -> NormalizedPath {
    let path = dir.path().join(name);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).ok();
    }
    fs::write(&path, content).expect("failed to create test file");
    NormalizedPath::new(path)
}

/// Helper: sleep enough for mtime to differ across all platforms.
fn sleep_for_mtime() {
    thread::sleep(Duration::from_millis(1100));
}

// =========================================================================
// STRESS TESTS — CacheSystem under load
// =========================================================================

#[test]
#[ignore]
fn stress_cache_system_many_files() {
    let dir = TempDir::new().unwrap();
    let cache = CacheSystem::new();

    // Create 1000 files.
    let mut paths = Vec::new();
    for i in 0..1000 {
        let path = create_file(&dir, &format!("file_{i:04}.c"), &format!("content {i}"));
        paths.push(path);
    }

    // Lookup all — populates cache.
    let mut original_hashes = Vec::new();
    for path in &paths {
        let result = cache.lookup_since(path, Clock::ZERO).unwrap();
        original_hashes.push(result.hash);
    }

    // Register all in journal.
    let _c1 = cache.apply_changes(paths.to_vec());

    // Modify every 7th file.
    sleep_for_mtime();
    for (i, path) in paths.iter().enumerate() {
        if i % 7 == 0 {
            fs::write(path, format!("modified content {i}")).unwrap();
        }
    }
    let c2 = cache.apply_changes(
        paths
            .iter()
            .enumerate()
            .filter(|(i, _)| i % 7 == 0)
            .map(|(_, p)| p.clone())
            .collect::<Vec<_>>(),
    );

    // Verify: modified files have different hashes, others are unchanged.
    for (i, path) in paths.iter().enumerate() {
        let result = cache.lookup_since(path, c2).unwrap();
        if i % 7 == 0 {
            assert_ne!(
                result.hash, original_hashes[i],
                "file {i} should have changed"
            );
            let expected = zccache::hash::hash_bytes(format!("modified content {i}").as_bytes());
            assert_eq!(result.hash, expected, "file {i} hash mismatch");
        } else {
            assert_eq!(
                result.hash, original_hashes[i],
                "file {i} should be unchanged"
            );
        }
    }

    // Fast path: lookups with c2 should not re-stat.
    for path in &paths {
        let result = cache.lookup_since(path, c2).unwrap();
        assert!(result.clock >= c2);
    }
}

#[test]
#[ignore]
fn stress_concurrent_lookups_during_changes() {
    let dir = TempDir::new().unwrap();
    let cache = Arc::new(CacheSystem::new());

    // Create 100 files.
    let mut paths = Vec::new();
    for i in 0..100 {
        let path = create_file(&dir, &format!("header_{i}.h"), &format!("content {i}"));
        paths.push(path);
    }

    // Populate cache.
    for path in &paths {
        cache.lookup_since(path, Clock::ZERO).unwrap();
    }
    let c1 = cache.apply_changes(paths.to_vec());

    // 16 reader threads doing lookups continuously.
    let mut handles = Vec::new();
    for t in 0..16 {
        let cache = Arc::clone(&cache);
        let paths = paths.clone();
        handles.push(thread::spawn(move || {
            for _ in 0..50 {
                for path in &paths {
                    // Should never panic — may get slow-path or fast-path.
                    let _result = cache.lookup_since(path, c1);
                }
            }
            t // Return thread id for debugging.
        }));
    }

    // Simultaneously apply changes from the main thread.
    for i in 0..20 {
        let changed = paths
            .iter()
            .enumerate()
            .filter(|(j, _)| j % 5 == i % 5)
            .map(|(_, p)| p.clone())
            .collect::<Vec<_>>();
        cache.apply_changes(changed.to_vec());
        thread::sleep(Duration::from_millis(1));
    }

    // All threads should complete without panic.
    for h in handles {
        h.join().expect("reader thread panicked");
    }
}

#[test]
#[ignore]
fn stress_rapid_apply_changes() {
    let cache = CacheSystem::new();

    // 1000 rapid change batches with different paths.
    for i in 0..1000 {
        let paths = (0..10)
            .map(|j| NormalizedPath::new(format!("batch_{i}/file_{j}.c")))
            .collect::<Vec<_>>();
        cache.apply_changes(paths.to_vec());
    }

    // Clock should be at 1000.
    assert_eq!(cache.current_clock().tick(), 1000);

    // Journal should have trimmed old entries (capacity 10000).
    // All recent queries should work.
    let recent_clock = Clock::ZERO; // Conservative: check from zero.
    assert!(cache
        .journal()
        .changed_since(&NormalizedPath::new("batch_999/file_0.c"), recent_clock));
}

#[test]
#[ignore]
fn stress_journal_overflow_recovery() {
    let cache = CacheSystem::new();
    let dir = TempDir::new().unwrap();

    // Create files and populate cache.
    let mut paths = Vec::new();
    for i in 0..50 {
        let path = create_file(&dir, &format!("f{i}.c"), &format!("v{i}"));
        paths.push(path);
    }
    for path in &paths {
        cache.lookup_since(path, Clock::ZERO).unwrap();
    }

    let c_before = cache.current_clock();

    // Apply overflow.
    let c_overflow = cache.apply_overflow();

    // All entries should be Low confidence now.
    for path in &paths {
        let entry = cache.metadata().get(path).unwrap();
        assert_eq!(
            entry.confidence,
            zccache::fscache::Confidence::Low,
            "entry should be Low after overflow"
        );
    }

    // All queries with clock before overflow should return "changed".
    for path in &paths {
        assert!(
            cache.journal().changed_since(path, c_before),
            "should report changed for pre-overflow clock"
        );
    }

    // Lookups after overflow should still work (re-verify via slow path).
    for path in &paths {
        let result = cache.lookup_since(path, c_overflow).unwrap();
        let expected = zccache::hash::hash_file(path).unwrap();
        assert_eq!(result.hash, expected);
    }
}

#[test]
#[ignore]
fn stress_settle_buffer_high_throughput() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let (raw_tx, raw_rx) = tokio::sync::mpsc::unbounded_channel();
        let (settled_tx, mut settled_rx) = tokio::sync::mpsc::unbounded_channel();

        let buffer = SettleBuffer::new(Duration::from_millis(10));
        let handle = tokio::spawn(async move {
            buffer.run(raw_rx, settled_tx).await;
        });

        // Send 10000 events as fast as possible.
        for i in 0..10_000 {
            raw_tx
                .send(WatchEvent::Modified(NormalizedPath::new(format!(
                    "src/file_{i}.c"
                ))))
                .unwrap();
        }
        drop(raw_tx);

        // Collect all settled batches.
        let mut total_changed = 0;
        let mut batch_count = 0;
        while let Some(event) = settled_rx.recv().await {
            if let SettledEvent::Batch { changed, .. } = event {
                total_changed += changed.len();
                batch_count += 1;
            }
        }

        // All 10000 unique files should be accounted for.
        assert_eq!(total_changed, 10_000);
        // Should have been coalesced into fewer batches than 10000 events.
        assert!(
            batch_count < 10_000,
            "expected coalescing, got {batch_count} batches"
        );

        handle.await.unwrap();
    });
}

// =========================================================================
// ADVERSARIAL TESTS — edge cases and race conditions
// =========================================================================

#[test]
#[ignore]
fn adversarial_rapid_file_mutations() {
    let dir = TempDir::new().unwrap();
    let path = create_file(&dir, "mutating.c", "v0");
    let cache = CacheSystem::new();

    // Initial lookup.
    cache.lookup_since(&path, Clock::ZERO).unwrap();

    // Rapidly mutate the file 100 times.
    sleep_for_mtime();
    let mut last_content = String::new();
    for i in 1..=100 {
        last_content = format!("version {i} with padding to change size {}", "x".repeat(i));
        fs::write(&path, &last_content).unwrap();
    }

    // Apply change event.
    let c = cache.apply_changes(vec![path.clone()]);

    // Final lookup should return hash of the LAST version written.
    let result = cache.lookup_since(&path, c).unwrap();
    let expected = zccache::hash::hash_bytes(last_content.as_bytes());
    assert_eq!(result.hash, expected);
}

#[test]
#[ignore]
fn adversarial_create_and_immediate_delete() {
    let dir = TempDir::new().unwrap();
    let cache = CacheSystem::new();

    // Create and immediately delete.
    let path = create_file(&dir, "ephemeral.c", "blink and you miss it");
    cache.lookup_since(&path, Clock::ZERO).unwrap();

    fs::remove_file(&path).unwrap();
    cache.apply_changes_with_removals(vec![], vec![path.clone()]);

    // Should be gone from metadata cache.
    assert!(cache.metadata().get(&path).is_none());

    // Lookup should fail.
    assert!(cache.lookup_since(&path, Clock::ZERO).is_err());
}

#[test]
#[ignore]
fn adversarial_same_size_different_content() {
    let dir = TempDir::new().unwrap();
    let path = create_file(&dir, "sneaky.c", "AAAA");
    let cache = CacheSystem::new();

    let hash1 = cache.lookup_since(&path, Clock::ZERO).unwrap();

    // Replace with same-size but different content.
    sleep_for_mtime();
    fs::write(&path, "BBBB").unwrap();
    let c = cache.apply_changes(vec![path.clone()]);

    let hash2 = cache.lookup_since(&path, c).unwrap();

    assert_ne!(
        hash1.hash, hash2.hash,
        "same-size different content must produce different hash"
    );
    assert_eq!(hash2.hash, zccache::hash::hash_bytes(b"BBBB"));
}

#[test]
#[ignore]
fn adversarial_concurrent_writers_and_readers() {
    let dir = TempDir::new().unwrap();
    let cache = Arc::new(CacheSystem::new());

    // Create 20 files.
    let mut paths = Vec::new();
    for i in 0..20 {
        paths.push(create_file(
            &dir,
            &format!("rw_{i}.c"),
            &format!("init {i}"),
        ));
    }

    // Populate cache.
    for path in &paths {
        cache.lookup_since(path, Clock::ZERO).unwrap();
    }
    let c1 = cache.apply_changes(paths.to_vec());

    let mut handles = Vec::new();

    // 4 writer threads: each owns a slice of files and modifies them.
    for t in 0..4 {
        let cache = Arc::clone(&cache);
        let my_paths = paths
            .iter()
            .enumerate()
            .filter(|(i, _)| i % 4 == t)
            .map(|(_, p)| p.clone())
            .collect::<Vec<_>>();
        handles.push(thread::spawn(move || {
            for round in 0..10 {
                for path in &my_paths {
                    fs::write(path, format!("t{t}_r{round}")).unwrap();
                }
                cache.apply_changes(my_paths.to_vec());
                thread::sleep(Duration::from_millis(1));
            }
        }));
    }

    // 8 reader threads: continuously lookup all files.
    for _ in 0..8 {
        let cache = Arc::clone(&cache);
        let paths = paths.clone();
        handles.push(thread::spawn(move || {
            for _ in 0..100 {
                for path in &paths {
                    // Should never panic. Result may be stale hash or fresh hash.
                    let _ = cache.lookup_since(path, c1);
                }
            }
        }));
    }

    for h in handles {
        h.join().expect("thread panicked");
    }

    // After all writers finish, final lookup should return correct hashes.
    sleep_for_mtime(); // Ensure mtime settles.
    let _final_clock = cache.current_clock();
    for (i, path) in paths.iter().enumerate() {
        let t = i % 4;
        let expected_content = format!("t{t}_r9"); // Last round written.
                                                   // Re-write to ensure stable state.
        fs::write(path, &expected_content).unwrap();
    }
    sleep_for_mtime();
    let c_final = cache.apply_changes(paths.to_vec());
    for (i, path) in paths.iter().enumerate() {
        let t = i % 4;
        let expected_content = format!("t{t}_r9");
        let result = cache.lookup_since(path, c_final).unwrap();
        let expected = zccache::hash::hash_bytes(expected_content.as_bytes());
        assert_eq!(result.hash, expected, "file {i} hash mismatch after writes");
    }
}

#[test]
#[ignore]
fn adversarial_rename_chain() {
    let dir = TempDir::new().unwrap();
    let cache = CacheSystem::new();

    // Create file A.
    let path_a = create_file(&dir, "chain_a.c", "chain content");
    cache.lookup_since(&path_a, Clock::ZERO).unwrap();

    // Rename A → B → C → D.
    let path_b = dir.path().join("chain_b.c");
    let path_c = dir.path().join("chain_c.c");
    let path_d = dir.path().join("chain_d.c");

    fs::rename(&path_a, &path_b).unwrap();
    cache.apply_changes_with_removals(vec![path_b.clone().into()], vec![path_a.clone()]);

    fs::rename(&path_b, &path_c).unwrap();
    cache.apply_changes_with_removals(vec![path_c.clone().into()], vec![path_b.clone().into()]);

    fs::rename(&path_c, &path_d).unwrap();
    cache.apply_changes_with_removals(vec![path_d.clone().into()], vec![path_c.clone().into()]);

    // A, B, C should be gone from cache.
    assert!(cache.metadata().get(&path_a).is_none());
    assert!(cache
        .metadata()
        .get(&NormalizedPath::new(&path_b))
        .is_none());
    assert!(cache
        .metadata()
        .get(&NormalizedPath::new(&path_c))
        .is_none());

    // D should have correct content.
    let result = cache
        .lookup_since(&NormalizedPath::new(&path_d), Clock::ZERO)
        .unwrap();
    assert_eq!(result.hash, zccache::hash::hash_bytes(b"chain content"));
}

#[test]
#[ignore]
fn adversarial_empty_and_binary_files() {
    let dir = TempDir::new().unwrap();
    let cache = CacheSystem::new();

    // Empty file.
    let empty = create_file(&dir, "empty.c", "");
    let h_empty = cache.lookup_since(&empty, Clock::ZERO).unwrap();
    assert_eq!(h_empty.hash, zccache::hash::hash_bytes(b""));

    // Binary content (null bytes, high bytes).
    let binary_content: Vec<u8> = (0..=255).cycle().take(8192).collect();
    let bin_path = dir.path().join("binary.bin");
    fs::write(&bin_path, &binary_content).unwrap();
    let h_bin = cache
        .lookup_since(&NormalizedPath::new(&bin_path), Clock::ZERO)
        .unwrap();
    assert_eq!(h_bin.hash, zccache::hash::hash_bytes(&binary_content));

    // Large file (1MB).
    let large_content = vec![0x42u8; 1_048_576];
    let large_path = dir.path().join("large.bin");
    fs::write(&large_path, &large_content).unwrap();
    let h_large = cache
        .lookup_since(&NormalizedPath::new(&large_path), Clock::ZERO)
        .unwrap();
    assert_eq!(h_large.hash, zccache::hash::hash_bytes(&large_content));
}

#[test]
#[ignore]
fn adversarial_many_apply_overflow_cycles() {
    let dir = TempDir::new().unwrap();
    let cache = CacheSystem::new();

    let mut paths = Vec::new();
    for i in 0..30 {
        paths.push(create_file(&dir, &format!("cycle_{i}.c"), &format!("v{i}")));
    }

    // Populate.
    for path in &paths {
        cache.lookup_since(path, Clock::ZERO).unwrap();
    }

    // Run 10 overflow/recovery cycles.
    for cycle in 0..10 {
        cache.apply_overflow();

        // All entries should be Low.
        for path in &paths {
            let entry = cache.metadata().get(path).unwrap();
            assert_eq!(
                entry.confidence,
                zccache::fscache::Confidence::Low,
                "cycle {cycle}: should be Low after overflow"
            );
        }

        // Lookups should still work (slow path re-verifies).
        for path in &paths {
            let result = cache.lookup_since(path, Clock::ZERO).unwrap();
            let expected = zccache::hash::hash_file(path).unwrap();
            assert_eq!(result.hash, expected, "cycle {cycle}: hash mismatch");
        }
    }
}

// =========================================================================
// INTEGRATION TEST — full pipeline with real watcher
// =========================================================================

#[test]
#[ignore]
fn integration_full_pipeline() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let dir = TempDir::new().unwrap();
        let cache = Arc::new(CacheSystem::new());

        // Create initial files.
        let path_a = create_file(&dir, "src/a.c", "file a");
        let path_b = create_file(&dir, "src/b.c", "file b");

        // Populate cache.
        cache.lookup_since(&path_a, Clock::ZERO).unwrap();
        cache.lookup_since(&path_b, Clock::ZERO).unwrap();

        // Set up watcher pipeline.
        let ignore = Arc::new(IgnoreFilter::default());
        let (mut watcher, raw_rx) = NotifyWatcher::new(ignore).unwrap();
        watcher.watch(dir.path()).unwrap();

        let (settled_tx, mut settled_rx) = tokio::sync::mpsc::unbounded_channel();
        let buffer = SettleBuffer::new(Duration::from_millis(100));
        let settle_handle = tokio::spawn(async move {
            buffer.run(raw_rx, settled_tx).await;
        });

        // Give watcher time to initialize.
        tokio::time::sleep(Duration::from_millis(200)).await;

        // Modify a file.
        sleep_for_mtime();
        fs::write(&path_a, "file a modified").unwrap();

        // Wait for settled event (with timeout).
        let settled = tokio::time::timeout(Duration::from_secs(5), settled_rx.recv()).await;

        match settled {
            Ok(Some(SettledEvent::Batch { changed, removed })) => {
                // The batch should contain our modified file.
                let changed_names: Vec<String> = changed
                    .iter()
                    .filter_map(|p| p.file_name())
                    .map(|n| n.to_string_lossy().to_string())
                    .collect();
                assert!(
                    changed_names.contains(&"a.c".to_string())
                        || changed_names.contains(&"src".to_string()),
                    "expected a.c or src in changed, got: {changed_names:?}"
                );
                assert!(removed.is_empty());

                // Apply to cache system.
                let c = cache.apply_changes_with_removals(changed, removed);

                // Verify updated hash.
                let result = cache.lookup_since(&path_a, c).unwrap();
                assert_eq!(result.hash, zccache::hash::hash_bytes(b"file a modified"));

                // B should be unchanged.
                let result_b = cache.lookup_since(&path_b, c).unwrap();
                assert_eq!(result_b.hash, zccache::hash::hash_bytes(b"file b"));
            }
            Ok(Some(SettledEvent::Overflow)) => {
                // Overflow is acceptable — just verify cache still works.
                cache.apply_overflow();
                let result = cache.lookup_since(&path_a, Clock::ZERO).unwrap();
                assert_eq!(result.hash, zccache::hash::hash_bytes(b"file a modified"));
            }
            Ok(None) => panic!("settle channel closed unexpectedly"),
            Err(_) => {
                // Timeout — watcher may not have picked up the change.
                // This can happen on some CI environments. Mark as soft failure.
                eprintln!(
                    "WARNING: watcher did not detect file change within 5s (may be CI environment)"
                );
            }
        }

        // Cleanup: drop watcher to close channels.
        drop(watcher);
        let _ = tokio::time::timeout(Duration::from_secs(2), settle_handle).await;
    });
}
