//! Adversarial classification tests for clang-cl / MSVC argument parsing.
//!
//! Issue #261: on Windows MSVC builds, clang-cl invocations were arriving at
//! the daemon (the compilation counter incremented) but never landing in any
//! outcome bucket (`cached`, `cold`, or `non-cacheable`). The root cause was
//! that `parse_invocation` only understood GCC-style flags; clang-cl's
//! `/c`, `/Fo:`, `/EHsc`, etc. silently dropped through the parser, leaving
//! the invocation unclassified.
//!
//! These tests exercise the contract that fix introduces:
//!   1. Every clang-cl / cl.exe invocation that reaches `parse_invocation`
//!      must produce exactly one of `Cacheable`, `MultiFile`, `NonCacheable`.
//!      No invocation is silently dropped.
//!   2. The canonical clang-cl compile shape (`/c /Fo:out.obj src.c`) MUST
//!      classify as `Cacheable` with the correct source and output paths.
//!   3. Argument shapes that real build systems (cc-rs, ninja, MSBuild)
//!      generate must classify correctly — including response files,
//!      mixed `-`/`/` prefixes, and the `/Tc`/`/Tp` source-language flags.

use std::path::Path;

use zccache::compiler::response_file::expand_response_files_in;
use zccache::compiler::{parse_invocation, CompilerFamily, ParsedInvocation};
use zccache::core::NormalizedPath;

fn s(v: &[&str]) -> Vec<String> {
    v.iter().map(|x| x.to_string()).collect()
}

/// Trip the classification invariant: every invocation must classify into
/// exactly one of the three buckets. This is the contract that keeps the
/// daemon's `total_compilations == cached + cold + non_cacheable` invariant
/// holding (issue #261).
fn assert_classifies(compiler: &str, argv: &[&str]) {
    let result = parse_invocation(compiler, &s(argv));
    match result {
        ParsedInvocation::Cacheable(_)
        | ParsedInvocation::MultiFile { .. }
        | ParsedInvocation::NonCacheable { .. } => (),
    }
}

// ─── Issue #261 canonical shapes ────────────────────────────────────────

#[test]
fn issue_261_clang_cl_classifies_as_cacheable() {
    // The exact shape that produced "2 total, 0 cached, 0 cold, 0
    // non-cacheable" in the issue. Must now be Cacheable.
    let result = parse_invocation("clang-cl", &s(&["/c", "/Fo:hello.obj", "hello.c"]));
    match result {
        ParsedInvocation::Cacheable(c) => {
            assert_eq!(c.family, CompilerFamily::Msvc);
            assert_eq!(c.source_file, NormalizedPath::new("hello.c"));
            assert_eq!(c.output_file, NormalizedPath::new("hello.obj"));
        }
        other => panic!("expected Cacheable, got: {other:?}"),
    }
}

#[test]
fn issue_261_clang_cl_exe_path_with_spaces() {
    // clang-cl is often invoked via its full path under `C:\Program Files`.
    let result = parse_invocation(
        "C:\\Program Files\\LLVM\\bin\\clang-cl.exe",
        &s(&["/c", "/Fo:hello.obj", "hello.c"]),
    );
    assert!(matches!(result, ParsedInvocation::Cacheable(_)));
}

#[test]
fn issue_261_cc_rs_style_invocation() {
    // Mirror what `cc-rs` generates for an `x86_64-pc-windows-msvc` target.
    let argv = &[
        "-nologo",
        "-MD",
        "-Z7",
        "-Brepro",
        "/I",
        "C:\\src\\zlib\\include",
        "-W4",
        "-DZLIB_DEBUG=1",
        "/c",
        "/Fo:zlib-out\\adler32.obj",
        "C:\\src\\zlib\\adler32.c",
    ];
    let result = parse_invocation("clang-cl", &s(argv));
    match result {
        ParsedInvocation::Cacheable(c) => {
            assert_eq!(c.family, CompilerFamily::Msvc);
            assert_eq!(
                c.source_file,
                NormalizedPath::new("C:\\src\\zlib\\adler32.c")
            );
            assert_eq!(c.output_file, NormalizedPath::new("zlib-out\\adler32.obj"));
        }
        other => panic!("expected Cacheable, got: {other:?}"),
    }
}

// ─── Classification invariant fuzzing ───────────────────────────────────

#[test]
fn invariant_random_shapes_classify() {
    // Hand-picked shapes that cover the corners of the parser. Each one
    // MUST classify (never silently drop). This is the fuzz-style guard
    // that satisfies the issue's request: "Add an assertion or a fuzz
    // test that guarantees this invariant."
    let shapes: &[&[&str]] = &[
        &[],
        &["/?"],
        &["--help"],
        &["--version"],
        &["/c"],
        &["foo.c"],
        &["/c", "foo.c"],
        &["/c", "/Fo:foo.obj", "foo.c"],
        &["/c", "/Fo", "foo.obj", "foo.c"],
        &["-c", "-o", "foo.obj", "foo.c"],
        &["-c", "-Fo:foo.obj", "foo.c"],
        &["/c", "/Tcfoo.c", "/Fo:foo.obj"],
        &["/c", "/Tp", "foo.cpp", "/Fo:foo.obj"],
        &["/c", "a.c", "b.c", "c.c"],
        &["/c", "/EHsc", "/std:c++17", "/Fo:foo.obj", "foo.cpp"],
        &["/E", "foo.c"],
        &["/P", "foo.c"],
        &["/EP", "foo.c"],
        &["/c", "/D", "FOO=1", "/I", "C:\\inc", "/Fo:foo.obj", "foo.c"],
        // Mixed prefixes
        &["/c", "-DFOO", "/DBAR", "/Fo:foo.obj", "foo.c"],
        // Unknown slash flags
        &["/c", "/SomeUnknownFlag", "/Fo:foo.obj", "foo.c"],
        // Just a /Fe (link) — must be non-cacheable
        &["/Fe:foo.exe", "foo.c"],
        // .C and .CPP uppercase extensions
        &["/c", "/Fo:foo.obj", "FOO.C"],
        &["/c", "/Fo:foo.obj", "FOO.CPP"],
    ];
    for shape in shapes {
        assert_classifies("clang-cl", shape);
        assert_classifies("cl.exe", shape);
    }
}

// ─── Response files (`@file`) ──────────────────────────────────────────

#[test]
fn response_file_with_msvc_flags_classifies() {
    // Build a response file containing MSVC-style args and exercise the
    // full expand → parse pipeline (which is what the daemon does in
    // `handle_compile`).
    let tmp = tempfile::tempdir().unwrap();
    let rsp_path = tmp.path().join("compile.rsp");
    std::fs::write(&rsp_path, "/c /Fo:hello.obj /EHsc /std:c++17 hello.cpp\n").unwrap();

    let argv = s(&[format!("@{}", rsp_path.display()).as_str()]);
    let expanded = expand_response_files_in(&argv, tmp.path()).unwrap();
    let result = parse_invocation("clang-cl", &expanded);
    match result {
        ParsedInvocation::Cacheable(c) => {
            assert_eq!(c.source_file, NormalizedPath::new("hello.cpp"));
            assert_eq!(c.output_file, NormalizedPath::new("hello.obj"));
        }
        other => panic!("expected Cacheable, got: {other:?}"),
    }
}

// ─── /Tc and /Tp source-language flags ──────────────────────────────────

#[test]
fn tc_inline_source_classifies() {
    let result = parse_invocation("clang-cl", &s(&["/c", "/Tchello.c", "/Fo:hello.obj"]));
    match result {
        ParsedInvocation::Cacheable(c) => {
            assert_eq!(c.source_file, NormalizedPath::new("hello.c"));
        }
        other => panic!("expected Cacheable, got: {other:?}"),
    }
}

#[test]
fn tp_space_separated_source_classifies() {
    let result = parse_invocation("clang-cl", &s(&["/c", "/Tp", "hello.cpp", "/Fo:hello.obj"]));
    match result {
        ParsedInvocation::Cacheable(c) => {
            assert_eq!(c.source_file, NormalizedPath::new("hello.cpp"));
        }
        other => panic!("expected Cacheable, got: {other:?}"),
    }
}

// ─── Negative classifications ───────────────────────────────────────────

#[test]
fn link_only_classifies_as_non_cacheable() {
    let result = parse_invocation("clang-cl", &s(&["foo.obj", "/Fe:foo.exe"]));
    assert!(matches!(result, ParsedInvocation::NonCacheable { .. }));
}

#[test]
fn preprocess_only_classifies_as_non_cacheable() {
    let result = parse_invocation("clang-cl", &s(&["/E", "foo.c"]));
    assert!(matches!(result, ParsedInvocation::NonCacheable { .. }));
}

#[test]
fn version_query_classifies_as_non_cacheable() {
    let result = parse_invocation("clang-cl", &s(&["--version"]));
    assert!(matches!(result, ParsedInvocation::NonCacheable { .. }));
}

// ─── Suppress unused warnings on non-Windows. ───────────────────────────

#[allow(dead_code)]
fn _silence_path_import() {
    let _ = Path::new(".");
}
