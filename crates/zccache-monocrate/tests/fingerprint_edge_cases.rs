mod common;

use tempfile::TempDir;
use zccache_fingerprint::{
    walk_files, walk_files_glob, CacheDecision, HashCache, RunReason, TwoLayerCache,
};

// ── Unicode filenames ────────────────────────────────────────────

#[test]
fn unicode_filenames() {
    let dir = TempDir::new().unwrap();
    common::create_file(dir.path(), "café.rs", "french");
    common::create_file(dir.path(), "日本語.rs", "japanese");
    common::create_file(dir.path(), "normal.rs", "ascii");

    let files = walk_files(dir.path(), &["rs"], &[]).unwrap();
    assert_eq!(files.len(), 3);
}

#[test]
fn unicode_directory_names() {
    let dir = TempDir::new().unwrap();
    common::create_file(dir.path(), "données/a.rs", "data");
    common::create_file(dir.path(), "ソース/b.rs", "source");

    let files = walk_files(dir.path(), &[], &[]).unwrap();
    assert_eq!(files.len(), 2);
}

#[test]
fn unicode_with_hash_cache() {
    let (src, cache_dir) = (TempDir::new().unwrap(), TempDir::new().unwrap());
    common::create_file(src.path(), "café.rs", "content");

    let cache = HashCache::new(cache_dir.path().join("fp.json"));
    let files = walk_files(src.path(), &[], &[]).unwrap();

    cache.check(&files).unwrap();
    cache.mark_success().unwrap();

    let d = cache
        .check(&walk_files(src.path(), &[], &[]).unwrap())
        .unwrap();
    assert_eq!(d, CacheDecision::Skip);
}

// ── Spaces and special chars ─────────────────────────────────────

#[test]
fn files_with_spaces() {
    let dir = TempDir::new().unwrap();
    common::create_file(dir.path(), "my file.rs", "spaced");
    common::create_file(dir.path(), "dir with spaces/a.rs", "nested");

    let files = walk_files(dir.path(), &[], &[]).unwrap();
    assert_eq!(files.len(), 2);
}

#[test]
fn files_with_special_chars() {
    let dir = TempDir::new().unwrap();
    common::create_file(dir.path(), "hello-world.rs", "dash");
    common::create_file(dir.path(), "under_score.rs", "under");
    common::create_file(dir.path(), "dot.name.rs", "dot");

    let files = walk_files(dir.path(), &["rs"], &[]).unwrap();
    assert_eq!(files.len(), 3);
}

// ── Deeply nested paths ─────────────────────────────────────────

#[test]
fn very_long_relative_path() {
    let dir = TempDir::new().unwrap();
    let mut path = String::new();
    for i in 0..15 {
        if !path.is_empty() {
            path.push('/');
        }
        path.push_str(&format!("level_{i:02}"));
    }
    path.push_str("/deep.rs");

    common::create_file(dir.path(), &path, "deep");

    let files = walk_files(dir.path(), &[], &[]).unwrap();
    assert_eq!(files.len(), 1);
    assert!(files[0].relative.contains("level_14/deep.rs"));
}

// ── Empty directory tree ─────────────────────────────────────────

#[test]
fn empty_directory_tree() {
    let dir = TempDir::new().unwrap();
    std::fs::create_dir_all(dir.path().join("a/b/c")).unwrap();
    std::fs::create_dir_all(dir.path().join("d/e")).unwrap();

    let files = walk_files(dir.path(), &[], &[]).unwrap();
    assert!(files.is_empty());
}

#[test]
fn single_file_at_root() {
    let dir = TempDir::new().unwrap();
    common::create_file(dir.path(), "only.rs", "solo");

    let files = walk_files(dir.path(), &[], &[]).unwrap();
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].relative, "only.rs");
}

// ── Glob with unicode and special ────────────────────────────────

#[test]
fn glob_unicode_filenames() {
    let dir = TempDir::new().unwrap();
    common::create_file(dir.path(), "café.rs", "french");
    common::create_file(dir.path(), "normal.rs", "ascii");

    let files = walk_files_glob(dir.path(), &["**/*.rs"], &[]).unwrap();
    assert_eq!(files.len(), 2);
}

#[test]
fn glob_with_spaces_in_path() {
    let dir = TempDir::new().unwrap();
    common::create_file(dir.path(), "my dir/a.rs", "r");
    common::create_file(dir.path(), "normal/b.rs", "r");

    let files = walk_files_glob(dir.path(), &["**/*.rs"], &[]).unwrap();
    assert_eq!(files.len(), 2);
}

// ── Cache with edge-case files ───────────────────────────────────

#[test]
fn two_layer_with_many_small_files() {
    let (src, cache_dir) = (TempDir::new().unwrap(), TempDir::new().unwrap());
    for i in 0..50 {
        common::create_file(src.path(), &format!("f{i:03}.rs"), &format!("{i}"));
    }

    let cache = TwoLayerCache::new(cache_dir.path().join("fp.json"));
    cache
        .check(&walk_files(src.path(), &[], &[]).unwrap())
        .unwrap();
    cache.mark_success().unwrap();

    let d = cache
        .check(&walk_files(src.path(), &[], &[]).unwrap())
        .unwrap();
    assert_eq!(d, CacheDecision::Skip);
}

#[test]
fn cache_with_binary_content() {
    let (src, cache_dir) = (TempDir::new().unwrap(), TempDir::new().unwrap());
    let binary = src.path().join("data.bin");
    std::fs::write(&binary, [0u8, 1, 2, 255, 0, 128, 0, 0]).unwrap();

    let cache = HashCache::new(cache_dir.path().join("fp.json"));
    cache
        .check(&walk_files(src.path(), &[], &[]).unwrap())
        .unwrap();
    cache.mark_success().unwrap();

    let d = cache
        .check(&walk_files(src.path(), &[], &[]).unwrap())
        .unwrap();
    assert_eq!(d, CacheDecision::Skip);

    // Change binary content.
    std::fs::write(&binary, [255u8, 254, 253]).unwrap();
    let d = cache
        .check(&walk_files(src.path(), &[], &[]).unwrap())
        .unwrap();
    assert_eq!(d, CacheDecision::Run(RunReason::ContentChanged));
}

// ── Read-only cache directory ────────────────────────────────────

// Skipped: setting read-only on Windows is different and test cleanup
// would fail. This is better tested on CI with Unix.

// ── Symlinks ─────────────────────────────────────────────────────

// walk_files uses follow_links(false), so symlinks should NOT be followed.
// Creating symlinks on Windows requires elevated privileges, so we skip on Windows.

#[test]
#[cfg_attr(windows, ignore)]
fn symlink_to_file_not_followed() {
    let dir = TempDir::new().unwrap();
    common::create_file(dir.path(), "real.rs", "content");

    #[cfg(unix)]
    std::os::unix::fs::symlink(dir.path().join("real.rs"), dir.path().join("link.rs")).unwrap();

    let files = walk_files(dir.path(), &[], &[]).unwrap();
    // Only real.rs should appear (symlink not followed).
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].relative, "real.rs");
}

#[test]
#[cfg_attr(windows, ignore)]
fn symlink_to_directory_not_followed() {
    let dir = TempDir::new().unwrap();
    common::create_file(dir.path(), "real_dir/a.rs", "ok");

    #[cfg(unix)]
    std::os::unix::fs::symlink(dir.path().join("real_dir"), dir.path().join("linked_dir")).unwrap();

    let files = walk_files(dir.path(), &[], &[]).unwrap();
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].relative, "real_dir/a.rs");
}
