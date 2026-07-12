//! Compiler-driver-as-linker tests (`gcc`, `clang`, `emcc`, ...).

use super::super::{parse_linker_invocation, LinkerFamily, ParsedLinkerInvocation};
use super::args;
use zccache_core::NormalizedPath;

#[test]
fn wasm_ld_cacheable() {
    let result = parse_linker_invocation("wasm-ld", args(&["-o", "output.wasm", "a.o", "b.o"]));
    match result {
        ParsedLinkerInvocation::Cacheable(c) => {
            assert_eq!(c.family, LinkerFamily::Lld);
            assert_eq!(c.output_file, NormalizedPath::new("output.wasm"));
            assert_eq!(c.input_files.len(), 2);
        }
        other => panic!("expected cacheable, got: {other:?}"),
    }
}

#[test]
fn emcc_link_cacheable() {
    let result = parse_linker_invocation("emcc", args(&["-o", "output.js", "a.o", "b.o"]));
    match result {
        ParsedLinkerInvocation::Cacheable(c) => {
            assert_eq!(c.family, LinkerFamily::CompilerDriver);
            assert_eq!(c.output_file, NormalizedPath::new("output.js"));
            assert_eq!(c.input_files.len(), 2);
        }
        other => panic!("expected cacheable, got: {other:?}"),
    }
}

#[test]
fn gcc_shared_basic() {
    let result =
        parse_linker_invocation("gcc", args(&["-shared", "-o", "libfoo.so", "a.o", "b.o"]));
    match result {
        ParsedLinkerInvocation::Cacheable(c) => {
            assert_eq!(c.family, LinkerFamily::CompilerDriver);
            assert_eq!(c.output_file, NormalizedPath::new("libfoo.so"));
            assert_eq!(c.input_files.len(), 2);
            assert!(!c.non_deterministic);
        }
        other => panic!("expected cacheable, got: {other:?}"),
    }
}

#[test]
fn clang_shared_dll() {
    let result =
        parse_linker_invocation("clang", args(&["-shared", "-o", "foo.dll", "a.o", "b.o"]));
    match result {
        ParsedLinkerInvocation::Cacheable(c) => {
            assert_eq!(c.family, LinkerFamily::CompilerDriver);
            assert_eq!(c.output_file, NormalizedPath::new("foo.dll"));
        }
        other => panic!("expected cacheable, got: {other:?}"),
    }
}

#[test]
fn gpp_shared_with_flags() {
    let result = parse_linker_invocation(
        "g++",
        args(&["-shared", "-fPIC", "-O2", "-o", "libfoo.so", "a.o", "-lm"]),
    );
    match result {
        ParsedLinkerInvocation::Cacheable(c) => {
            assert_eq!(c.input_files, vec![NormalizedPath::new("a.o")]);
            assert!(c.cache_relevant_flags.contains(&"-fPIC".to_string()));
            assert!(c.cache_relevant_flags.contains(&"-O2".to_string()));
            assert!(c.cache_relevant_flags.contains(&"-lm".to_string()));
        }
        other => panic!("expected cacheable, got: {other:?}"),
    }
}

#[test]
fn gcc_shared_wl_build_id_uuid_non_deterministic() {
    let result = parse_linker_invocation(
        "gcc",
        args(&["-shared", "-Wl,--build-id=uuid", "-o", "libfoo.so", "a.o"]),
    );
    match result {
        ParsedLinkerInvocation::Cacheable(c) => {
            assert!(c.non_deterministic, "-Wl,--build-id=uuid must be flagged");
        }
        other => panic!("expected cacheable, got: {other:?}"),
    }
}

#[test]
fn gcc_with_compile_flag_non_cacheable() {
    // gcc -c -shared means compile only (not link), should not be cached as link
    let result = parse_linker_invocation("gcc", args(&["-c", "-shared", "-o", "foo.o", "foo.c"]));
    assert!(matches!(
        result,
        ParsedLinkerInvocation::NonCacheable { .. }
    ));
}

#[test]
fn gcc_exe_cacheable() {
    // gcc without -shared is executable linking — cacheable
    let result = parse_linker_invocation("gcc", args(&["-o", "a.out", "main.o"]));
    match result {
        ParsedLinkerInvocation::Cacheable(c) => {
            assert_eq!(c.family, LinkerFamily::CompilerDriver);
            assert_eq!(c.output_file, NormalizedPath::new("a.out"));
            assert_eq!(c.input_files, vec![NormalizedPath::new("main.o")]);
            assert!(!c.non_deterministic);
        }
        other => panic!("expected cacheable, got: {other:?}"),
    }
}

#[test]
fn gcc_shared_no_output_non_cacheable() {
    let result = parse_linker_invocation("gcc", args(&["-shared", "a.o"]));
    assert!(matches!(
        result,
        ParsedLinkerInvocation::NonCacheable { .. }
    ));
}

#[test]
fn gcc_shared_no_object_inputs_non_cacheable() {
    // Source files (.c) are not valid linker inputs — need pre-compiled .o
    let result = parse_linker_invocation("gcc", args(&["-shared", "-o", "libfoo.so", "foo.c"]));
    assert!(matches!(
        result,
        ParsedLinkerInvocation::NonCacheable { .. }
    ));
}

#[test]
fn cross_compile_gcc() {
    let result = parse_linker_invocation(
        "x86_64-w64-mingw32-gcc",
        args(&["-shared", "-o", "foo.dll", "a.o"]),
    );
    match result {
        ParsedLinkerInvocation::Cacheable(c) => {
            assert_eq!(c.family, LinkerFamily::CompilerDriver);
            assert_eq!(c.tool, NormalizedPath::new("x86_64-w64-mingw32-gcc"));
        }
        other => panic!("expected cacheable, got: {other:?}"),
    }
}

#[test]
fn gcc_shared_with_wl_soname() {
    let result = parse_linker_invocation(
        "gcc",
        args(&[
            "-shared",
            "-Wl,-soname,libfoo.so.1",
            "-o",
            "libfoo.so.1.0",
            "a.o",
        ]),
    );
    match result {
        ParsedLinkerInvocation::Cacheable(c) => {
            assert!(c
                .cache_relevant_flags
                .contains(&"-Wl,-soname,libfoo.so.1".to_string()));
        }
        other => panic!("expected cacheable, got: {other:?}"),
    }
}

#[test]
fn gcc_no_secondary_outputs() {
    let result = parse_linker_invocation("gcc", args(&["-shared", "-o", "libfoo.so", "a.o"]));
    match result {
        ParsedLinkerInvocation::Cacheable(c) => {
            assert!(c.secondary_outputs.is_empty());
        }
        other => panic!("expected cacheable, got: {other:?}"),
    }
}

#[test]
fn gcc_driver_declares_wl_map_and_dependency_outputs() {
    let result = parse_linker_invocation(
        "gcc",
        args(&[
            "-o",
            "app",
            "-Wl,-Map,reports/app.map,--dependency-file=deps/app.d",
            "main.o",
        ]),
    );
    let ParsedLinkerInvocation::Cacheable(c) = result else {
        panic!("expected cacheable")
    };
    assert_eq!(
        c.secondary_outputs,
        vec![
            NormalizedPath::new("reports/app.map"),
            NormalizedPath::new("deps/app.d"),
        ]
    );
}

#[test]
fn clang_driver_declares_apple_semantic_destinations() {
    let result = parse_linker_invocation(
        "clang",
        args(&[
            "-o",
            "app",
            "-Wl,-map,reports/app.map,-dependency_info,deps/app.dat",
            "main.o",
        ]),
    );
    let ParsedLinkerInvocation::Cacheable(c) = result else {
        panic!("expected cacheable")
    };
    assert_eq!(
        c.secondary_outputs,
        vec![
            NormalizedPath::new("reports/app.map"),
            NormalizedPath::new("deps/app.dat"),
        ]
    );
}
