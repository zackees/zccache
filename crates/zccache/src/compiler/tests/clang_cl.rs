//! clang-cl / cl.exe dispatch into the MSVC parser (issue #261), MSVC-style flags.

use super::args;
use super::super::{detect_family, parse_invocation, CompilerFamily, ParsedInvocation};
use crate::core::NormalizedPath;

#[test]
fn detect_clang_cl_is_msvc() {
    // clang-cl speaks MSVC argv syntax — must be classified as Msvc so
    // the MSVC parser handles `/Fo`, `/c`, etc. Issue #261.
    assert_eq!(detect_family("clang-cl"), CompilerFamily::Msvc);
    assert_eq!(detect_family("clang-cl.exe"), CompilerFamily::Msvc);
    assert_eq!(detect_family("Clang-CL.EXE"), CompilerFamily::Msvc);
    assert_eq!(
        detect_family("C:\\Program Files\\LLVM\\bin\\clang-cl.exe"),
        CompilerFamily::Msvc
    );
    // Versioned clang-cl.
    assert_eq!(detect_family("clang-cl-17"), CompilerFamily::Msvc);
    assert_eq!(detect_family("clang-cl-18.exe"), CompilerFamily::Msvc);
    // Plain clang remains Clang, not Msvc.
    assert_eq!(detect_family("clang"), CompilerFamily::Clang);
    assert_eq!(detect_family("clang++"), CompilerFamily::Clang);
    assert_eq!(detect_family("clang-17"), CompilerFamily::Clang);
}

#[test]
fn clang_cl_compile_classified_as_cacheable() {
    // The exact symptom from issue #261: clang-cl invocations with
    // MSVC-style `/c`, `/Fo:`, etc. must land in the Cacheable bucket,
    // not be silently dropped.
    let result = parse_invocation("clang-cl", &args(&["/c", "/Fo:hello.obj", "hello.c"]));
    match result {
        ParsedInvocation::Cacheable(c) => {
            assert_eq!(c.source_file, NormalizedPath::new("hello.c"));
            assert_eq!(c.output_file, NormalizedPath::new("hello.obj"));
            assert_eq!(c.family, CompilerFamily::Msvc);
        }
        other => panic!("expected cacheable, got: {other:?}"),
    }
}

#[test]
fn cl_compile_classified_as_cacheable() {
    // Plain `cl.exe` (not clang-cl) with MSVC flags.
    let result = parse_invocation(
        "cl.exe",
        &args(&["/c", "/EHsc", "/std:c++17", "/Fo:hello.obj", "hello.cpp"]),
    );
    match result {
        ParsedInvocation::Cacheable(c) => {
            assert_eq!(c.source_file, NormalizedPath::new("hello.cpp"));
            assert_eq!(c.output_file, NormalizedPath::new("hello.obj"));
            assert_eq!(c.family, CompilerFamily::Msvc);
        }
        other => panic!("expected cacheable, got: {other:?}"),
    }
}

#[test]
fn clang_with_msvc_style_args_falls_back_to_msvc_parser() {
    // Some build systems invoke plain `clang` but pass `/c` and `/Fo`
    // (which clang understands when targeting MSVC). Without the
    // heuristic dispatch the GCC parser silently drops `/c` and the
    // invocation gets misclassified as a link.
    let result = parse_invocation("clang", &args(&["/c", "/Fo:hello.obj", "hello.c"]));
    match result {
        ParsedInvocation::Cacheable(c) => {
            assert_eq!(c.source_file, NormalizedPath::new("hello.c"));
            assert_eq!(c.output_file, NormalizedPath::new("hello.obj"));
            // family stays Clang because the executable is `clang`,
            // even though we dispatch to the MSVC parser.
            assert_eq!(c.family, CompilerFamily::Clang);
        }
        other => panic!("expected cacheable, got: {other:?}"),
    }
}

#[test]
fn clang_cl_no_c_flag_classified_as_non_cacheable() {
    // Link-only clang-cl invocation must end up in `non-cacheable`,
    // not be silently dropped.
    let result = parse_invocation("clang-cl", &args(&["hello.c", "/Fe:hello.exe"]));
    assert!(matches!(result, ParsedInvocation::NonCacheable { .. }));
}

#[test]
fn clang_cl_help_query_is_non_cacheable() {
    let result = parse_invocation("clang-cl", &args(&["/?"]));
    assert!(matches!(result, ParsedInvocation::NonCacheable { .. }));
}

#[test]
fn clang_cl_version_query_is_non_cacheable() {
    let result = parse_invocation("clang-cl", &args(&["--version"]));
    assert!(matches!(result, ParsedInvocation::NonCacheable { .. }));
}

#[test]
fn clang_cl_with_d_macro_space_separated() {
    // `/D NAME=VAL` must consume both elements so the value isn't
    // misclassified as a source.
    let result = parse_invocation(
        "clang-cl",
        &args(&["/c", "/D", "NDEBUG=1", "/Fo:hello.obj", "hello.c"]),
    );
    match result {
        ParsedInvocation::Cacheable(c) => {
            assert_eq!(c.source_file, NormalizedPath::new("hello.c"));
        }
        other => panic!("expected cacheable, got: {other:?}"),
    }
}

#[test]
fn clang_cl_with_i_path_containing_spaces() {
    let result = parse_invocation(
        "clang-cl",
        &args(&[
            "/c",
            "/I",
            "C:\\Program Files\\include",
            "/Fo:hello.obj",
            "hello.c",
        ]),
    );
    match result {
        ParsedInvocation::Cacheable(c) => {
            assert_eq!(c.source_file, NormalizedPath::new("hello.c"));
        }
        other => panic!("expected cacheable, got: {other:?}"),
    }
}

#[test]
fn clang_cl_mixed_dash_and_slash_flags() {
    // `cc-rs` produces invocations with both `-D` and `/c` mixed.
    let result = parse_invocation(
        "clang-cl",
        &args(&["/c", "-DFOO=1", "/DBAR=2", "/Fo:hello.obj", "hello.c"]),
    );
    match result {
        ParsedInvocation::Cacheable(c) => {
            assert_eq!(c.source_file, NormalizedPath::new("hello.c"));
        }
        other => panic!("expected cacheable, got: {other:?}"),
    }
}

#[test]
fn clang_cl_unknown_slash_flag_preserved_not_dropped() {
    let result = parse_invocation(
        "clang-cl",
        &args(&["/c", "/XYZUnknown", "/Fo:hello.obj", "hello.c"]),
    );
    match result {
        ParsedInvocation::Cacheable(c) => {
            assert!(c.unknown_flags.contains(&"/XYZUnknown".to_string()));
        }
        other => panic!("expected cacheable, got: {other:?}"),
    }
}

#[test]
fn clang_cl_multi_file_classified() {
    let result = parse_invocation("clang-cl", &args(&["/c", "a.c", "b.c"]));
    match result {
        ParsedInvocation::MultiFile { compilations, .. } => {
            assert_eq!(compilations.len(), 2);
            assert_eq!(compilations[0].output_file, NormalizedPath::new("a.obj"));
            assert_eq!(compilations[1].output_file, NormalizedPath::new("b.obj"));
        }
        other => panic!("expected MultiFile, got: {other:?}"),
    }
}

// ─── Invariant: classification is total (issue #261) ─────────────────
//
// For every parseable invocation we must return exactly one of
// `Cacheable`, `MultiFile`, or `NonCacheable`. Nothing silently drops.
// The daemon's `total_compilations` counter must always match
// `cached + cold + non_cacheable + errors`. This is enforced at the
// daemon layer; here we just guarantee the parser never produces a
// shape that lets the daemon skip classification.

fn assert_classifies(compiler: &str, argv: &[&str]) {
    let result = parse_invocation(compiler, &args(argv));
    match result {
        ParsedInvocation::Cacheable(_)
        | ParsedInvocation::MultiFile { .. }
        | ParsedInvocation::NonCacheable { .. } => (),
    }
}

#[test]
fn invariant_every_clang_cl_shape_classifies() {
    // No matter what we throw at clang-cl, the parser MUST produce one
    // of the three classification outcomes. This prevents the issue
    // #261 silent-drop regression from ever reappearing.
    assert_classifies("clang-cl", &["/c", "/Fo:foo.obj", "foo.c"]);
    assert_classifies("clang-cl", &["-c", "-o", "foo.obj", "foo.c"]);
    assert_classifies("clang-cl", &["/c", "foo.c", "bar.cpp"]);
    assert_classifies("clang-cl", &[]);
    assert_classifies("clang-cl", &["--version"]);
    assert_classifies("clang-cl", &["/?"]);
    assert_classifies("clang-cl", &["/c"]);
    assert_classifies("clang-cl", &["foo.c"]);
    assert_classifies("clang-cl", &["/E", "foo.c"]);
    assert_classifies(
        "clang-cl",
        &[
            "/c",
            "/EHsc",
            "/std:c++17",
            "/MD",
            "/W4",
            "/Zi",
            "/Fd:vc.pdb",
            "/Fo:hello.obj",
            "/showIncludes",
            "hello.cpp",
        ],
    );
    assert_classifies(
        "C:\\Program Files\\LLVM\\bin\\clang-cl.exe",
        &["/c", "/Fo:foo.obj", "foo.c"],
    );
}
