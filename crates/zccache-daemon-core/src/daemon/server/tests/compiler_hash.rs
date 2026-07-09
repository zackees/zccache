//! Tests for `CompilerHashCache` and the rustc compile-context builder.
//! These exercise the (path, mtime, size)-keyed compiler-hash cache that
//! avoids re-hashing the compiler binary on every request.

use super::super::*;
use std::time::SystemTime;

#[test]
fn compiler_hash_cache_reuses_hash_for_unchanged_compiler() {
    let tmp = tempfile::tempdir().unwrap();
    let compiler = tmp.path().join("rustc.exe");
    std::fs::write(&compiler, b"fake rustc").unwrap();

    let cache = CompilerHashCache::new();
    let hash_calls = AtomicUsize::new(0);
    let first = cache.get_or_hash_with(&compiler, |_| {
        hash_calls.fetch_add(1, Ordering::Relaxed);
        Some(ContentHash::from_bytes([7; 32]))
    });
    let second = cache.get_or_hash_with(&compiler, |_| {
        hash_calls.fetch_add(1, Ordering::Relaxed);
        Some(ContentHash::from_bytes([9; 32]))
    });

    assert_eq!(first, Some(ContentHash::from_bytes([7; 32])));
    assert_eq!(second, first);
    assert_eq!(hash_calls.load(Ordering::Relaxed), 1);
}

#[test]
fn compiler_hash_cache_rehashes_when_compiler_metadata_changes() {
    let tmp = tempfile::tempdir().unwrap();
    let compiler = tmp.path().join("rustc.exe");
    std::fs::write(&compiler, b"fake rustc").unwrap();
    filetime::set_file_mtime(
        &compiler,
        filetime::FileTime::from_unix_time(1_000_000_000, 0),
    )
    .unwrap();

    let cache = CompilerHashCache::new();
    let hash_calls = AtomicUsize::new(0);
    let first = cache.get_or_hash_with(&compiler, |_| {
        hash_calls.fetch_add(1, Ordering::Relaxed);
        Some(ContentHash::from_bytes([1; 32]))
    });

    std::fs::write(&compiler, b"fake rustc changed").unwrap();
    filetime::set_file_mtime(
        &compiler,
        filetime::FileTime::from_unix_time(1_000_000_010, 0),
    )
    .unwrap();

    let second = cache.get_or_hash_with(&compiler, |_| {
        hash_calls.fetch_add(1, Ordering::Relaxed);
        Some(ContentHash::from_bytes([2; 32]))
    });

    assert_eq!(first, Some(ContentHash::from_bytes([1; 32])));
    assert_eq!(second, Some(ContentHash::from_bytes([2; 32])));
    assert_eq!(hash_calls.load(Ordering::Relaxed), 2);
}

#[test]
fn rustc_context_build_reuses_compiler_hash_cache() {
    let tmp = tempfile::tempdir().unwrap();
    let compiler = tmp.path().join("rustc.exe");
    let source = tmp.path().join("lib.rs");
    let output = tmp.path().join("libunit.rmeta");
    std::fs::write(&compiler, b"fake rustc").unwrap();
    std::fs::write(&source, b"pub fn unit() {}").unwrap();

    let args: Vec<String> = vec![
        "--crate-name".into(),
        "unit".into(),
        "--edition".into(),
        "2021".into(),
        "--emit=dep-info,metadata".into(),
        source.to_string_lossy().into_owned(),
        "-o".into(),
        output.to_string_lossy().into_owned(),
    ];
    let compilation = crate::compiler::CacheableCompilation {
        compiler: compiler.clone().into(),
        family: crate::compiler::CompilerFamily::Rustc,
        source_file: source.clone().into(),
        output_file: output.into(),
        original_args: std::sync::Arc::from(args),
        unknown_flags: Vec::new(),
    };
    let cache = CompilerHashCache::new();
    let expected_hash = crate::hash::hash_file(&compiler).ok();

    let first = build_rustc_compile_context(&compilation, tmp.path(), &[], &cache);
    let second = build_rustc_compile_context(&compilation, tmp.path(), &[], &cache);

    let first_hash = match first {
        BuildContextResult::Rustc { rustc_ctx, .. } => rustc_ctx.compiler_hash,
        BuildContextResult::Cc { .. } => panic!("expected rustc context"),
    };
    let second_hash = match second {
        BuildContextResult::Rustc { rustc_ctx, .. } => rustc_ctx.compiler_hash,
        BuildContextResult::Cc { .. } => panic!("expected rustc context"),
    };
    assert_eq!(first_hash, expected_hash);
    assert_eq!(second_hash, expected_hash);
    assert_eq!(cache.len(), 1);
}

// ── Issue #517 Option 3: rustc -vV identity instead of binary hash ─────

/// `hash_rustc_identity` against a real rustc on PATH. Skips on hosts
/// without rustc (e.g. minimal CI containers); the assertion only
/// fires when we can compare against a known-good `-vV` output.
#[test]
fn hash_rustc_identity_matches_rustc_vv_output_when_rustc_available() {
    let Some(rustc) = crate::test_support::find_rustc() else {
        eprintln!("SKIP: rustc not found on PATH");
        return;
    };

    let identity = hash_rustc_identity(rustc.as_path()).expect("rustc -vV must produce a hash");

    // Recompute the expected hash from rustc's own -vV output. If
    // production code matches, identity == expected.
    let output = std::process::Command::new(rustc.as_path())
        .arg("-vV")
        .output()
        .expect("rustc -vV must spawn");
    assert!(output.status.success());
    let expected = crate::hash::hash_bytes(&output.stdout);

    assert_eq!(identity, expected);
    // And it must NOT match the full-binary hash — that's the whole
    // point: the version-string hash is the cheaper alternative.
    let binary_hash = crate::hash::hash_file(rustc.as_path()).ok();
    assert_ne!(Some(identity), binary_hash);
}

/// Stubbed binary that can't be spawned falls back to the file-content
/// hash so cache keys remain well-defined for tests and broken
/// toolchains.
#[test]
fn hash_rustc_identity_falls_back_to_file_hash_when_spawn_fails() {
    let tmp = tempfile::tempdir().unwrap();
    let fake_rustc = tmp.path().join("not-actually-rustc");
    std::fs::write(&fake_rustc, b"").unwrap();

    let identity = hash_rustc_identity(&fake_rustc);
    let file_hash = crate::hash::hash_file(&fake_rustc).ok();

    assert_eq!(identity, file_hash);
}

// ── Issue #517: persisted compiler hash cache ───────────────────────────

#[test]
fn compiler_hash_cache_save_then_load_roundtrip_preserves_entries() {
    let tmp = tempfile::tempdir().unwrap();
    let compiler = tmp.path().join("rustc.exe");
    let snapshot = tmp.path().join("compiler_hash.bin");
    std::fs::write(&compiler, b"fake rustc").unwrap();

    let cache = CompilerHashCache::new();
    let primed = cache.get_or_hash_with(&compiler, |_| Some(ContentHash::from_bytes([42; 32])));
    assert_eq!(primed, Some(ContentHash::from_bytes([42; 32])));
    cache.save_to_disk(&snapshot).unwrap();
    assert!(snapshot.exists(), "save_to_disk must produce a file");

    let restored = CompilerHashCache::load_from_disk(&snapshot).unwrap();
    let hash_calls = AtomicUsize::new(0);
    let from_restored = restored.get_or_hash_with(&compiler, |_| {
        hash_calls.fetch_add(1, Ordering::Relaxed);
        Some(ContentHash::from_bytes([99; 32]))
    });
    assert_eq!(from_restored, Some(ContentHash::from_bytes([42; 32])));
    assert_eq!(
        hash_calls.load(Ordering::Relaxed),
        0,
        "loaded snapshot must short-circuit the hasher when stat is unchanged",
    );
}

#[test]
fn compiler_hash_cache_load_missing_file_returns_empty() {
    let tmp = tempfile::tempdir().unwrap();
    let missing = tmp.path().join("does-not-exist.bin");

    let cache = CompilerHashCache::load_from_disk(&missing).unwrap();
    assert_eq!(cache.len(), 0);
}

#[test]
fn compiler_hash_cache_save_empty_cache_does_not_create_file() {
    let tmp = tempfile::tempdir().unwrap();
    let snapshot = tmp.path().join("compiler_hash.bin");

    let cache = CompilerHashCache::new();
    cache.save_to_disk(&snapshot).unwrap();

    assert!(
        !snapshot.exists(),
        "empty cache must not write a zero-entry file",
    );
}

#[test]
fn compiler_hash_cache_load_rehashes_when_binary_changes_after_save() {
    // Safety net: a stale snapshot must NEVER substitute for a real hash
    // of a changed binary. `get_or_hash_with` already enforces this via
    // its (mtime, size) stat-verify; this test pins it down so future
    // refactors of the persist code can't drop the guard.
    let tmp = tempfile::tempdir().unwrap();
    let compiler = tmp.path().join("rustc.exe");
    let snapshot = tmp.path().join("compiler_hash.bin");
    std::fs::write(&compiler, b"original rustc").unwrap();
    filetime::set_file_mtime(
        &compiler,
        filetime::FileTime::from_unix_time(1_000_000_000, 0),
    )
    .unwrap();

    let cache = CompilerHashCache::new();
    cache.get_or_hash_with(&compiler, |_| Some(ContentHash::from_bytes([1; 32])));
    cache.save_to_disk(&snapshot).unwrap();

    std::fs::write(&compiler, b"changed rustc binary").unwrap();
    filetime::set_file_mtime(
        &compiler,
        filetime::FileTime::from_unix_time(1_000_000_500, 0),
    )
    .unwrap();

    let restored = CompilerHashCache::load_from_disk(&snapshot).unwrap();
    let hash_calls = AtomicUsize::new(0);
    let observed = restored.get_or_hash_with(&compiler, |_| {
        hash_calls.fetch_add(1, Ordering::Relaxed);
        Some(ContentHash::from_bytes([7; 32]))
    });
    assert_eq!(
        observed,
        Some(ContentHash::from_bytes([7; 32])),
        "stale snapshot must not survive a binary change",
    );
    assert_eq!(hash_calls.load(Ordering::Relaxed), 1);
}

// ── Issue #784: deferred compiler-hash-cache load ─────────────────────────

/// `bind_with_cache_dir` no longer reads the compiler-hash-cache snapshot
/// from disk. The cache starts empty regardless of what is on disk; the
/// daemon binary's `compiler_hash_cache_loader().load_and_install()` does
/// the merge after the readiness lockfile is written.
#[tokio::test]
async fn bind_does_not_load_compiler_hash_cache_from_disk() {
    let tmp = tempfile::tempdir().unwrap();
    let cache_dir = crate::core::NormalizedPath::new(tmp.path());
    let snapshot_path = crate::core::config::compiler_hash_cache_path_from_cache_dir(&cache_dir);

    // Pre-write a snapshot with two entries.
    let pre = CompilerHashCache::new();
    let compiler_a = tmp.path().join("rustc-a.exe");
    let compiler_b = tmp.path().join("rustc-b.exe");
    std::fs::write(&compiler_a, b"fake rustc a").unwrap();
    std::fs::write(&compiler_b, b"fake rustc b").unwrap();
    pre.get_or_hash_with(&compiler_a, |_| Some(ContentHash::from_bytes([0xAA; 32])));
    pre.get_or_hash_with(&compiler_b, |_| Some(ContentHash::from_bytes([0xBB; 32])));
    pre.save_to_disk(snapshot_path.as_path()).unwrap();
    assert!(snapshot_path.as_path().exists());

    // Bind — must NOT read the snapshot (the load is deferred to the
    // background loader fired post-lockfile by the daemon binary).
    let endpoint = crate::ipc::unique_test_endpoint();
    let server = DaemonServer::bind_with_cache_dir(&endpoint, &cache_dir).unwrap();
    let state = server.test_state();
    assert_eq!(
        state.compiler_hash_cache.len(),
        0,
        "bind must start with an empty compiler-hash cache",
    );
    assert!(
        !state.compiler_hash_cache_loaded.load(Ordering::Acquire),
        "the loaded flag must start false until the background loader runs",
    );
}

/// The `compiler_hash_cache_loader()` handle reads the snapshot and
/// merges it into the live cache. Confirms the deferred-load path
/// reaches functional parity with the old sync-in-bind path.
#[tokio::test]
async fn compiler_hash_cache_loader_merges_snapshot_into_live_cache() {
    let tmp = tempfile::tempdir().unwrap();
    let cache_dir = crate::core::NormalizedPath::new(tmp.path());
    let snapshot_path = crate::core::config::compiler_hash_cache_path_from_cache_dir(&cache_dir);

    let pre = CompilerHashCache::new();
    let compiler = tmp.path().join("rustc.exe");
    std::fs::write(&compiler, b"fake rustc").unwrap();
    pre.get_or_hash_with(&compiler, |_| Some(ContentHash::from_bytes([0x42; 32])));
    pre.save_to_disk(snapshot_path.as_path()).unwrap();

    let endpoint = crate::ipc::unique_test_endpoint();
    let server = DaemonServer::bind_with_cache_dir(&endpoint, &cache_dir).unwrap();

    // Run the loader synchronously (it's `spawn_blocking`-safe but
    // calling it inline in the test is equivalent for observability).
    server.compiler_hash_cache_loader().load_and_install();

    let state = server.test_state();
    assert_eq!(
        state.compiler_hash_cache.len(),
        1,
        "loader must merge the persisted entry into the live cache",
    );
    assert!(
        state.compiler_hash_cache_loaded.load(Ordering::Acquire),
        "loader must set compiler_hash_cache_loaded=true so shutdown save fires",
    );

    // The loaded entry must short-circuit the hasher on next lookup —
    // proving the (path, mtime, size) -> hash mapping survived the
    // bind → loader hop.
    let hash_calls = AtomicUsize::new(0);
    let observed = state.compiler_hash_cache.get_or_hash_with(&compiler, |_| {
        hash_calls.fetch_add(1, Ordering::Relaxed);
        Some(ContentHash::from_bytes([0x99; 32]))
    });
    assert_eq!(observed, Some(ContentHash::from_bytes([0x42; 32])));
    assert_eq!(
        hash_calls.load(Ordering::Relaxed),
        0,
        "loaded entry must hit, not re-hash",
    );
}

/// Missing on-disk snapshot is not an error: the loader logs a warning,
/// leaves the live cache empty, and still flips the loaded flag so
/// shutdown save fires (and short-circuits because the cache is empty
/// per `save_to_disk`'s empty-cache early-exit).
#[tokio::test]
async fn compiler_hash_cache_loader_tolerates_missing_snapshot() {
    let tmp = tempfile::tempdir().unwrap();
    let cache_dir = crate::core::NormalizedPath::new(tmp.path());
    let endpoint = crate::ipc::unique_test_endpoint();
    let server = DaemonServer::bind_with_cache_dir(&endpoint, &cache_dir).unwrap();

    server.compiler_hash_cache_loader().load_and_install();

    let state = server.test_state();
    assert_eq!(state.compiler_hash_cache.len(), 0);
    assert!(
        state.compiler_hash_cache_loaded.load(Ordering::Acquire),
        "loaded flag flips even when the snapshot was absent",
    );
}

/// `merge_from` is `&self`, takes ownership of the loaded cache, and
/// drains all entries. Documents the contract the deferred loader
/// relies on.
#[test]
fn compiler_hash_cache_merge_from_drains_other() {
    let tmp = tempfile::tempdir().unwrap();
    let a = tmp.path().join("a");
    let b = tmp.path().join("b");
    std::fs::write(&a, b"a").unwrap();
    std::fs::write(&b, b"b").unwrap();

    let live = CompilerHashCache::new();
    let mtime = SystemTime::now();
    live.entries.insert(
        crate::core::NormalizedPath::new(&a),
        CompilerHashEntry {
            mtime,
            size: 1,
            hash: ContentHash::from_bytes([1; 32]),
        },
    );

    let loaded = CompilerHashCache::new();
    loaded.entries.insert(
        crate::core::NormalizedPath::new(&b),
        CompilerHashEntry {
            mtime,
            size: 1,
            hash: ContentHash::from_bytes([2; 32]),
        },
    );

    live.merge_from(loaded);

    assert_eq!(live.entries.len(), 2);
    assert!(live
        .entries
        .contains_key(&crate::core::NormalizedPath::new(&a)));
    assert!(live
        .entries
        .contains_key(&crate::core::NormalizedPath::new(&b)));
}
