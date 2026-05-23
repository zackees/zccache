//! Default output paths, PCH naming, concatenated -o, unknown-flag preservation, BUG_LINKER repro.

use super::args;
use super::super::{parse_invocation, ParsedInvocation};
use zccache::core::NormalizedPath;

// ─── PCH default output tests ─────────────────────────────────────────

#[test]
fn pch_default_output_clang() {
    // `clang++ -x c++-header src/pch.h` → output `pch.h.pch` (filename only, no dir)
    let result = parse_invocation("clang++", &args(&["-x", "c++-header", "src/pch.h"]));
    match result {
        ParsedInvocation::Cacheable(c) => {
            assert_eq!(c.source_file, NormalizedPath::new("src/pch.h"));
            assert_eq!(c.output_file, NormalizedPath::new("pch.h.pch"));
        }
        other => panic!("expected cacheable, got: {other:?}"),
    }
}

#[test]
fn pch_default_output_gcc() {
    // `gcc -x c-header src/pch.h` → output `pch.h.gch` (filename only, no dir)
    let result = parse_invocation("gcc", &args(&["-x", "c-header", "src/pch.h"]));
    match result {
        ParsedInvocation::Cacheable(c) => {
            assert_eq!(c.source_file, NormalizedPath::new("src/pch.h"));
            assert_eq!(c.output_file, NormalizedPath::new("pch.h.gch"));
        }
        other => panic!("expected cacheable, got: {other:?}"),
    }
}

#[test]
fn pch_default_output_strips_directory() {
    // `clang++ -x c++-header src/fl/audio/fft/fft.h` → output uses filename only.
    // Regression: old code produced `src/fl/audio/fft/fft.h.pch`, causing spurious
    // PCH files to be written into the source tree during cache restoration.
    let result = parse_invocation(
        "clang++",
        &args(&["-x", "c++-header", "src/fl/audio/fft/fft.h"]),
    );
    match result {
        ParsedInvocation::Cacheable(c) => {
            assert_eq!(c.source_file, NormalizedPath::new("src/fl/audio/fft/fft.h"));
            assert_eq!(c.output_file, NormalizedPath::new("fft.h.pch"));
        }
        other => panic!("expected cacheable, got: {other:?}"),
    }
}

#[test]
fn pch_default_output_absolute_path_strips_to_filename() {
    // Absolute source path must also produce filename-only output.
    // Regression: old code produced `/abs/path/src/pch.h.pch` which
    // the daemon resolved as an absolute write into the source tree.
    let result = parse_invocation(
        "clang++",
        &args(&["-x", "c++-header", "/abs/path/src/pch.h"]),
    );
    match result {
        ParsedInvocation::Cacheable(c) => {
            assert_eq!(c.source_file, NormalizedPath::new("/abs/path/src/pch.h"));
            assert_eq!(c.output_file, NormalizedPath::new("pch.h.pch"));
        }
        other => panic!("expected cacheable, got: {other:?}"),
    }
}

#[test]
fn pch_default_output_explicit_o_unchanged() {
    // Explicit `-o` still honored — no change in behavior
    let result = parse_invocation(
        "clang++",
        &args(&["-x", "c++-header", "pch.h", "-o", "build/pch.h.pch"]),
    );
    match result {
        ParsedInvocation::Cacheable(c) => {
            assert_eq!(c.source_file, NormalizedPath::new("pch.h"));
            assert_eq!(c.output_file, NormalizedPath::new("build/pch.h.pch"));
        }
        other => panic!("expected cacheable, got: {other:?}"),
    }
}

#[test]
fn normal_compile_default_output_unchanged() {
    // Regression guard: normal compilation still defaults to stem.o
    let result = parse_invocation("gcc", &args(&["-c", "foo.cpp"]));
    match result {
        ParsedInvocation::Cacheable(c) => {
            assert_eq!(c.output_file, NormalizedPath::new("foo.o"));
        }
        other => panic!("expected cacheable, got: {other:?}"),
    }
}

// ─── Concatenated -o flag tests ───────────────────────────────────────

#[test]
fn concatenated_o_flag_parsed() {
    // `-obuild/foo.o` (no space) is valid for clang/gcc and must be recognized.
    let result = parse_invocation("clang", &args(&["-c", "foo.cpp", "-obuild/foo.o"]));
    match result {
        ParsedInvocation::Cacheable(c) => {
            assert_eq!(c.output_file, NormalizedPath::new("build/foo.o"));
        }
        other => panic!("expected cacheable, got: {other:?}"),
    }
}

#[test]
fn concatenated_o_flag_pch() {
    // PCH compilation with concatenated -o must preserve the build directory path.
    // This is the root cause of BUG_LINKER.md: `-opath` was silently dropped
    // as an unknown flag, causing default_output() to be used instead.
    let result = parse_invocation(
        "clang++",
        &args(&["-x", "c++-header", "pch.h", "-obuild/pch.h.pch"]),
    );
    match result {
        ParsedInvocation::Cacheable(c) => {
            assert_eq!(c.output_file, NormalizedPath::new("build/pch.h.pch"));
        }
        other => panic!("expected cacheable, got: {other:?}"),
    }
}

// ─── Unknown flags preservation tests ─────────────────────────────────

#[test]
fn all_flags_preserved() {
    // Every arg must be accounted for: either recognized by the parser
    // (source, output, known flag) or captured in unknown_flags.
    // Nothing is silently dropped.
    let input = args(&[
        "-c",
        "foo.cpp",
        "-o",
        "foo.o",
        "-Wall",
        "-Wextra",
        "-O2",
        "-Xclang",
        "-fno-spell-checking",
        "-std=c++17",
        "-DFOO=bar",
        "-I/usr/include",
        "-isystem",
        "/usr/local/include",
        "-unknown-future-flag",
    ]);
    let result = parse_invocation("clang++", &input);
    match result {
        ParsedInvocation::Cacheable(c) => {
            assert_eq!(c.source_file, NormalizedPath::new("foo.cpp"));
            assert_eq!(c.output_file, NormalizedPath::new("foo.o"));
            // Unknown flags are preserved, not dropped
            assert!(c.unknown_flags.contains(&"-Wall".to_string()));
            assert!(c.unknown_flags.contains(&"-Wextra".to_string()));
            assert!(c.unknown_flags.contains(&"-O2".to_string()));
            assert!(c
                .unknown_flags
                .contains(&"-unknown-future-flag".to_string()));
            // Concatenated known flags end up in unknown_flags
            // (parser only extracts -o value; -D/-I/-std with joined
            // values are not in FLAGS_WITH_VALUE so they go here)
            assert!(c.unknown_flags.contains(&"-std=c++17".to_string()));
            assert!(c.unknown_flags.contains(&"-DFOO=bar".to_string()));
            assert!(c.unknown_flags.contains(&"-I/usr/include".to_string()));
        }
        other => panic!("expected cacheable, got: {other:?}"),
    }
}

#[test]
fn xclang_value_not_misidentified_as_source() {
    // -Xclang takes the next arg as a pass-through value.
    // Without FLAGS_WITH_VALUE coverage, the value could be
    // misidentified as a source file.
    let result = parse_invocation(
        "clang++",
        &args(&[
            "-c",
            "foo.cpp",
            "-Xclang",
            "-fno-spell-checking",
            "-o",
            "foo.o",
        ]),
    );
    match result {
        ParsedInvocation::Cacheable(c) => {
            assert_eq!(c.source_file, NormalizedPath::new("foo.cpp"));
            // Only one source file — -fno-spell-checking must NOT be treated as source
        }
        other => panic!("expected cacheable, got: {other:?}"),
    }
}

#[test]
fn mllvm_value_not_misidentified_as_source() {
    let result = parse_invocation(
        "clang++",
        &args(&["-c", "foo.cpp", "-mllvm", "-some-llvm-opt", "-o", "foo.o"]),
    );
    match result {
        ParsedInvocation::Cacheable(c) => {
            assert_eq!(c.source_file, NormalizedPath::new("foo.cpp"));
        }
        other => panic!("expected cacheable, got: {other:?}"),
    }
}

// ─── PCH output path mismatch repro (BUG_LINKER.md) ──────────────────

#[test]
fn pch_output_path_mismatch_repro() {
    // Repro for BUG_LINKER.md: when no -o is provided for a nested
    // source header, default_output() returns filename-only which
    // doesn't match where clang actually writes (next to source).
    // The caller (build system) should always provide -o; the parser
    // must recognize all forms of -o (space-separated and concatenated).
    let result = parse_invocation(
        "clang++",
        &args(&["-x", "c++-header", "src/fl/fx/2d/flowfield_q31.h"]),
    );
    match result {
        ParsedInvocation::Cacheable(c) => {
            assert_eq!(c.output_file, NormalizedPath::new("flowfield_q31.h.pch"));
            assert_eq!(
                c.source_file,
                NormalizedPath::new("src/fl/fx/2d/flowfield_q31.h")
            );
        }
        other => panic!("expected cacheable, got: {other:?}"),
    }
}
