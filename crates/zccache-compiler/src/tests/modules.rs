//! C++20 modules: .cppm/.ixx, -x c++-module, header units, --precompile, GCC -fmodules-ts.

use super::args;
use crate::{parse_invocation, ParsedInvocation};
use zccache_core::NormalizedPath;

// Group A: Source extension recognition (.cppm, .ixx)

#[test]
fn cppm_extension_is_cacheable() {
    let result = parse_invocation("clang++", &args(&["-c", "module.cppm", "-o", "module.pcm"]));
    match result {
        ParsedInvocation::Cacheable(c) => {
            assert_eq!(c.source_file, NormalizedPath::new("module.cppm"));
            assert_eq!(c.output_file, NormalizedPath::new("module.pcm"));
        }
        other => panic!("expected cacheable, got: {other:?}"),
    }
}

#[test]
fn ixx_extension_is_cacheable() {
    let result = parse_invocation("g++", &args(&["-c", "module.ixx", "-o", "module.o"]));
    match result {
        ParsedInvocation::Cacheable(c) => {
            assert_eq!(c.source_file, NormalizedPath::new("module.ixx"));
            assert_eq!(c.output_file, NormalizedPath::new("module.o"));
        }
        other => panic!("expected cacheable, got: {other:?}"),
    }
}

#[test]
fn cppm_default_output_with_precompile_is_pcm() {
    // --precompile without -o should produce stem.pcm
    let result = parse_invocation("clang++", &args(&["--precompile", "module.cppm"]));
    match result {
        ParsedInvocation::Cacheable(c) => {
            assert_eq!(c.source_file, NormalizedPath::new("module.cppm"));
            assert_eq!(c.output_file, NormalizedPath::new("module.pcm"));
        }
        other => panic!("expected cacheable, got: {other:?}"),
    }
}

#[test]
fn cppm_default_output_with_c_flag_is_object() {
    // -c on a .cppm without -o should produce stem.o (normal object)
    let result = parse_invocation("clang++", &args(&["-c", "module.cppm"]));
    match result {
        ParsedInvocation::Cacheable(c) => {
            assert_eq!(c.source_file, NormalizedPath::new("module.cppm"));
            assert_eq!(c.output_file, NormalizedPath::new("module.o"));
        }
        other => panic!("expected cacheable, got: {other:?}"),
    }
}

#[test]
fn cppm_multi_file() {
    let result = parse_invocation("clang++", &args(&["-c", "a.cppm", "b.cppm"]));
    match result {
        ParsedInvocation::MultiFile { compilations, .. } => {
            assert_eq!(compilations.len(), 2);
            assert_eq!(compilations[0].source_file, NormalizedPath::new("a.cppm"));
            assert_eq!(compilations[1].source_file, NormalizedPath::new("b.cppm"));
        }
        other => panic!("expected MultiFile, got: {other:?}"),
    }
}

// Group B: -x c++-module language mode

#[test]
fn x_cpp_module_with_precompile_is_cacheable() {
    let result = parse_invocation(
        "clang++",
        &args(&[
            "-x",
            "c++-module",
            "--precompile",
            "interface.cpp",
            "-o",
            "interface.pcm",
        ]),
    );
    match result {
        ParsedInvocation::Cacheable(c) => {
            assert_eq!(c.source_file, NormalizedPath::new("interface.cpp"));
            assert_eq!(c.output_file, NormalizedPath::new("interface.pcm"));
        }
        other => panic!("expected cacheable, got: {other:?}"),
    }
}

#[test]
fn x_cpp_module_with_c_flag_is_cacheable() {
    let result = parse_invocation(
        "clang++",
        &args(&[
            "-x",
            "c++-module",
            "-c",
            "interface.cpp",
            "-o",
            "interface.o",
        ]),
    );
    match result {
        ParsedInvocation::Cacheable(c) => {
            assert_eq!(c.source_file, NormalizedPath::new("interface.cpp"));
            assert_eq!(c.output_file, NormalizedPath::new("interface.o"));
        }
        other => panic!("expected cacheable, got: {other:?}"),
    }
}

#[test]
fn x_cpp_module_without_c_or_precompile_is_non_cacheable() {
    // Module mode alone does NOT imply compilation (unlike header mode).
    // Without -c or --precompile, this is a link invocation.
    let result = parse_invocation(
        "clang++",
        &args(&["-x", "c++-module", "interface.cpp", "-o", "interface"]),
    );
    assert!(
        matches!(result, ParsedInvocation::NonCacheable { .. }),
        "-x c++-module without -c or --precompile should be non-cacheable, got: {result:?}"
    );
}

#[test]
fn x_cpp_module_accepts_non_source_extension() {
    // -x c++-module should allow any positional arg as a source file
    // (same behavior as -x c++-header with non-standard extensions).
    let result = parse_invocation(
        "clang++",
        &args(&["-x", "c++-module", "--precompile", "interface.mpp"]),
    );
    match result {
        ParsedInvocation::Cacheable(c) => {
            assert_eq!(c.source_file, NormalizedPath::new("interface.mpp"));
        }
        other => panic!("expected cacheable, got: {other:?}"),
    }
}

#[test]
fn x_cpp_module_default_output_precompile() {
    // --precompile without -o → stem.pcm
    let result = parse_invocation(
        "clang++",
        &args(&["-x", "c++-module", "--precompile", "interface.cpp"]),
    );
    match result {
        ParsedInvocation::Cacheable(c) => {
            assert_eq!(c.output_file, NormalizedPath::new("interface.pcm"));
        }
        other => panic!("expected cacheable, got: {other:?}"),
    }
}

#[test]
fn x_cpp_module_default_output_c_flag() {
    // -c without -o → stem.o (even in module mode)
    let result = parse_invocation(
        "clang++",
        &args(&["-x", "c++-module", "-c", "interface.cpp"]),
    );
    match result {
        ParsedInvocation::Cacheable(c) => {
            assert_eq!(c.output_file, NormalizedPath::new("interface.o"));
        }
        other => panic!("expected cacheable, got: {other:?}"),
    }
}

#[test]
fn x_cpp_module_reset_by_x_cpp() {
    // -x c++ resets module mode, just like it resets header mode.
    let result = parse_invocation(
        "clang++",
        &args(&[
            "-x",
            "c++-module",
            "--precompile",
            "interface.mpp",
            "-x",
            "c++",
            "-c",
            "main.cpp",
        ]),
    );
    match result {
        ParsedInvocation::MultiFile { compilations, .. } => {
            assert_eq!(compilations.len(), 2);
            assert_eq!(
                compilations[0].source_file,
                NormalizedPath::new("interface.mpp")
            );
            assert_eq!(compilations[1].source_file, NormalizedPath::new("main.cpp"));
        }
        other => panic!("expected MultiFile, got: {other:?}"),
    }
}

#[test]
fn x_cpp_module_implies_compilation_with_precompile() {
    // --precompile without -c is still a cacheable compilation.
    let result = parse_invocation(
        "clang++",
        &args(&[
            "-x",
            "c++-module",
            "--precompile",
            "interface.cpp",
            "-o",
            "interface.pcm",
        ]),
    );
    match result {
        ParsedInvocation::Cacheable(c) => {
            assert_eq!(c.source_file, NormalizedPath::new("interface.cpp"));
            assert_eq!(c.output_file, NormalizedPath::new("interface.pcm"));
        }
        other => panic!("expected cacheable, got: {other:?}"),
    }
}

// Group C: Header units (-x c++-header-unit / -x c-header-unit)

#[test]
fn x_cpp_header_unit_with_precompile_is_cacheable() {
    let result = parse_invocation(
        "clang++",
        &args(&[
            "-x",
            "c++-header-unit",
            "--precompile",
            "foo.h",
            "-o",
            "foo.pcm",
        ]),
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
fn x_c_header_unit_with_c_flag_is_cacheable() {
    let result = parse_invocation(
        "gcc",
        &args(&["-x", "c-header-unit", "-c", "foo.h", "-o", "foo.pcm"]),
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
fn x_cpp_header_unit_default_output_is_pcm() {
    // Header unit without -o → filename.pcm
    let result = parse_invocation(
        "clang++",
        &args(&["-x", "c++-header-unit", "--precompile", "foo.h"]),
    );
    match result {
        ParsedInvocation::Cacheable(c) => {
            assert_eq!(c.output_file, NormalizedPath::new("foo.h.pcm"));
        }
        other => panic!("expected cacheable, got: {other:?}"),
    }
}

#[test]
fn x_cpp_header_unit_implies_compilation() {
    // Header-unit mode implies compilation (no -c needed), like header mode.
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

// Group D: --precompile flag handling

#[test]
fn precompile_on_normal_cpp_is_cacheable() {
    // --precompile on a .cpp (with export module inside) is valid.
    let result = parse_invocation("clang++", &args(&["--precompile", "foo.cpp"]));
    match result {
        ParsedInvocation::Cacheable(c) => {
            assert_eq!(c.source_file, NormalizedPath::new("foo.cpp"));
            assert_eq!(c.output_file, NormalizedPath::new("foo.pcm"));
        }
        other => panic!("expected cacheable, got: {other:?}"),
    }
}

#[test]
fn precompile_without_source_is_non_cacheable() {
    let result = parse_invocation("clang++", &args(&["--precompile", "-O2"]));
    assert!(
        matches!(result, ParsedInvocation::NonCacheable { .. }),
        "--precompile without source should be non-cacheable, got: {result:?}"
    );
}

#[test]
fn precompile_and_c_flag_together() {
    // Both --precompile and -c can coexist. --precompile takes precedence
    // for default output (produces .pcm, not .o).
    let result = parse_invocation(
        "clang++",
        &args(&["--precompile", "-c", "module.cppm", "-o", "module.pcm"]),
    );
    match result {
        ParsedInvocation::Cacheable(c) => {
            assert_eq!(c.source_file, NormalizedPath::new("module.cppm"));
            assert_eq!(c.output_file, NormalizedPath::new("module.pcm"));
        }
        other => panic!("expected cacheable, got: {other:?}"),
    }
}

// Group E: GCC -fmodules-ts interaction

#[test]
fn gcc_fmodules_ts_with_cppm_is_cacheable() {
    // -fmodules-ts falls through to unknown_flags, which is fine.
    let result = parse_invocation(
        "g++",
        &args(&["-fmodules-ts", "-c", "module.cppm", "-o", "module.o"]),
    );
    match result {
        ParsedInvocation::Cacheable(c) => {
            assert_eq!(c.source_file, NormalizedPath::new("module.cppm"));
            assert_eq!(c.output_file, NormalizedPath::new("module.o"));
            assert!(c.unknown_flags.contains(&"-fmodules-ts".to_string()));
        }
        other => panic!("expected cacheable, got: {other:?}"),
    }
}

#[test]
fn gcc_fmodules_ts_with_x_module_precompile() {
    let result = parse_invocation(
        "g++",
        &args(&[
            "-fmodules-ts",
            "-x",
            "c++-module",
            "--precompile",
            "interface.cpp",
        ]),
    );
    match result {
        ParsedInvocation::Cacheable(c) => {
            assert_eq!(c.source_file, NormalizedPath::new("interface.cpp"));
            assert_eq!(c.output_file, NormalizedPath::new("interface.pcm"));
        }
        other => panic!("expected cacheable, got: {other:?}"),
    }
}
