#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::panic_in_result_fn, clippy::unwrap_in_result)]

mod common;

use tempfile::TempDir;
use zccache::fingerprint::{walk_files, walk_files_glob};

// ── Basic patterns ───────────────────────────────────────────────

#[test]
fn recursive_include() {
    let dir = TempDir::new().unwrap();
    common::create_file(dir.path(), "src/a.rs", "r");
    common::create_file(dir.path(), "src/nested/b.rs", "r");
    common::create_file(dir.path(), "src/c.py", "p");
    common::create_file(dir.path(), "lib.rs", "r");

    let files = walk_files_glob(dir.path(), &["src/**/*.rs"], &[]).unwrap();
    let rels = common::rel_paths(&files);
    assert_eq!(rels.len(), 2);
    assert!(rels.contains(&"src/a.rs"));
    assert!(rels.contains(&"src/nested/b.rs"));
}

#[test]
fn exact_filename() {
    let dir = TempDir::new().unwrap();
    common::create_file(dir.path(), "Cargo.toml", "t");
    common::create_file(dir.path(), "src/lib.rs", "r");

    let files = walk_files_glob(dir.path(), &["Cargo.toml"], &[]).unwrap();
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].relative, "Cargo.toml");
}

#[test]
fn directory_scoped() {
    let dir = TempDir::new().unwrap();
    common::create_file(dir.path(), "src/a.rs", "ok");
    common::create_file(dir.path(), "tests/b.rs", "skip");

    let files = walk_files_glob(dir.path(), &["src/**"], &[]).unwrap();
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].relative, "src/a.rs");
}

#[test]
fn multiple_include_patterns() {
    let dir = TempDir::new().unwrap();
    common::create_file(dir.path(), "src/a.rs", "r");
    common::create_file(dir.path(), "Cargo.toml", "t");
    common::create_file(dir.path(), "README.md", "m");

    let files = walk_files_glob(dir.path(), &["src/**", "Cargo.toml"], &[]).unwrap();
    let rels = common::rel_paths(&files);
    assert_eq!(rels.len(), 2);
    assert!(rels.contains(&"Cargo.toml"));
    assert!(rels.contains(&"src/a.rs"));
}

#[test]
fn brace_alternation() {
    let dir = TempDir::new().unwrap();
    common::create_file(dir.path(), "a.rs", "r");
    common::create_file(dir.path(), "b.toml", "t");
    common::create_file(dir.path(), "c.py", "p");

    let files = walk_files_glob(dir.path(), &["*.{rs,toml}"], &[]).unwrap();
    let rels = common::rel_paths(&files);
    assert_eq!(rels.len(), 2);
    assert!(rels.contains(&"a.rs"));
    assert!(rels.contains(&"b.toml"));
}

#[test]
fn question_mark_wildcard() {
    let dir = TempDir::new().unwrap();
    common::create_file(dir.path(), "a.rs", "one");
    common::create_file(dir.path(), "ab.rs", "two");

    let files = walk_files_glob(dir.path(), &["?.rs"], &[]).unwrap();
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].relative, "a.rs");
}

// ── Exclude patterns ─────────────────────────────────────────────

#[test]
fn exclude_overrides_include() {
    let dir = TempDir::new().unwrap();
    common::create_file(dir.path(), "src/a.rs", "ok");
    common::create_file(dir.path(), "tests/b.rs", "skip");
    common::create_file(dir.path(), "benches/c.rs", "skip");

    let files = walk_files_glob(dir.path(), &["**/*.rs"], &["tests/**", "benches/**"]).unwrap();
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].relative, "src/a.rs");
}

#[test]
fn directory_short_circuit() {
    let dir = TempDir::new().unwrap();
    common::create_file(dir.path(), "src/a.rs", "ok");
    common::create_file(dir.path(), ".git/config", "skip");
    common::create_file(dir.path(), ".git/objects/ab/cd", "skip");
    common::create_file(dir.path(), "target/debug/main", "skip");

    let files = walk_files_glob(dir.path(), &[], &[".git/**", "target/**"]).unwrap();
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].relative, "src/a.rs");
}

#[test]
fn exclude_specific_file() {
    let dir = TempDir::new().unwrap();
    common::create_file(dir.path(), "Cargo.toml", "t");
    common::create_file(dir.path(), "Cargo.lock", "l");

    let files = walk_files_glob(dir.path(), &[], &["Cargo.lock"]).unwrap();
    let rels = common::rel_paths(&files);
    assert_eq!(rels.len(), 1);
    assert!(rels.contains(&"Cargo.toml"));
}

#[test]
fn nested_exclude_inside_include() {
    let dir = TempDir::new().unwrap();
    common::create_file(dir.path(), "src/lib.rs", "ok");
    common::create_file(dir.path(), "src/generated/auto.rs", "skip");

    let files = walk_files_glob(dir.path(), &["src/**"], &["src/generated/**"]).unwrap();
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].relative, "src/lib.rs");
}

// ── Empty / no match ─────────────────────────────────────────────

#[test]
fn empty_include_matches_all() {
    let dir = TempDir::new().unwrap();
    common::create_file(dir.path(), "a.rs", "r");
    common::create_file(dir.path(), "b.py", "p");

    let files = walk_files_glob(dir.path(), &[], &[]).unwrap();
    assert_eq!(files.len(), 2);
}

#[test]
fn no_matches_returns_empty() {
    let dir = TempDir::new().unwrap();
    common::create_file(dir.path(), "a.rs", "r");

    let files = walk_files_glob(dir.path(), &["*.xyz"], &[]).unwrap();
    assert!(files.is_empty());
}

#[test]
fn invalid_glob_pattern_returns_error() {
    let dir = TempDir::new().unwrap();
    common::create_file(dir.path(), "a.rs", "r");

    let result = walk_files_glob(dir.path(), &["[invalid"], &[]);
    assert!(result.is_err());
}

// ── Sorting and paths ────────────────────────────────────────────

#[test]
fn results_sorted() {
    let dir = TempDir::new().unwrap();
    common::create_file(dir.path(), "z.rs", "z");
    common::create_file(dir.path(), "a.rs", "a");
    common::create_file(dir.path(), "m.rs", "m");

    let files = walk_files_glob(dir.path(), &[], &[]).unwrap();
    let rels = common::rel_paths(&files);
    assert_eq!(rels, vec!["a.rs", "m.rs", "z.rs"]);
}

#[test]
fn forward_slash_normalization() {
    let dir = TempDir::new().unwrap();
    common::create_file(dir.path(), "src/nested/deep/a.rs", "r");

    let files = walk_files_glob(dir.path(), &["**/*.rs"], &[]).unwrap();
    assert_eq!(files[0].relative, "src/nested/deep/a.rs");
    assert!(!files[0].relative.contains('\\'));
}

#[test]
fn absolute_paths_valid() {
    let dir = TempDir::new().unwrap();
    common::create_file(dir.path(), "a.rs", "r");

    let files = walk_files_glob(dir.path(), &[], &[]).unwrap();
    assert!(files[0].absolute.is_absolute());
    assert!(files[0].absolute.exists());
}

// ── Overlap / duplicates ─────────────────────────────────────────

#[test]
fn overlapping_patterns_no_dupes() {
    let dir = TempDir::new().unwrap();
    common::create_file(dir.path(), "src/a.rs", "r");
    common::create_file(dir.path(), "b.rs", "r");

    let files = walk_files_glob(dir.path(), &["**/*.rs", "src/**"], &[]).unwrap();
    // src/a.rs matches both patterns, but should appear only once.
    assert_eq!(files.len(), 2);
}

// ── Dotfiles and extensionless ───────────────────────────────────

#[test]
fn dotfiles_included() {
    let dir = TempDir::new().unwrap();
    common::create_file(dir.path(), ".hidden", "secret");
    common::create_file(dir.path(), ".env", "KEY=VAL");

    let files = walk_files_glob(dir.path(), &[], &[]).unwrap();
    assert_eq!(files.len(), 2);
}

#[test]
fn extensionless_files_matched() {
    let dir = TempDir::new().unwrap();
    common::create_file(dir.path(), "Makefile", "all:");
    common::create_file(dir.path(), "LICENSE", "MIT");

    let files = walk_files_glob(dir.path(), &["Makefile"], &[]).unwrap();
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].relative, "Makefile");
}

// ── Parity with walk_files ───────────────────────────────────────

#[test]
fn parity_with_walk_files() {
    let dir = TempDir::new().unwrap();
    common::create_file(dir.path(), "src/a.rs", "r");
    common::create_file(dir.path(), "src/b.py", "p");
    common::create_file(dir.path(), ".git/config", "nope");
    common::create_file(dir.path(), "lib/c.rs", "r");

    let from_walk = walk_files(dir.path(), &["rs"], &[".git"]).unwrap();
    let from_glob = walk_files_glob(dir.path(), &["**/*.rs"], &[".git/**"]).unwrap();

    let walk_rels: Vec<_> = from_walk.iter().map(|f| &f.relative).collect();
    let glob_rels: Vec<_> = from_glob.iter().map(|f| &f.relative).collect();
    assert_eq!(walk_rels, glob_rels);
}

// ── Error cases ──────────────────────────────────────────────────

#[test]
fn nonexistent_root_errors() {
    let dir = TempDir::new().unwrap();
    let bad = dir.path().join("nope");
    assert!(walk_files_glob(&bad, &[], &[]).is_err());
}
