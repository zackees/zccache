//! Clang/GCC parsing: -c, multi-file, -x header mode, header units, sticky-mode regressions.

use super::super::{parse_invocation, CompilerFamily, ParsedInvocation};
use super::args;
use crate::core::NormalizedPath;

#[test]
fn basic_cacheable_compilation() {
    let result = parse_invocation("clang++", &args(&["-c", "hello.cpp", "-o", "hello.o"]));
    match result {
        ParsedInvocation::Cacheable(c) => {
            assert_eq!(c.source_file, NormalizedPath::new("hello.cpp"));
            assert_eq!(c.output_file, NormalizedPath::new("hello.o"));
            assert_eq!(c.family, CompilerFamily::Clang);
        }
        other => panic!("expected cacheable, got: {other:?}"),
    }
}

#[test]
fn no_c_flag_is_non_cacheable() {
    let result = parse_invocation("gcc", &args(&["hello.cpp", "-o", "hello"]));
    assert!(matches!(result, ParsedInvocation::NonCacheable { .. }));
}

#[test]
fn preprocessing_only_non_cacheable() {
    let result = parse_invocation("gcc", &args(&["-E", "hello.cpp"]));
    assert!(matches!(result, ParsedInvocation::NonCacheable { .. }));
}

#[test]
fn multi_file_split() {
    let result = parse_invocation("gcc", &args(&["-c", "a.cpp", "b.cpp"]));
    match result {
        ParsedInvocation::MultiFile {
            compilations,
            source_indices,
            ..
        } => {
            assert_eq!(compilations.len(), 2);
            assert_eq!(compilations[0].source_file, NormalizedPath::new("a.cpp"));
            assert_eq!(compilations[0].output_file, NormalizedPath::new("a.o"));
            assert_eq!(compilations[1].source_file, NormalizedPath::new("b.cpp"));
            assert_eq!(compilations[1].output_file, NormalizedPath::new("b.o"));
            assert_eq!(source_indices, vec![1, 2]);
        }
        other => panic!("expected MultiFile, got: {other:?}"),
    }
}

#[test]
fn multi_file_with_flags() {
    let result = parse_invocation(
        "g++",
        &args(&["-c", "-O2", "main.cpp", "-Wall", "util.cpp"]),
    );
    match result {
        ParsedInvocation::MultiFile {
            compilations,
            original_args,
            source_indices,
        } => {
            assert_eq!(compilations.len(), 2);
            assert_eq!(compilations[0].source_file, NormalizedPath::new("main.cpp"));
            assert_eq!(compilations[1].source_file, NormalizedPath::new("util.cpp"));
            // Flags are in original_args, not per-compilation
            assert!(original_args.contains(&"-O2".to_string()));
            assert!(original_args.contains(&"-Wall".to_string()));
            assert_eq!(source_indices, vec![2, 4]);
        }
        other => panic!("expected MultiFile, got: {other:?}"),
    }
}

#[test]
fn multi_file_mixed_extensions() {
    let result = parse_invocation("gcc", &args(&["-c", "file1.c", "file2.cpp"]));
    match result {
        ParsedInvocation::MultiFile { compilations, .. } => {
            assert_eq!(compilations.len(), 2);
            assert_eq!(compilations[0].source_file, NormalizedPath::new("file1.c"));
            assert_eq!(
                compilations[1].source_file,
                NormalizedPath::new("file2.cpp")
            );
        }
        other => panic!("expected MultiFile, got: {other:?}"),
    }
}

#[test]
fn stdin_non_cacheable() {
    let result = parse_invocation("gcc", &args(&["-c", "-"]));
    assert!(matches!(result, ParsedInvocation::NonCacheable { .. }));
}

#[test]
fn default_output_name() {
    let result = parse_invocation("gcc", &args(&["-c", "foo.cpp"]));
    match result {
        ParsedInvocation::Cacheable(c) => {
            assert_eq!(c.output_file, NormalizedPath::new("foo.o"));
        }
        _ => panic!("expected cacheable"),
    }
}

#[test]
fn original_args_preserved() {
    let input = args(&["-c", "hello.cpp", "-O2", "-std=c++17", "-DNDEBUG", "-Wall"]);
    let result = parse_invocation("clang++", &input);
    match result {
        ParsedInvocation::Cacheable(c) => {
            assert_eq!(*c.original_args, *input);
        }
        _ => panic!("expected cacheable"),
    }
}

#[test]
fn unknown_flags_preserved_in_original_args() {
    let input = args(&[
        "-c",
        "hello.cpp",
        "--deploy-dependencies",
        "--custom-flag=value",
        "-o",
        "hello.o",
    ]);
    let result = parse_invocation("clang++", &input);
    match result {
        ParsedInvocation::Cacheable(c) => {
            assert_eq!(*c.original_args, *input);
            assert_eq!(c.source_file, NormalizedPath::new("hello.cpp"));
            assert_eq!(c.output_file, NormalizedPath::new("hello.o"));
        }
        other => panic!("expected cacheable, got: {other:?}"),
    }
}

#[test]
fn include_pch_flag_with_value() {
    let result = parse_invocation(
        "clang++",
        &args(&["-c", "foo.cpp", "-include-pch", "pch.h.pch", "-o", "foo.o"]),
    );
    match result {
        ParsedInvocation::Cacheable(c) => {
            // PCH path is NOT treated as a source file
            assert_eq!(c.source_file, NormalizedPath::new("foo.cpp"));
            // Original args preserved
            assert!(c.original_args.contains(&"-include-pch".to_string()));
            assert!(c.original_args.contains(&"pch.h.pch".to_string()));
        }
        other => panic!("expected cacheable, got: {other:?}"),
    }
}

#[test]
fn pch_generation_cpp_header_is_cacheable() {
    // `clang -x c++-header -c pch.h -o pch.h.pch` should be cacheable
    let result = parse_invocation(
        "clang++",
        &args(&["-x", "c++-header", "-c", "pch.h", "-o", "pch.h.pch"]),
    );
    match result {
        ParsedInvocation::Cacheable(c) => {
            assert_eq!(c.source_file, NormalizedPath::new("pch.h"));
            assert_eq!(c.output_file, NormalizedPath::new("pch.h.pch"));
        }
        other => panic!("expected cacheable, got: {other:?}"),
    }
}

#[test]
fn pch_generation_c_header_is_cacheable() {
    // `gcc -x c-header -c stdafx.h -o stdafx.h.gch` should be cacheable
    let result = parse_invocation(
        "gcc",
        &args(&["-x", "c-header", "-c", "stdafx.h", "-o", "stdafx.h.gch"]),
    );
    match result {
        ParsedInvocation::Cacheable(c) => {
            assert_eq!(c.source_file, NormalizedPath::new("stdafx.h"));
            assert_eq!(c.output_file, NormalizedPath::new("stdafx.h.gch"));
        }
        other => panic!("expected cacheable, got: {other:?}"),
    }
}

#[test]
fn pch_generation_without_c_flag_is_cacheable() {
    // Meson invokes PCH generation without -c:
    // `clang++ -x c++-header header.h -o header.pch`
    // `-x c++-header` implies compilation, so -c is redundant.
    let result = parse_invocation(
        "clang++",
        &args(&["-x", "c++-header", "FastLED.h", "-o", "FastLED.h.pch"]),
    );
    match result {
        ParsedInvocation::Cacheable(c) => {
            assert_eq!(c.source_file, NormalizedPath::new("FastLED.h"));
            assert_eq!(c.output_file, NormalizedPath::new("FastLED.h.pch"));
        }
        other => panic!("expected cacheable, got: {other:?}"),
    }
}

#[test]
fn pch_generation_c_header_without_c_flag_is_cacheable() {
    let result = parse_invocation(
        "gcc",
        &args(&["-x", "c-header", "stdafx.h", "-o", "stdafx.h.gch"]),
    );
    match result {
        ParsedInvocation::Cacheable(c) => {
            assert_eq!(c.source_file, NormalizedPath::new("stdafx.h"));
            assert_eq!(c.output_file, NormalizedPath::new("stdafx.h.gch"));
        }
        other => panic!("expected cacheable, got: {other:?}"),
    }
}

#[test]
fn pch_generation_with_meson_flags_is_cacheable() {
    // Full Meson-style PCH invocation with extra flags
    let result = parse_invocation(
        "ctc-clang++",
        &args(&[
            "-x",
            "c++-header",
            "FastLED.h",
            "-o",
            "FastLED.h.pch",
            "-MD",
            "-MF",
            "FastLED.h.pch.d",
            "-fPIC",
            "-Iinclude",
            "-Werror=invalid-pch",
        ]),
    );
    match result {
        ParsedInvocation::Cacheable(c) => {
            assert_eq!(c.source_file, NormalizedPath::new("FastLED.h"));
            assert_eq!(c.output_file, NormalizedPath::new("FastLED.h.pch"));
        }
        other => panic!("expected cacheable, got: {other:?}"),
    }
}

#[test]
fn header_without_x_flag_is_not_source() {
    // Without `-x c++-header`, a .h file should NOT be recognized as a source
    let result = parse_invocation("clang++", &args(&["-c", "pch.h"]));
    assert!(
        matches!(result, ParsedInvocation::NonCacheable { .. }),
        "bare .h without -x header mode should be non-cacheable"
    );
}

#[test]
fn x_flag_reset_disables_header_mode() {
    // `-x c++-header pch.h -x c++ main.cpp -c -o main.o`
    // After `-x c++`, header_mode resets — main.cpp is a normal source,
    // pch.h was collected as header-mode source → multi-file.
    let result = parse_invocation(
        "clang++",
        &args(&[
            "-x",
            "c++-header",
            "pch.h",
            "-x",
            "c++",
            "main.cpp",
            "-c",
            "-o",
            "main.o",
        ]),
    );
    match result {
        ParsedInvocation::MultiFile { compilations, .. } => {
            assert_eq!(compilations.len(), 2);
            assert_eq!(compilations[0].source_file, NormalizedPath::new("pch.h"));
            assert_eq!(compilations[1].source_file, NormalizedPath::new("main.cpp"));
        }
        other => panic!("expected MultiFile, got: {other:?}"),
    }
}

#[test]
fn x_cpp_after_header_is_normal_compilation() {
    // `-x c++-header -x c++ main.cpp -c -o main.o`
    // No header file between the two -x flags. The second -x c++ resets
    // header_mode, so main.cpp is a normal compilation.
    let result = parse_invocation(
        "clang++",
        &args(&[
            "-x",
            "c++-header",
            "-x",
            "c++",
            "main.cpp",
            "-c",
            "-o",
            "main.o",
        ]),
    );
    match result {
        ParsedInvocation::Cacheable(c) => {
            assert_eq!(c.source_file, NormalizedPath::new("main.cpp"));
            assert_eq!(c.output_file, NormalizedPath::new("main.o"));
        }
        other => panic!("expected Cacheable, got: {other:?}"),
    }
}

// ─── Regression tests: sticky header_mode bug ─────────────────────────

#[test]
fn sticky_header_mode_cpp_not_spuriously_pch() {
    // BUG: old code set header_mode=true on `-x c++-header` but never
    // reset it on `-x c++`, so main.cpp was treated as a header file
    // needing PCH generation. After the fix, `-x c++` resets header_mode,
    // and main.cpp is a normal source — not a PCH candidate.
    let result = parse_invocation(
        "clang++",
        &args(&[
            "-x",
            "c++-header",
            "pch.h",
            "-o",
            "pch.h.pch",
            "-x",
            "c++",
            "-c",
            "main.cpp",
            "-o",
            "main.o",
        ]),
    );
    // main.cpp must be recognized as a normal source via its extension,
    // NOT via header_mode. With the old bug, header_mode stayed true and
    // both pch.h and main.cpp were header-mode sources.
    match &result {
        ParsedInvocation::MultiFile { compilations, .. } => {
            assert_eq!(compilations.len(), 2);
            // pch.h picked up in header_mode
            assert_eq!(compilations[0].source_file, NormalizedPath::new("pch.h"));
            // main.cpp picked up by extension after reset
            assert_eq!(compilations[1].source_file, NormalizedPath::new("main.cpp"));
        }
        other => panic!("expected MultiFile, got: {other:?}"),
    }
}

#[test]
fn sticky_header_mode_non_source_not_captured_after_reset() {
    // BUG: with sticky header_mode, a positional arg like "README.txt"
    // after `-x c++` would be treated as a source file because
    // `is_source_file(arg) || header_mode` was true. After fix,
    // header_mode is false after `-x c++`, so non-source extensions
    // are correctly ignored.
    let result = parse_invocation(
        "clang++",
        &args(&["-x", "c++-header", "pch.h", "-x", "c++", "-c", "main.cpp"]),
    );
    match &result {
        ParsedInvocation::MultiFile { compilations, .. } => {
            assert_eq!(compilations.len(), 2);
            assert_eq!(compilations[0].source_file, NormalizedPath::new("pch.h"));
            assert_eq!(compilations[1].source_file, NormalizedPath::new("main.cpp"));
        }
        other => panic!("expected MultiFile, got: {other:?}"),
    }
}

#[test]
fn sticky_header_mode_no_c_flag_after_reset_is_non_cacheable() {
    // BUG: with sticky header_mode, the `!has_c_flag && !header_mode`
    // check at the end would pass (header_mode was still true),
    // making a link invocation appear cacheable. After fix, `-x c++`
    // resets header_mode so without -c it's correctly non-cacheable.
    let result = parse_invocation(
        "clang++",
        &args(&["-x", "c++-header", "-x", "c++", "main.cpp", "-o", "main"]),
    );
    assert!(
        matches!(result, ParsedInvocation::NonCacheable { .. }),
        "after -x c++ reset, no -c should be non-cacheable, got: {result:?}"
    );
}

#[test]
fn header_unit_c_is_cacheable() {
    // `-x c-header-unit` activates header-unit mode (C++20 module support).
    // Header-unit mode implies compilation, producing .pcm output.
    let result = parse_invocation(
        "clang++",
        &args(&["-x", "c-header-unit", "foo.h", "-o", "foo.pcm"]),
    );
    match result {
        ParsedInvocation::Cacheable(c) => {
            assert_eq!(c.source_file, NormalizedPath::new("foo.h"));
            assert_eq!(c.output_file, NormalizedPath::new("foo.pcm"));
        }
        other => panic!("expected cacheable, got: {other:?}"),
    }
}

#[test]
fn header_unit_cpp_is_cacheable() {
    // `-x c++-header-unit` activates header-unit mode (C++20 module support).
    let result = parse_invocation(
        "clang++",
        &args(&["-x", "c++-header-unit", "foo.h", "-o", "foo.pcm"]),
    );
    match result {
        ParsedInvocation::Cacheable(c) => {
            assert_eq!(c.source_file, NormalizedPath::new("foo.h"));
            assert_eq!(c.output_file, NormalizedPath::new("foo.pcm"));
        }
        other => panic!("expected cacheable, got: {other:?}"),
    }
}
