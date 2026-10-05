use super::*;
use std::fs;
use tempfile::TempDir;

fn create_file(dir: &Path, rel: &str, content: &str) {
    let path = dir.join(rel);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(&path, content).unwrap();
}

/// Put `path`'s modification time back to `mtime_ns` after a content change.
///
/// `File::set_times` is std and stable well below the 1.95 MSRV, so the test
/// needs no new dependency to reproduce the shape of edit that #1897 is about.
fn restore_mtime(path: &Path, mtime_ns: u64) {
    let file = fs::File::options().write(true).open(path).unwrap();
    let restored = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_nanos(mtime_ns);
    file.set_times(std::fs::FileTimes::new().set_modified(restored))
        .unwrap();
}

#[test]
fn first_check_returns_run() {
    let src = TempDir::new().unwrap();
    let cache_dir = TempDir::new().unwrap();
    create_file(src.path(), "a.rs", "fn main() {}");

    let mgr = FingerprintManager::new();
    let result = mgr.check(
        &cache_dir.path().join("fp.json"),
        "two-layer",
        src.path(),
        &[],
        &[],
        &[],
    );
    assert_eq!(result.decision, "run");
    assert_eq!(result.reason.as_deref(), Some("no cache file"));
}

#[test]
fn check_then_mark_success_then_check_returns_skip() {
    let src = TempDir::new().unwrap();
    let cache_dir = TempDir::new().unwrap();
    create_file(src.path(), "a.rs", "fn main() {}");

    let cache_file = cache_dir.path().join("fp.json");
    let mgr = FingerprintManager::new();

    // First check: run (no cache).
    let result = mgr.check(&cache_file, "two-layer", src.path(), &[], &[], &[]);
    assert_eq!(result.decision, "run");

    // Mark success.
    mgr.mark_success(&cache_file);

    // Second check: skip (clean).
    let result = mgr.check(&cache_file, "two-layer", src.path(), &[], &[], &[]);
    assert_eq!(result.decision, "skip");
}

#[test]
fn on_batch_changed_sets_dirty() {
    let src = TempDir::new().unwrap();
    let cache_dir = TempDir::new().unwrap();
    create_file(src.path(), "a.rs", "original");

    let cache_file = cache_dir.path().join("fp.json");
    let mgr = FingerprintManager::new();

    mgr.check(&cache_file, "two-layer", src.path(), &[], &[], &[]);
    mgr.mark_success(&cache_file);

    // Modify the file on disk.
    std::thread::sleep(std::time::Duration::from_millis(50));
    create_file(src.path(), "a.rs", "modified");

    // Simulate watcher event.
    mgr.on_batch(&[src.path().join("a.rs").into()], &[]);

    // Check should return run (dirty).
    let result = mgr.check(&cache_file, "two-layer", src.path(), &[], &[], &[]);
    assert_eq!(result.decision, "run");
    assert_eq!(result.reason.as_deref(), Some("content changed"));
}

#[test]
fn on_batch_removed_sets_dirty() {
    let src = TempDir::new().unwrap();
    let cache_dir = TempDir::new().unwrap();
    create_file(src.path(), "a.rs", "content");
    create_file(src.path(), "b.rs", "content2");

    let cache_file = cache_dir.path().join("fp.json");
    let mgr = FingerprintManager::new();

    mgr.check(&cache_file, "two-layer", src.path(), &[], &[], &[]);
    mgr.mark_success(&cache_file);

    // Simulate watcher event for removed file.
    mgr.on_batch(&[], &[src.path().join("b.rs").into()]);

    let result = mgr.check(&cache_file, "two-layer", src.path(), &[], &[], &[]);
    assert_eq!(result.decision, "run");
}

#[test]
fn smart_touch_does_not_set_dirty() {
    let src = TempDir::new().unwrap();
    let cache_dir = TempDir::new().unwrap();
    create_file(src.path(), "a.rs", "stable");

    let cache_file = cache_dir.path().join("fp.json");
    let mgr = FingerprintManager::new();

    mgr.check(&cache_file, "two-layer", src.path(), &[], &[], &[]);
    mgr.mark_success(&cache_file);

    // "Touch" the file (rewrite same content).
    std::thread::sleep(std::time::Duration::from_millis(50));
    create_file(src.path(), "a.rs", "stable");

    // Simulate watcher event — content is the same, so dirty should NOT be set.
    mgr.on_batch(&[src.path().join("a.rs").into()], &[]);

    let result = mgr.check(&cache_file, "two-layer", src.path(), &[], &[], &[]);
    assert_eq!(result.decision, "skip");
}

#[test]
fn mark_failure_forces_rerun() {
    let src = TempDir::new().unwrap();
    let cache_dir = TempDir::new().unwrap();
    create_file(src.path(), "a.rs", "content");

    let cache_file = cache_dir.path().join("fp.json");
    let mgr = FingerprintManager::new();

    mgr.check(&cache_file, "two-layer", src.path(), &[], &[], &[]);
    mgr.mark_failure(&cache_file);

    let result = mgr.check(&cache_file, "two-layer", src.path(), &[], &[], &[]);
    assert_eq!(result.decision, "run");
    assert_eq!(result.reason.as_deref(), Some("previous failure"));
}

#[test]
fn invalidate_removes_watch() {
    let src = TempDir::new().unwrap();
    let cache_dir = TempDir::new().unwrap();
    create_file(src.path(), "a.rs", "content");

    let cache_file = cache_dir.path().join("fp.json");
    let mgr = FingerprintManager::new();

    mgr.check(&cache_file, "two-layer", src.path(), &[], &[], &[]);
    mgr.mark_success(&cache_file);
    assert_eq!(mgr.watch_count(), 1);

    mgr.invalidate(&cache_file);
    assert_eq!(mgr.watch_count(), 0);

    // Next check should do a fresh scan.
    let result = mgr.check(&cache_file, "two-layer", src.path(), &[], &[], &[]);
    assert_eq!(result.decision, "run");
    assert_eq!(result.reason.as_deref(), Some("no cache file"));
}

#[test]
fn unrelated_watcher_event_ignored() {
    let src = TempDir::new().unwrap();
    let cache_dir = TempDir::new().unwrap();
    create_file(src.path(), "a.rs", "content");

    let cache_file = cache_dir.path().join("fp.json");
    let mgr = FingerprintManager::new();

    mgr.check(&cache_file, "two-layer", src.path(), &[], &[], &[]);
    mgr.mark_success(&cache_file);

    // Event for a file outside the watched root.
    mgr.on_batch(&[NormalizedPath::from("/some/other/path.rs")], &[]);

    let result = mgr.check(&cache_file, "two-layer", src.path(), &[], &[], &[]);
    assert_eq!(result.decision, "skip");
}

// ── Bug regression tests ──────────────────────────────────

#[test]
fn bug_a_changed_files_reported_when_dirty() {
    // Bug A: changed_files was always empty even when on_batch detected changes.
    let src = TempDir::new().unwrap();
    let cache_dir = TempDir::new().unwrap();
    create_file(src.path(), "a.rs", "original");
    create_file(src.path(), "b.rs", "stable");

    let cache_file = cache_dir.path().join("fp.json");
    let mgr = FingerprintManager::new();

    mgr.check(&cache_file, "two-layer", src.path(), &[], &[], &[]);
    mgr.mark_success(&cache_file);

    // Modify only a.rs.
    std::thread::sleep(std::time::Duration::from_millis(50));
    create_file(src.path(), "a.rs", "modified");
    mgr.on_batch(&[src.path().join("a.rs").into()], &[]);

    let result = mgr.check(&cache_file, "two-layer", src.path(), &[], &[], &[]);
    assert_eq!(result.decision, "run");
    // Bug: changed_files was always empty.
    assert!(
        !result.changed_files.is_empty(),
        "changed_files must report which files changed, got empty"
    );
    assert!(
        result.changed_files.iter().any(|f| f.contains("a.rs")),
        "changed_files should contain a.rs, got {:?}",
        result.changed_files
    );
}

#[test]
fn bug_b_mark_success_does_not_swallow_concurrent_events() {
    // Bug B: on_batch between check and mark_success was silently lost.
    let src = TempDir::new().unwrap();
    let cache_dir = TempDir::new().unwrap();
    create_file(src.path(), "a.rs", "v1");

    let cache_file = cache_dir.path().join("fp.json");
    let mgr = FingerprintManager::new();

    mgr.check(&cache_file, "two-layer", src.path(), &[], &[], &[]);
    mgr.mark_success(&cache_file);

    // File changes → dirty.
    std::thread::sleep(std::time::Duration::from_millis(50));
    create_file(src.path(), "a.rs", "v2");
    mgr.on_batch(&[src.path().join("a.rs").into()], &[]);

    // check returns "run" (dirty).
    let result = mgr.check(&cache_file, "two-layer", src.path(), &[], &[], &[]);
    assert_eq!(result.decision, "run");

    // ANOTHER file changes between check and mark_success.
    std::thread::sleep(std::time::Duration::from_millis(50));
    create_file(src.path(), "a.rs", "v3");
    mgr.on_batch(&[src.path().join("a.rs").into()], &[]);

    // User marks the operation as successful (based on v2 state).
    mgr.mark_success(&cache_file);

    // Bug: mark_success cleared dirty unconditionally, so the v3 change is lost.
    // The next check MUST return "run" because v3 arrived after the check.
    let result = mgr.check(&cache_file, "two-layer", src.path(), &[], &[], &[]);
    assert_eq!(
        result.decision, "run",
        "events arriving between check and mark_success must not be lost"
    );
}

#[test]
fn bug_c_pending_status_does_not_rescan() {
    // Bug C: after initial check (status=pending), a second check without
    // mark_success fell through to a full rescan returning "no cache file".
    let src = TempDir::new().unwrap();
    let cache_dir = TempDir::new().unwrap();
    create_file(src.path(), "a.rs", "content");

    let cache_file = cache_dir.path().join("fp.json");
    let mgr = FingerprintManager::new();

    // Initial check → "run" (no cache file).
    let result = mgr.check(&cache_file, "two-layer", src.path(), &[], &[], &[]);
    assert_eq!(result.decision, "run");
    assert_eq!(result.reason.as_deref(), Some("no cache file"));

    // Second check without marking → should still say "run" but NOT "no cache file".
    let result = mgr.check(&cache_file, "two-layer", src.path(), &[], &[], &[]);
    assert_eq!(result.decision, "run");
    assert_ne!(
        result.reason.as_deref(),
        Some("no cache file"),
        "pending watch should not trigger a full rescan"
    );
}

#[test]
fn bug_d_non_canonical_root_breaks_on_batch() {
    // Bug D: on_batch receives absolute paths from watcher, but root can be
    // non-canonical (e.g. "." or "path/sub/.."). strip_prefix fails silently,
    // so watcher events are never matched and the watch never becomes dirty.
    let src = TempDir::new().unwrap();
    let cache_dir = TempDir::new().unwrap();
    fs::create_dir(src.path().join("sub")).unwrap();
    create_file(src.path(), "a.rs", "original");

    let cache_file = cache_dir.path().join("fp.json");
    let mgr = FingerprintManager::new();

    // Use non-canonical root: /tmp/xxx/sub/.. ≡ /tmp/xxx/
    let non_canonical_root = src.path().join("sub").join("..");
    mgr.check(&cache_file, "two-layer", &non_canonical_root, &[], &[], &[]);
    mgr.mark_success(&cache_file);

    // Modify file on disk.
    std::thread::sleep(std::time::Duration::from_millis(50));
    create_file(src.path(), "a.rs", "modified");

    // Watcher events use canonical paths (\\?\ stripped on Windows).
    let canonical_root = canon(src.path());
    mgr.on_batch(&[canonical_root.join("a.rs")], &[]);

    // Without fix: returns "skip" because on_batch couldn't strip the prefix.
    let result = mgr.check(&cache_file, "two-layer", &non_canonical_root, &[], &[], &[]);
    assert_eq!(
        result.decision, "run",
        "on_batch with canonical paths must work even when root was non-canonical"
    );
}

#[test]
fn bug_e_non_canonical_cache_file_breaks_mark_success() {
    // Bug E: mark_success/mark_failure/invalidate compare cache_file by path
    // equality. If check() and mark_success receive different representations
    // of the same path, they won't match.
    let src = TempDir::new().unwrap();
    let cache_dir = TempDir::new().unwrap();
    fs::create_dir(cache_dir.path().join("sub")).unwrap();
    create_file(src.path(), "a.rs", "content");

    let mgr = FingerprintManager::new();

    // check() with non-canonical cache_file.
    let non_canonical_cache = cache_dir.path().join("sub").join("..").join("fp.json");
    mgr.check(&non_canonical_cache, "two-layer", src.path(), &[], &[], &[]);

    // mark_success() with canonical cache_file.
    let canonical_cache = canon(cache_dir.path()).join("fp.json");
    mgr.mark_success(&canonical_cache);

    // Without fix: mark_success couldn't find the watch, so status is still "pending".
    let result = mgr.check(&non_canonical_cache, "two-layer", src.path(), &[], &[], &[]);
    assert_eq!(
        result.decision, "skip",
        "mark_success with canonical path must match watch created with non-canonical path"
    );
}

#[test]
fn verify_catches_missed_watcher_events() {
    // Regression test for BUGS.md: daemon fp check misses in-place edits
    // when watcher events are not delivered (no on_batch call).
    let src = TempDir::new().unwrap();
    let cache_dir = TempDir::new().unwrap();
    create_file(src.path(), "a.rs", "original");

    let cache_file = cache_dir.path().join("fp.json");
    let mgr = FingerprintManager::new();

    mgr.check(&cache_file, "two-layer", src.path(), &[], &[], &[]);
    mgr.mark_success(&cache_file);

    // Modify file WITHOUT calling on_batch (simulates missed watcher event).
    std::thread::sleep(std::time::Duration::from_millis(50));
    create_file(src.path(), "a.rs", "modified");

    // Must detect the change via filesystem verification.
    let result = mgr.check(&cache_file, "two-layer", src.path(), &[], &[], &[]);
    assert_eq!(
        result.decision, "run",
        "must detect change without on_batch"
    );
    assert!(
        result.changed_files.iter().any(|f| f.contains("a.rs")),
        "changed_files should contain a.rs, got {:?}",
        result.changed_files
    );
}

#[test]
fn verify_smart_touch_still_skips() {
    // Smart touch (same content, new mtime) should still return "skip".
    let src = TempDir::new().unwrap();
    let cache_dir = TempDir::new().unwrap();
    create_file(src.path(), "a.rs", "stable");

    let cache_file = cache_dir.path().join("fp.json");
    let mgr = FingerprintManager::new();

    mgr.check(&cache_file, "two-layer", src.path(), &[], &[], &[]);
    mgr.mark_success(&cache_file);

    // Rewrite same content (mtime changes, content doesn't).
    std::thread::sleep(std::time::Duration::from_millis(50));
    create_file(src.path(), "a.rs", "stable");

    // Filesystem verify should detect mtime change, re-hash, find same content → skip.
    let result = mgr.check(&cache_file, "two-layer", src.path(), &[], &[], &[]);
    assert_eq!(result.decision, "skip", "smart touch must not trigger run");
}

#[test]
fn replacement_preserving_mtime_is_detected() {
    // #1897: the file object at the path was replaced with an equally long one
    // and the mtime restored, so mtime+size were identical to the tracked state
    // and the check answered "skip" — a stale cache hit.
    let src = TempDir::new().unwrap();
    let cache_dir = TempDir::new().unwrap();
    create_file(src.path(), "a.rs", "aaaaaaaa");

    let path = src.path().join("a.rs");
    let original_mtime = crate::fingerprint::persist::mtime_ns(&path).unwrap();
    let original_id = crate::platform::fs::identity::file_identity(&path).ok();

    let cache_file = cache_dir.path().join("fp.json");
    let mgr = FingerprintManager::new();

    mgr.check(&cache_file, "two-layer", src.path(), &[], &[], &[]);
    mgr.mark_success(&cache_file);

    // Replace the object behind the path: same size, new file id. The
    // replacement is created *before* the unlink and renamed into place,
    // because a filesystem is free to hand the just-freed file id straight
    // back to the next create in the same directory — on Windows, where
    // `inode_change_ns` is 0, that reuse would silently degrade this test
    // back to the mtime-and-size comparison it exists to disprove.
    let replacement = src.path().join("replacement.rs");
    create_file(src.path(), "replacement.rs", "bbbbbbbb");
    fs::remove_file(&path).unwrap();
    fs::rename(&replacement, &path).unwrap();
    restore_mtime(&path, original_mtime);
    assert_ne!(
        crate::platform::fs::identity::file_identity(&path).ok(),
        original_id,
        "test precondition: the file identity must change"
    );

    let result = mgr.check(&cache_file, "two-layer", src.path(), &[], &[], &[]);
    assert_eq!(
        result.decision, "run",
        "a path replacement with a preserved mtime and size must not skip"
    );
    assert!(
        result.changed_files.iter().any(|f| f.contains("a.rs")),
        "changed_files should contain a.rs, got {:?}",
        result.changed_files
    );
}

#[cfg(unix)]
#[test]
fn content_change_preserving_mtime_and_size_is_detected() {
    // #1897: an in-place truncate+write leaves the inode/dev unchanged, so only
    // the Unix `ctime` (which `utimensat` cannot restore) distinguishes it from
    // an untouched file.
    let src = TempDir::new().unwrap();
    let cache_dir = TempDir::new().unwrap();
    create_file(src.path(), "a.rs", "aaaaaaaa");

    let path = src.path().join("a.rs");
    let original_mtime = crate::fingerprint::persist::mtime_ns(&path).unwrap();
    let original_id = crate::platform::fs::identity::file_identity(&path).ok();

    let cache_file = cache_dir.path().join("fp.json");
    let mgr = FingerprintManager::new();

    mgr.check(&cache_file, "two-layer", src.path(), &[], &[], &[]);
    mgr.mark_success(&cache_file);

    std::thread::sleep(std::time::Duration::from_millis(50));
    // Truncate + write: same inode, same size, new bytes.
    create_file(src.path(), "a.rs", "bbbbbbbb");
    restore_mtime(&path, original_mtime);
    assert_eq!(
        crate::platform::fs::identity::file_identity(&path).ok(),
        original_id,
        "test precondition: the inode must not change"
    );

    let result = mgr.check(&cache_file, "two-layer", src.path(), &[], &[], &[]);
    assert_eq!(
        result.decision, "run",
        "an in-place edit with a restored mtime must not skip"
    );
    assert!(
        result.changed_files.iter().any(|f| f.contains("a.rs")),
        "changed_files should contain a.rs, got {:?}",
        result.changed_files
    );
}

#[test]
fn two_watches_independent() {
    let src = TempDir::new().unwrap();
    let cache_dir = TempDir::new().unwrap();
    create_file(src.path(), "a.rs", "content");

    let cache1 = cache_dir.path().join("c1.json");
    let cache2 = cache_dir.path().join("c2.json");
    let mgr = FingerprintManager::new();

    // Initialize both watches.
    mgr.check(&cache1, "two-layer", src.path(), &[], &[], &[]);
    mgr.mark_success(&cache1);
    mgr.check(&cache2, "hash", src.path(), &[], &[], &[]);
    mgr.mark_success(&cache2);

    // Invalidate only cache1.
    mgr.invalidate(&cache1);

    // cache2 should still be clean.
    let r2 = mgr.check(&cache2, "hash", src.path(), &[], &[], &[]);
    assert_eq!(r2.decision, "skip");

    // cache1 should need a fresh scan.
    let r1 = mgr.check(&cache1, "two-layer", src.path(), &[], &[], &[]);
    assert_eq!(r1.decision, "run");
}

#[test]
fn on_batch_many_watches_completes_quickly() {
    // Regression test for issue #724: on_batch must not hold the watch-map lock
    // across per-file canonicalize/hash I/O. With hundreds of watch roots the
    // old implementation canonicalized every changed path once *per watch*
    // (O(n^2) syscalls) and hashed under a DashMap write-shard lock, starving
    // RPC handlers and wedging the daemon. The fix pre-computes path metadata
    // once per batch, so this large batch must finish well within budget.
    const ROOTS: usize = 200;

    let cache_dir = TempDir::new().unwrap();
    let mgr = FingerprintManager::new();
    let mut roots = Vec::with_capacity(ROOTS);
    let mut cache_files = Vec::with_capacity(ROOTS);

    for i in 0..ROOTS {
        let root = TempDir::new().unwrap();
        create_file(root.path(), "src.cpp", "original");
        let cache_file = cache_dir.path().join(format!("fp{i}.json"));
        mgr.check(&cache_file, "two-layer", root.path(), &[], &[], &[]);
        mgr.mark_success(&cache_file);
        cache_files.push(cache_file);
        roots.push(root);
    }

    // Modify every tracked file, then deliver one big watcher batch.
    std::thread::sleep(std::time::Duration::from_millis(50));
    let mut changed = Vec::with_capacity(ROOTS);
    for root in &roots {
        create_file(root.path(), "src.cpp", "modified");
        changed.push(canon(&root.path().join("src.cpp")));
    }

    let start = std::time::Instant::now();
    mgr.on_batch(&changed, &[]);
    let elapsed = start.elapsed();

    // Generous budget: the wedge made this effectively unbounded under lock
    // contention. The lock-free variant is orders of magnitude faster.
    assert!(
        elapsed < std::time::Duration::from_secs(10),
        "on_batch over {ROOTS} watches took {elapsed:?}, expected < 10s (issue #724 regression)"
    );

    // Correctness: every watch must now be dirty.
    for (i, cache_file) in cache_files.iter().enumerate() {
        let result = mgr.check(cache_file, "two-layer", roots[i].path(), &[], &[], &[]);
        assert_eq!(
            result.decision, "run",
            "watch {i} should be dirty after on_batch"
        );
    }
}

/// Regression test for issue #1908: `check`'s verification pass must not hold
/// a watch-map shard guard across its stat/blake3 window. The collect hook
/// pauses the verification thread *inside* that window; the main thread then
/// drives `mark_success`, whose `iter_mut` needs every shard's write guard.
/// If verification held any guard, this would block for the hook's full
/// timeout instead of completing immediately.
#[test]
fn verify_does_not_hold_the_write_shard_while_hashing() {
    use std::time::{Duration, Instant};

    let src = TempDir::new().unwrap();
    let cache_dir = TempDir::new().unwrap();
    create_file(src.path(), "src.cpp", "original");

    let cache_file = cache_dir.path().join("fp.json");
    let other_cache_file = cache_dir.path().join("other.json");
    let mgr = FingerprintManager::new();

    mgr.check(&cache_file, "two-layer", src.path(), &[], &[], &[]);
    mgr.mark_success(&cache_file);

    // Make the next check take the verification path (layer 1 fails: mtime/size).
    std::thread::sleep(Duration::from_millis(50));
    create_file(src.path(), "src.cpp", "modified");

    let (start_tx, start_rx) = std::sync::mpsc::channel::<()>();
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    // `Receiver` is `Send` but not `Sync`, and the hook must be `Fn + Send +
    // Sync`; the mutex bridges that without changing the protocol.
    let release_rx = std::sync::Mutex::new(release_rx);
    verify::set_collect_hook(move || {
        let _ = start_tx.send(());
        // Bounded so a failed assertion cannot wedge the whole test binary.
        if let Ok(release) = release_rx.lock() {
            let _ = release.recv_timeout(Duration::from_secs(10));
        }
    });

    let root = src.path().to_path_buf();
    let cache = cache_file.clone();
    let mut elapsed: Option<Duration> = None;
    std::thread::scope(|scope| {
        // Borrowed, not moved: the main thread below drives `mgr.mark_success`,
        // which is the whole point of the assertion.
        scope.spawn(|| {
            // Arm on this thread so no other test's collect pass can consume
            // the hook.
            verify::arm_collect_hook();
            mgr.check(&cache, "two-layer", &root, &[], &[], &[])
        });

        // The moment the old implementation is sitting on the shard with its
        // guard live.
        start_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("collect hook never fired");

        let t0 = Instant::now();
        mgr.mark_success(&other_cache_file);
        elapsed = Some(t0.elapsed());

        // Release before any assertion that can panic, so a failure cannot
        // deadlock the verification thread and hang the whole test binary.
        let _ = release_tx.send(());
    });

    let elapsed = elapsed.unwrap();

    assert!(
        elapsed < Duration::from_secs(2),
        "mark_success took {elapsed:?} while the verification hash was in flight; \
         check() is holding the watch-map write shard across filesystem I/O \
         (issue #1908 regression)"
    );
}

/// The collect/apply split must not change any verdict `check` used to return.
#[test]
fn verify_collect_and_apply_preserve_semantics() {
    // (i) Real content change is still reported as "run" with the rel listed.
    let src = TempDir::new().unwrap();
    let cache_dir = TempDir::new().unwrap();
    create_file(src.path(), "a.rs", "original");
    let cache_file = cache_dir.path().join("fp.json");
    let mgr = FingerprintManager::new();

    mgr.check(&cache_file, "two-layer", src.path(), &[], &[], &[]);
    mgr.mark_success(&cache_file);
    std::thread::sleep(std::time::Duration::from_millis(50));
    create_file(src.path(), "a.rs", "modified");

    let changed = mgr.check(&cache_file, "two-layer", src.path(), &[], &[], &[]);
    assert_eq!(changed.decision, "run");
    assert!(
        changed.changed_files.iter().any(|f| f.contains("a.rs")),
        "changed_files should contain a.rs, got {:?}",
        changed.changed_files
    );

    // (ii) A smart touch after the change is clean content → "skip". This only
    // holds if the apply phase of (i) wrote the post-change `hash_hex`/
    // `mtime_ns` back into `watch.files`: without that write the tracked hash
    // still holds the pre-change value, so layer 1 fails and layer 2 re-reports
    // a change that was already recorded.
    std::thread::sleep(std::time::Duration::from_millis(50));
    mgr.mark_success(&cache_file);
    create_file(src.path(), "a.rs", "modified");
    let touched = mgr.check(&cache_file, "two-layer", src.path(), &[], &[], &[]);
    assert_eq!(
        touched.decision, "skip",
        "apply must persist the new hash_hex/mtime_ns, else every later check \
         re-reports a change it already recorded"
    );

    // (iii) A second check immediately after a genuine change still says "run".
    std::thread::sleep(std::time::Duration::from_millis(50));
    create_file(src.path(), "a.rs", "modified again");
    let again = mgr.check(&cache_file, "two-layer", src.path(), &[], &[], &[]);
    assert_eq!(again.decision, "run");
    assert!(again.changed_files.iter().any(|f| f.contains("a.rs")));
}
