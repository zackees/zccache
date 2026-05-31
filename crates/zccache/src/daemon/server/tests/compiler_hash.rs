//! Tests for `CompilerHashCache` and the rustc compile-context builder.
//! These exercise the (path, mtime, size)-keyed compiler-hash cache that
//! avoids re-hashing the compiler binary on every request.

use super::super::*;

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
fn prehash_paths_populates_cache_and_short_circuits_subsequent_lookups() {
    // Issue #517 Option 2: the daemon startup task warms the cache for
    // known compiler paths. After warmup, a real request that calls
    // `get_or_hash` on the same path MUST not re-hash — that's the whole
    // point of the warmup.
    let tmp = tempfile::tempdir().unwrap();
    let rustc = tmp.path().join("rustc");
    let cc = tmp.path().join("cc");
    let cxx = tmp.path().join("does-not-exist");
    std::fs::write(&rustc, b"fake rustc").unwrap();
    std::fs::write(&cc, b"fake cc").unwrap();

    let cache = CompilerHashCache::new();
    let hashed = prehash_paths(&cache, &[rustc.clone(), cc.clone(), cxx.clone()]);

    assert_eq!(
        hashed, 2,
        "the two existing paths should hash; missing one silently skipped"
    );
    assert_eq!(cache.len(), 2);

    // Re-hashing the same paths must hit the cache (no fresh blake3).
    let hash_calls = AtomicUsize::new(0);
    cache.get_or_hash_with(&rustc, |_| {
        hash_calls.fetch_add(1, Ordering::Relaxed);
        Some(ContentHash::from_bytes([0xff; 32]))
    });
    cache.get_or_hash_with(&cc, |_| {
        hash_calls.fetch_add(1, Ordering::Relaxed);
        Some(ContentHash::from_bytes([0xff; 32]))
    });
    assert_eq!(
        hash_calls.load(Ordering::Relaxed),
        0,
        "warmed entries must short-circuit the hasher",
    );
}

#[test]
fn prehash_candidates_from_env_filters_unset_and_empty() {
    use std::sync::Mutex;
    // serde_test uses lazy_static; we don't need that — just guard
    // against the parallel-test env-var race.
    static ENV_LOCK: Mutex<()> = Mutex::new(());
    let _guard = ENV_LOCK.lock().unwrap();

    // Stash + clear the three keys we manipulate so a prior test (or the
    // surrounding shell) does not bias the assertion.
    let saved: Vec<(&str, Option<std::ffi::OsString>)> = PREHASH_ENV_VARS
        .iter()
        .map(|k| (*k, std::env::var_os(k)))
        .collect();
    for (k, _) in &saved {
        std::env::remove_var(k);
    }

    std::env::set_var("RUSTC", "/opt/rustup/bin/rustc");
    std::env::set_var("CC", ""); // empty must be filtered
                                 // CXX intentionally unset

    let candidates = prehash_candidates_from_env();
    assert_eq!(
        candidates,
        vec![std::path::PathBuf::from("/opt/rustup/bin/rustc")]
    );

    // Restore prior state so we don't leak into other tests.
    for (k, v) in saved {
        match v {
            Some(value) => std::env::set_var(k, value),
            None => std::env::remove_var(k),
        }
    }
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
