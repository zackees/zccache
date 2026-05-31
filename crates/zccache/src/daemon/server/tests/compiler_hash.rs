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
