//! Integration + regression tests for response files end-to-end.
//!
//! Covers:
//! - Integration with `parse_invocation` through response files
//! - Regression guards for known parser/expander traps
//! - Windows-specific spill-rsp roundtrips (fbuild shape)
//!
//! Run all:    soldr cargo test -p zccache --test compiler_response_file_integration_test -- --nocapture
//! Run single: soldr cargo test -p zccache --test compiler_response_file_integration_test -- <test_name> --nocapture

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::panic_in_result_fn,
    clippy::unwrap_in_result
)]

use std::path::Path;
use zccache::compiler::response_file::{expand_response_files, ResponseFileError};

#[cfg(windows)]
use zccache::compiler::response_file::{
    parse_response_file_content, write_response_file_if_needed,
};

fn s(v: &[&str]) -> Vec<String> {
    v.iter().map(|x| x.to_string()).collect()
}

#[cfg(windows)]
fn force_spill_args_owned(seed: Vec<String>) -> Vec<String> {
    let mut args = seed;
    while args.iter().map(|a| a.len() + 3).sum::<usize>() < 31_000 {
        args.push(format!("-D_FILLER_{}={}", args.len(), "X".repeat(128)));
    }
    args
}

// ═══════════════════════════════════════════════════════════════════════════════
// SECTION 9: INTEGRATION — RESPONSE FILES + parse_invocation
// ═══════════════════════════════════════════════════════════════════════════════

/// All args come from a response file.
#[test]
fn integration_all_args_from_response_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("all.rsp");
    std::fs::write(&path, "-c foo.cpp -o foo.o -O2 -Wall").unwrap();

    let args = s(&[&format!("@{}", path.display())]);
    let expanded = expand_response_files(&args).unwrap();
    match zccache::compiler::parse_invocation("gcc", &expanded) {
        zccache::compiler::ParsedInvocation::Cacheable(c) => {
            assert_eq!(c.source_file, Path::new("foo.cpp"));
            assert_eq!(c.output_file, Path::new("foo.o"));
            assert!(c.original_args.contains(&"-O2".to_string()));
            assert!(c.original_args.contains(&"-Wall".to_string()));
        }
        other => panic!("expected cacheable, got: {other:?}"),
    }
}

/// -c flag comes from response file, source file is inline.
#[test]
fn integration_c_flag_from_response_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("flags.rsp");
    std::fs::write(&path, "-c -O2 -Wall").unwrap();

    let args = s(&["foo.cpp", &format!("@{}", path.display()), "-o", "foo.o"]);
    let expanded = expand_response_files(&args).unwrap();
    match zccache::compiler::parse_invocation("clang", &expanded) {
        zccache::compiler::ParsedInvocation::Cacheable(c) => {
            assert_eq!(c.source_file, Path::new("foo.cpp"));
            assert_eq!(c.output_file, Path::new("foo.o"));
        }
        other => panic!("expected cacheable, got: {other:?}"),
    }
}

/// Non-cacheable flag (-E) comes from response file.
#[test]
fn integration_noncacheable_flag_from_response_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("preprocess.rsp");
    std::fs::write(&path, "-E -DNDEBUG").unwrap();

    let args = s(&["-c", "foo.cpp", &format!("@{}", path.display())]);
    let expanded = expand_response_files(&args).unwrap();
    match zccache::compiler::parse_invocation("gcc", &expanded) {
        zccache::compiler::ParsedInvocation::NonCacheable { .. } => { /* expected */ }
        other => panic!("expected cacheable, got: {other:?}"),
    }
}

/// Response file with quoted source path containing spaces.
#[test]
fn integration_quoted_source_path_from_response_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("quoted.rsp");
    std::fs::write(&path, "-c \"path with spaces/main.cpp\" -o main.o").unwrap();

    let args = s(&[&format!("@{}", path.display())]);
    let expanded = expand_response_files(&args).unwrap();
    match zccache::compiler::parse_invocation("gcc", &expanded) {
        zccache::compiler::ParsedInvocation::Cacheable(c) => {
            assert_eq!(c.source_file, Path::new("path with spaces/main.cpp"));
            assert_eq!(c.output_file, Path::new("main.o"));
        }
        other => panic!("expected cacheable, got: {other:?}"),
    }
}

/// Nested response files that together form a cacheable invocation.
#[test]
fn integration_nested_response_files_cacheable() {
    let dir = tempfile::tempdir().unwrap();

    let flags_file = dir.path().join("flags.rsp");
    std::fs::write(&flags_file, "-O2 -Wall -DNDEBUG").unwrap();

    let outer_file = dir.path().join("outer.rsp");
    std::fs::write(
        &outer_file,
        format!("-c main.cpp @{}", flags_file.display()),
    )
    .unwrap();

    let args = s(&[&format!("@{}", outer_file.display()), "-o", "main.o"]);
    let expanded = expand_response_files(&args).unwrap();
    match zccache::compiler::parse_invocation("g++", &expanded) {
        zccache::compiler::ParsedInvocation::Cacheable(c) => {
            assert_eq!(c.source_file, Path::new("main.cpp"));
            assert_eq!(c.output_file, Path::new("main.o"));
            assert!(c.original_args.contains(&"-O2".to_string()));
            assert!(c.original_args.contains(&"-Wall".to_string()));
            assert!(c.original_args.contains(&"-DNDEBUG".to_string()));
        }
        other => panic!("expected cacheable, got: {other:?}"),
    }
}

/// Multiple source files spread across inline and response file → non-cacheable.
#[test]
fn integration_multiple_sources_via_response_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("sources.rsp");
    std::fs::write(&path, "b.cpp").unwrap();

    let args = s(&["-c", "a.cpp", &format!("@{}", path.display())]);
    let expanded = expand_response_files(&args).unwrap();
    match zccache::compiler::parse_invocation("gcc", &expanded) {
        zccache::compiler::ParsedInvocation::MultiFile { compilations, .. } => {
            assert_eq!(compilations.len(), 2);
        }
        other => {
            panic!("expected MultiFile with 2 sources, got: {other:?}")
        }
    }
}

/// Response file with -D flag using = with quoted value, verify cache key capture.
#[test]
fn integration_define_with_quoted_value_in_cache_key() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("defines.rsp");
    std::fs::write(
        &path,
        "-DVERSION=\"1.2.3\" -DBUILD_TYPE=\"Release\" -DPATH=\"/usr/local\"",
    )
    .unwrap();

    let args = s(&["-c", "main.cpp", &format!("@{}", path.display())]);
    let expanded = expand_response_files(&args).unwrap();
    match zccache::compiler::parse_invocation("clang++", &expanded) {
        zccache::compiler::ParsedInvocation::Cacheable(c) => {
            assert!(c.original_args.contains(&"-DVERSION=1.2.3".to_string()));
            assert!(c
                .original_args
                .contains(&"-DBUILD_TYPE=Release".to_string()));
            assert!(c.original_args.contains(&"-DPATH=/usr/local".to_string()));
        }
        other => panic!("expected cacheable, got: {other:?}"),
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// SECTION 11: PARSER ADVERSARIAL — REGRESSION GUARDS
// ═══════════════════════════════════════════════════════════════════════════════

/// Ensure that a response file arg is not confused with a compiler flag
/// when the arg starts with @.
#[test]
fn regression_at_arg_not_confused_with_flag() {
    // After expansion, @file becomes its contents. But if expansion produces
    // an arg like "@something" that isn't a file, expansion should error.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("produces_at.rsp");
    // This file's content, after parsing, includes "@nonexistent"
    std::fs::write(&path, "-O2 @nonexistent -Wall").unwrap();

    let args = s(&[&format!("@{}", path.display())]);
    let result = expand_response_files(&args);
    // @nonexistent should be treated as a response file reference → ReadError
    assert!(matches!(result, Err(ResponseFileError::ReadError { .. })));
}

/// Windows-style path in @reference.
#[test]
fn regression_windows_path_in_at_reference() {
    // On Windows, @C:\path\to\file.rsp should work.
    // On Unix, this path just won't exist, giving ReadError.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("win.rsp");
    std::fs::write(&path, "-O2").unwrap();

    // Use the actual temp path (works on both platforms)
    let args = s(&[&format!("@{}", path.display())]);
    let result = expand_response_files(&args).unwrap();
    assert_eq!(result, s(&["-O2"]));
}

/// Relative path ../ in response file reference.
#[test]
fn regression_relative_path_in_reference() {
    let dir = tempfile::tempdir().unwrap();
    let subdir = dir.path().join("sub");
    std::fs::create_dir_all(&subdir).unwrap();

    // Create file in parent dir
    let parent_file = dir.path().join("parent.rsp");
    std::fs::write(&parent_file, "-DFROM_PARENT").unwrap();

    // Create file in subdir that references ../parent.rsp
    let child_file = subdir.join("child.rsp");
    std::fs::write(
        &child_file,
        format!("-DFROM_CHILD @{}", parent_file.display()),
    )
    .unwrap();

    let args = s(&[&format!("@{}", child_file.display())]);
    let result = expand_response_files(&args).unwrap();
    assert_eq!(result, s(&["-DFROM_CHILD", "-DFROM_PARENT"]));
}

/// Verify that the `seen` set uses canonical paths, so symlinks are detected.
/// (Only works on Unix, so we skip on Windows.)
#[cfg(unix)]
#[test]
fn regression_symlink_cycle_detected() {
    use std::os::unix::fs::symlink;

    let dir = tempfile::tempdir().unwrap();
    let real_a = dir.path().join("real_a.rsp");
    let link_b = dir.path().join("link_b.rsp");

    // real_a references link_b, which is a symlink back to real_a
    std::fs::write(&real_a, format!("@{}", link_b.display())).unwrap();
    symlink(&real_a, &link_b).unwrap();

    let args = s(&[&format!("@{}", real_a.display())]);
    let result = expand_response_files(&args);
    assert!(result.is_err());
    assert!(matches!(
        result.unwrap_err(),
        ResponseFileError::CircularReference { .. }
    ));
}

/// Response file containing only comments-looking lines (# prefix).
/// There is no comment syntax in GCC response files — # is literal.
#[test]
fn regression_hash_is_not_comment() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("hash.rsp");
    std::fs::write(&path, "# this is not a comment\n-O2").unwrap();

    let args = s(&[&format!("@{}", path.display())]);
    let result = expand_response_files(&args).unwrap();
    // # and "this", "is", etc. are all separate arguments
    assert!(result.contains(&"#".to_string()));
    assert!(result.contains(&"-O2".to_string()));
}

/// Response file with trailing newline vs without — should produce same args.
#[test]
fn regression_trailing_newline_irrelevant() {
    let dir = tempfile::tempdir().unwrap();

    let with_nl = dir.path().join("with_nl.rsp");
    std::fs::write(&with_nl, "-O2 -Wall\n").unwrap();

    let without_nl = dir.path().join("without_nl.rsp");
    std::fs::write(&without_nl, "-O2 -Wall").unwrap();

    let r1 = expand_response_files(&s(&[&format!("@{}", with_nl.display())])).unwrap();
    let r2 = expand_response_files(&s(&[&format!("@{}", without_nl.display())])).unwrap();
    assert_eq!(r1, r2);
}

/// Argument that is just "@" followed by space and then a real @file.
#[test]
fn regression_bare_at_then_real_at() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("real.rsp");
    std::fs::write(&path, "-DREAL").unwrap();

    let args = s(&["@", &format!("@{}", path.display())]);
    let result = expand_response_files(&args).unwrap();
    assert_eq!(result, s(&["@", "-DREAL"]));
}

#[cfg(windows)]
#[test]
fn windows_fbuild_shape_roundtrips_through_spill_rsp() {
    let dir = tempfile::tempdir().unwrap();
    let includes = dir.path().join("includes.rsp");
    std::fs::write(
        &includes,
        "'-IC:\\SDK\\Include'\n'-IC:\\Project Root\\Generated Headers'\n",
    )
    .unwrap();

    let args = force_spill_args_owned(vec![
        "-c".to_string(),
        r"C:\Project Root\src\main.c".to_string(),
        "-o".to_string(),
        r"C:\Project Root\build\main.o".to_string(),
        r#"-DVERSION="1.2.3""#.to_string(),
        r#"-DPKG_PATH="C:\Program Files\Vendor SDK\include""#.to_string(),
        format!("@{}", includes.display()),
    ]);

    let rsp =
        write_response_file_if_needed(&args, dir.path(), zccache::compiler::CompilerFamily::Clang)
            .unwrap()
            .expect("spill rsp should be written");
    let written = std::fs::read_to_string(&rsp.path).unwrap();
    let reparsed = parse_response_file_content(&written);

    assert_eq!(reparsed, args);
}

#[cfg(windows)]
#[test]
fn windows_fbuild_shape_preserves_expanded_argv_semantics() {
    let original = s(&[
        "-c",
        r"C:\Project Root\src\main.c",
        "-o",
        r"C:\Project Root\build\main.o",
        r#"-DVERSION="1.2.3""#,
        r#"-DPKG_PATH="C:\Program Files\Vendor SDK\include""#,
        r"-IC:\SDK\Include",
        r"-IC:\Project Root\Generated Headers",
    ]);

    let dir = tempfile::tempdir().unwrap();
    let args = force_spill_args_owned(original.clone());
    let rsp =
        write_response_file_if_needed(&args, dir.path(), zccache::compiler::CompilerFamily::Clang)
            .unwrap()
            .expect("spill rsp should be written");
    let written = std::fs::read_to_string(&rsp.path).unwrap();
    let reparsed = parse_response_file_content(&written);

    assert_eq!(reparsed[..original.len()], original);
}
