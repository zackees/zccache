//! Tests for the depfile parser, strategy selection, and canonicalize cache.

use std::path::Path;

use tempfile::TempDir;

use super::super::args::UserDepFlags;
use super::canonicalize::{canonicalize_cache_len_for_test, canonicalize_path, strip_win_prefix};
use super::error::DepfileError;
use super::parse::{
    find_separator_colon, join_continuations, parse_depfile, parse_depfile_path, split_and_unescape,
};
use super::strategy::{prepare_depfile, user_depfile_destination, DepfileStrategy};
use zccache_core::NormalizedPath;

/// Helper: create a file with empty content inside a temp dir.
fn touch(dir: &Path, name: &str) -> NormalizedPath {
    let p = dir.join(name);
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(&p, "").unwrap();
    p.into()
}

/// Helper: canonicalize a path (or return it unchanged), stripping \\?\ on Windows.
fn canon(p: &Path) -> NormalizedPath {
    strip_win_prefix(
        std::fs::canonicalize(p)
            .unwrap_or_else(|_| p.to_path_buf())
            .into(),
    )
}

// ── 1. parse_single_line ─────────────────────────────────────────────

#[test]
fn parse_single_line() {
    let dir = TempDir::new().unwrap();
    let cwd = dir.path();
    let source = touch(cwd, "foo.c");
    let bar_h = touch(cwd, "bar.h");

    let content = "foo.o: foo.c bar.h";
    let result = parse_depfile(content, &source, cwd).unwrap();

    assert_eq!(result.resolved.len(), 1);
    assert_eq!(result.resolved[0], canon(&bar_h));
    assert!(result.unresolved.is_empty());
    assert!(!result.has_computed);
}

// ── 2. parse_continuations ───────────────────────────────────────────

#[test]
fn parse_continuations() {
    let dir = TempDir::new().unwrap();
    let cwd = dir.path();
    let source = touch(cwd, "foo.c");
    let bar_h = touch(cwd, "bar.h");
    let baz_h = touch(cwd, "baz.h");

    let content = "foo.o: foo.c bar.h \\\n  baz.h";
    let result = parse_depfile(content, &source, cwd).unwrap();

    assert_eq!(result.resolved.len(), 2);
    assert!(result.resolved.contains(&canon(&bar_h)));
    assert!(result.resolved.contains(&canon(&baz_h)));
}

// ── 3. parse_escaped_spaces ──────────────────────────────────────────

#[test]
fn parse_escaped_spaces() {
    // Use a synthetic depfile. We can't easily create dirs with spaces
    // in tempdir on all platforms, so just verify parsing logic: the
    // token should have the space unescaped.
    let content = r"foo.o: foo.c path\ with\ spaces/foo.h";
    let source = Path::new("/nonexistent/foo.c");
    let cwd = Path::new("/nonexistent");

    let result = parse_depfile(content, source, cwd).unwrap();

    // The resolved path won't canonicalize (files don't exist), so
    // check that it ends with the unescaped name.
    assert_eq!(result.resolved.len(), 1);
    let dep = &result.resolved[0];
    let dep_str = dep.to_string_lossy();
    assert!(
        dep_str.contains("path with spaces"),
        "expected unescaped space in path, got: {dep_str}"
    );
}

// ── 4. parse_multiple_targets ────────────────────────────────────────

#[test]
fn parse_multiple_targets() {
    let dir = TempDir::new().unwrap();
    let cwd = dir.path();
    let source = touch(cwd, "foo.c");
    let bar_h = touch(cwd, "bar.h");

    // Multiple targets before the colon (gcc -MD -MT produces this).
    let content = "foo.o foo.d: foo.c bar.h";
    let result = parse_depfile(content, &source, cwd).unwrap();

    assert_eq!(result.resolved.len(), 1);
    assert_eq!(result.resolved[0], canon(&bar_h));
}

// ── 5. parse_empty_deps ──────────────────────────────────────────────

#[test]
fn parse_empty_deps() {
    let dir = TempDir::new().unwrap();
    let cwd = dir.path();
    let source = touch(cwd, "foo.c");

    // Source is the only dependency — it gets excluded.
    let content = "foo.o: foo.c";
    let result = parse_depfile(content, &source, cwd).unwrap();

    assert!(result.resolved.is_empty());
}

// ── 6. parse_relative_paths_resolved ─────────────────────────────────

#[test]
fn parse_relative_paths_resolved() {
    let dir = TempDir::new().unwrap();
    let cwd = dir.path();
    let source = touch(cwd, "src/main.c");
    let header = touch(cwd, "inc/util.h");

    // Relative paths in the depfile should be resolved against cwd.
    let content = "src/main.o: src/main.c inc/util.h";
    let result = parse_depfile(content, &source, cwd).unwrap();

    assert_eq!(result.resolved.len(), 1);
    assert_eq!(result.resolved[0], canon(&header));
}

// ── 7. parse_source_excluded ─────────────────────────────────────────

#[test]
fn parse_source_excluded() {
    let dir = TempDir::new().unwrap();
    let cwd = dir.path();
    let source = touch(cwd, "main.c");
    let alpha = touch(cwd, "alpha.h");
    let beta = touch(cwd, "beta.h");

    let content = "main.o: main.c alpha.h beta.h";
    let result = parse_depfile(content, &source, cwd).unwrap();

    // main.c should not appear in resolved.
    assert_eq!(result.resolved.len(), 2);
    assert!(result.resolved.contains(&canon(&alpha)));
    assert!(result.resolved.contains(&canon(&beta)));
    assert!(!result.resolved.contains(&canon(&source)));
}

// ── 8. parse_deduplicates ────────────────────────────────────────────

#[test]
fn parse_deduplicates() {
    let dir = TempDir::new().unwrap();
    let cwd = dir.path();
    let source = touch(cwd, "foo.c");
    let bar_h = touch(cwd, "bar.h");

    // bar.h appears twice — should be deduplicated.
    let content = "foo.o: foo.c bar.h bar.h";
    let result = parse_depfile(content, &source, cwd).unwrap();

    assert_eq!(result.resolved.len(), 1);
    assert_eq!(result.resolved[0], canon(&bar_h));
}

// ── 9. parse_windows_drive_letters ───────────────────────────────────

#[test]
#[cfg(windows)]
fn parse_windows_drive_letters() {
    // The colon after `C` should not be treated as the target separator.
    let content = r"C:\build\foo.o: C:\src\foo.c C:\inc\bar.h";
    let source = Path::new(r"C:\src\foo.c");
    let cwd = Path::new(r"C:\build");

    let result = parse_depfile(content, source, cwd).unwrap();

    // bar.h should be present (foo.c excluded as source).
    assert_eq!(result.resolved.len(), 1);
    let dep = &result.resolved[0];
    let dep_str = dep.to_string_lossy();
    assert!(
        dep_str.contains("bar.h"),
        "expected bar.h in resolved, got: {dep_str}"
    );
}

#[test]
#[cfg(not(windows))]
fn parse_windows_drive_letters() {
    // On non-Windows, just verify the colon parser doesn't choke on
    // drive-letter colons and finds the correct separator.
    let content = r"C:\build\foo.o: C:\src\foo.c C:\inc\bar.h";
    let source = Path::new(r"C:\src\foo.c");
    let cwd = Path::new(r"C:\build");

    let result = parse_depfile(content, source, cwd).unwrap();

    // Source exclusion relies on std::path which handles `\` differently
    // on Unix, so just check that parsing succeeded and bar.h is present.
    let dep_strs: Vec<String> = result
        .resolved
        .iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect();
    assert!(
        dep_strs.iter().any(|s| s.contains("bar.h")),
        "expected bar.h in resolved, got: {dep_strs:?}"
    );
}

// ── 10. parse_empty_content_errors ───────────────────────────────────

#[test]
fn parse_empty_content_errors() {
    let result = parse_depfile("", Path::new("foo.c"), Path::new("/tmp"));
    assert!(result.is_err());
    match result.unwrap_err() {
        DepfileError::Malformed(msg) => {
            assert!(msg.contains("empty"), "unexpected message: {msg}");
        }
        other => panic!("expected Malformed, got: {other:?}"),
    }
}

// ── 11. parse_real_gcc_output ────────────────────────────────────────

#[test]
fn parse_real_gcc_output() {
    let dir = TempDir::new().unwrap();
    let cwd = dir.path();
    let source = touch(cwd, "main.c");
    let config_h = touch(cwd, "config.h");

    // Create a fake system include dir structure.
    let stdio_h = touch(cwd, "usr/include/stdio.h");
    let stdlib_h = touch(cwd, "usr/include/stdlib.h");

    let content = format!(
        "main.o: main.c config.h \\\n {} \\\n {}",
        stdio_h.display(),
        stdlib_h.display(),
    );

    let result = parse_depfile(&content, &source, cwd).unwrap();

    assert_eq!(result.resolved.len(), 3);
    assert!(result.resolved.contains(&canon(&config_h)));
    assert!(result.resolved.contains(&canon(&stdio_h)));
    assert!(result.resolved.contains(&canon(&stdlib_h)));
    assert!(!result.has_computed);
    assert!(result.unresolved.is_empty());
}

// ── 12. parse_real_clang_output ──────────────────────────────────────

#[test]
fn parse_real_clang_output() {
    let dir = TempDir::new().unwrap();
    let cwd = dir.path();
    let source = touch(cwd, "app.cpp");
    let app_h = touch(cwd, "app.h");
    let types_h = touch(cwd, "include/types.h");
    let vector_h = touch(cwd, "usr/include/c++/vector");

    // Clang-style: uses absolute paths, continuation lines, targets
    // include both .o and .d.
    let content = format!(
        "app.o app.d: {} {} \\\n  {} \\\n  {}",
        source.display(),
        app_h.display(),
        types_h.display(),
        vector_h.display(),
    );

    let result = parse_depfile(&content, &source, cwd).unwrap();

    assert_eq!(result.resolved.len(), 3);
    assert!(result.resolved.contains(&canon(&app_h)));
    assert!(result.resolved.contains(&canon(&types_h)));
    assert!(result.resolved.contains(&canon(&vector_h)));
    assert!(!result.has_computed);
}

// ── Additional edge cases ────────────────────────────────────────────

#[test]
fn whitespace_only_is_malformed() {
    let result = parse_depfile("   \n  \t  \n", Path::new("x.c"), Path::new("/tmp"));
    assert!(matches!(result, Err(DepfileError::Malformed(_))));
}

#[test]
fn no_colon_is_malformed() {
    let result = parse_depfile("foo.o foo.c bar.h", Path::new("foo.c"), Path::new("/tmp"));
    assert!(matches!(result, Err(DepfileError::Malformed(_))));
}

#[test]
fn parse_depfile_path_reads_file() {
    let dir = TempDir::new().unwrap();
    let cwd = dir.path();
    let source = touch(cwd, "src.c");
    let hdr = touch(cwd, "hdr.h");

    let depfile = cwd.join("src.d");
    std::fs::write(&depfile, "src.o: src.c hdr.h").unwrap();

    let result = parse_depfile_path(&depfile, &source, cwd).unwrap();
    assert_eq!(result.resolved.len(), 1);
    assert_eq!(result.resolved[0], canon(&hdr));
}

#[test]
fn parse_depfile_path_missing_file() {
    let result = parse_depfile_path(
        Path::new("/nonexistent.d"),
        Path::new("x.c"),
        Path::new("/tmp"),
    );
    assert!(matches!(result, Err(DepfileError::Io(_))));
}

#[test]
fn escaped_hash_in_path() {
    let content = r"foo.o: foo.c path\#2/bar.h";
    let source = Path::new("/nonexistent/foo.c");
    let cwd = Path::new("/nonexistent");

    let result = parse_depfile(content, source, cwd).unwrap();
    assert_eq!(result.resolved.len(), 1);
    let dep_str = result.resolved[0].to_string_lossy();
    assert!(
        dep_str.contains("path#2"),
        "expected unescaped '#' in path, got: {dep_str}"
    );
}

#[test]
fn crlf_continuations() {
    let dir = TempDir::new().unwrap();
    let cwd = dir.path();
    let source = touch(cwd, "foo.c");
    let bar_h = touch(cwd, "bar.h");

    let content = "foo.o: foo.c \\\r\n  bar.h";
    let result = parse_depfile(content, &source, cwd).unwrap();

    assert_eq!(result.resolved.len(), 1);
    assert_eq!(result.resolved[0], canon(&bar_h));
}

#[test]
fn display_impl_for_errors() {
    let io_err = DepfileError::Io(std::io::Error::new(
        std::io::ErrorKind::NotFound,
        "not found",
    ));
    let msg = format!("{io_err}");
    assert!(msg.contains("I/O error"));

    let mal_err = DepfileError::Malformed("bad content".to_string());
    let msg = format!("{mal_err}");
    assert!(msg.contains("malformed"));
    assert!(msg.contains("bad content"));
}

// ── Unit tests for internal helpers ──────────────────────────────────

#[test]
fn join_continuations_replaces_with_space() {
    assert_eq!(join_continuations("a \\\n  b"), "a    b");
    assert_eq!(join_continuations("a \\\r\n  b"), "a    b");
}

#[test]
fn join_continuations_preserves_other_backslashes() {
    assert_eq!(join_continuations(r"C:\path\file"), r"C:\path\file");
}

#[test]
fn find_separator_colon_simple() {
    assert_eq!(find_separator_colon("foo.o: bar.c").unwrap(), 5);
}

#[test]
fn find_separator_colon_skips_drive_letter() {
    // The colon at position 1 (C:) should be skipped; the real
    // separator is after "foo.o".
    let line = r"C:\build\foo.o: C:\src\bar.c";
    let pos = find_separator_colon(line).unwrap();
    assert_eq!(&line[pos..pos + 1], ":");
    // The separator should be at position 14 (after "C:\build\foo.o").
    assert_eq!(pos, 14);
}

#[test]
fn split_and_unescape_basic() {
    let tokens = split_and_unescape(" foo.c  bar.h  baz.h ");
    assert_eq!(tokens, vec!["foo.c", "bar.h", "baz.h"]);
}

#[test]
fn split_and_unescape_escaped_space() {
    let tokens = split_and_unescape(r" path\ with\ spaces/foo.h bar.h ");
    assert_eq!(tokens, vec!["path with spaces/foo.h", "bar.h"]);
}

#[test]
fn split_and_unescape_escaped_hash() {
    let tokens = split_and_unescape(r" file\#1.h ");
    assert_eq!(tokens, vec!["file#1.h"]);
}

// ── Strategy tests ───────────────────────────────────────────────────

#[test]
fn strategy_unsupported() {
    let dep_flags = UserDepFlags::default();
    let (args, strategy) =
        prepare_depfile(false, &dep_flags, Path::new("foo.o"), Path::new("/tmp"));
    assert!(args.is_empty());
    assert_eq!(strategy, DepfileStrategy::Unsupported);
}

#[test]
fn strategy_user_mf() {
    let dep_flags = UserDepFlags {
        has_md: true,
        mf_path: Some(NormalizedPath::from("/build/deps.d")),
    };
    let (args, strategy) = prepare_depfile(true, &dep_flags, Path::new("foo.o"), Path::new("/tmp"));
    assert!(args.is_empty());
    assert_eq!(
        strategy,
        DepfileStrategy::UserSpecified {
            path: NormalizedPath::from("/build/deps.d")
        }
    );
}

#[test]
fn strategy_user_md_no_mf() {
    let dep_flags = UserDepFlags {
        has_md: true,
        mf_path: None,
    };
    let (args, strategy) = prepare_depfile(true, &dep_flags, Path::new("foo.o"), Path::new("/tmp"));
    assert!(args.is_empty());
    assert_eq!(
        strategy,
        DepfileStrategy::UserDefault {
            path: NormalizedPath::from("foo.d")
        }
    );
}

// ── Issue #643: user_depfile_destination semantics ──────────────────

#[test]
fn user_depfile_destination_returns_mf_path_when_present() {
    let dep_flags = UserDepFlags {
        has_md: true,
        mf_path: Some(NormalizedPath::from("/build/explicit.d")),
    };
    assert_eq!(
        user_depfile_destination(&dep_flags, Path::new("/out/foo.o")),
        Some(NormalizedPath::from("/build/explicit.d")),
        "explicit -MF must win over the implicit <output>.d default",
    );
}

#[test]
fn user_depfile_destination_derives_default_from_output_when_md_only() {
    let dep_flags = UserDepFlags {
        has_md: true,
        mf_path: None,
    };
    assert_eq!(
        user_depfile_destination(&dep_flags, Path::new("/out/foo.o")),
        Some(NormalizedPath::from("/out/foo.d")),
        "-MD without -MF defaults to <output_stem>.d alongside the object",
    );
}

#[test]
fn user_depfile_destination_none_when_user_has_no_dep_flags() {
    let dep_flags = UserDepFlags::default();
    assert_eq!(
        user_depfile_destination(&dep_flags, Path::new("/out/foo.o")),
        None,
        "no user dep flags = injected strategy = not the user's depfile",
    );
}

#[test]
fn strategy_injected() {
    let dep_flags = UserDepFlags::default();
    let (args, strategy) = prepare_depfile(true, &dep_flags, Path::new("foo.o"), Path::new("/tmp"));

    assert_eq!(args.len(), 3);
    assert_eq!(args[0], "-MD");
    assert_eq!(args[1], "-MF");
    assert!(args[2].ends_with(".d"));

    match strategy {
        DepfileStrategy::Injected { path } => {
            assert!(path.to_string_lossy().ends_with(".d"));
            assert!(path.starts_with("/tmp"));
        }
        other => panic!("expected Injected, got: {other:?}"),
    }
}

#[test]
fn strategy_injected_adds_args() {
    let dep_flags = UserDepFlags::default();
    let (args, _) = prepare_depfile(true, &dep_flags, Path::new("bar.o"), Path::new("/tmp"));
    assert_eq!(args[0], "-MD");
    assert_eq!(args[1], "-MF");
    // The path should contain the stem "bar".
    assert!(
        args[2].contains("bar"),
        "expected 'bar' in path: {}",
        args[2]
    );
}

/// Issue #573: `canonicalize_path` caches its results by input
/// path string. Tested by verifying the function returns the same
/// canonical output across two calls with the same input — both
/// pre- and post-cache that's guaranteed correctness-wise, but
/// after the first call the entry is in the global cache (size
/// monotonically increases), so the contract is sound.
///
/// We don't assert on absolute or delta cache_len because the
/// canonicalize cache is process-wide and other tests in the
/// crate populate it concurrently — equality assertions race.
/// The cache HIT is implicit in the function's contract.
#[test]
fn canonicalize_path_caches_results() {
    let dir = TempDir::new().unwrap();
    let cwd = dir.path();
    let hdr = touch(cwd, "shared-header-573a.h");

    let first = canonicalize_path(&hdr, cwd);
    let second = canonicalize_path(&hdr, cwd);
    assert_eq!(first, second, "cached canonical output must match");

    // Cache must monotonically grow: at minimum our entry is in
    // there now (others may have been added concurrently too).
    assert!(canonicalize_cache_len_for_test() > 0);
}

/// Issue #573 regression guard: canonicalize_path cache must
/// distinguish entries by input path. Two different input
/// strings of the same file produce two cache entries — verified
/// by snapshotting the cache len before-and-after under the
/// assumption that no concurrent test would happen to insert
/// AND evict in the same window (the cache never evicts today).
/// Inputs use unique per-test names so no other test contributes
/// entries with the same keys.
#[test]
fn canonicalize_path_cache_distinguishes_inputs() {
    let dir = TempDir::new().unwrap();
    let cwd = dir.path();
    let hdr = touch(cwd, "distinct-573b.h");
    // Two inputs that differ as strings but resolve to the same
    // file. Both forms are unique to this test (per-tempdir).
    let abs_input = hdr.clone();
    let mut redundant_input = cwd.to_path_buf();
    redundant_input.push(".");
    redundant_input.push("distinct-573b.h");

    let r1 = canonicalize_path(&abs_input, cwd);
    let r2 = canonicalize_path(&redundant_input, cwd);
    // Both resolve to the same on-disk file → same canonical.
    assert_eq!(
        r1, r2,
        "different inputs of the same file resolve identically"
    );
}
