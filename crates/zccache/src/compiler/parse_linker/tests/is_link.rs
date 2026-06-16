//! `is_link_invocation` classification tests.

use super::args;
use super::super::is_link_invocation;

#[test]
fn is_link_invocation_direct_linker() {
    // Direct linkers are always link invocations (args don't matter for detection)
    assert!(is_link_invocation(
        "ld",
        &args(&["-shared", "-o", "foo.so", "a.o"])
    ));
    assert!(is_link_invocation("ld", &args(&["-o", "a.out", "main.o"])));
    assert!(is_link_invocation(
        "link.exe",
        &args(&["/DLL", "/OUT:foo.dll", "a.obj"])
    ));
}

#[test]
fn is_link_invocation_compiler_driver_shared() {
    assert!(is_link_invocation(
        "gcc",
        &args(&["-shared", "-o", "foo.so", "a.o"])
    ));
    assert!(is_link_invocation(
        "clang++",
        &args(&["-shared", "-o", "foo.so", "a.o"])
    ));
}

#[test]
fn is_link_invocation_compiler_not_shared() {
    // gcc -c is compilation, NOT a link invocation
    assert!(!is_link_invocation(
        "gcc",
        &args(&["-c", "foo.c", "-o", "foo.o"])
    ));
    // gcc -E is preprocessing, NOT a link invocation
    assert!(!is_link_invocation("gcc", &args(&["-E", "foo.c"])));
    // gcc -S is assembly generation, NOT a link invocation
    assert!(!is_link_invocation("gcc", &args(&["-S", "foo.c"])));
    // gcc -o a.out main.o IS a link invocation (exe link)
    assert!(is_link_invocation("gcc", &args(&["-o", "a.out", "main.o"])));
}

#[test]
fn is_link_invocation_pch_generation_not_link() {
    // PCH generation with -x c++-header is compilation, NOT linking
    assert!(!is_link_invocation(
        "clang++",
        &args(&["-x", "c++-header", "header.h", "-o", "header.pch"])
    ));
    assert!(!is_link_invocation(
        "gcc",
        &args(&["-x", "c-header", "stdafx.h", "-o", "stdafx.h.gch"])
    ));
    // Cross-compiler with "clang" in the name
    assert!(!is_link_invocation(
        "ctc-clang++",
        &args(&[
            "-x",
            "c++-header",
            "FastLED.h",
            "-o",
            "FastLED.h.pch",
            "-fPIC",
            "-Iinclude",
        ])
    ));
    // With -c AND -x c++-header — still not a link
    assert!(!is_link_invocation(
        "clang++",
        &args(&["-x", "c++-header", "-c", "header.h", "-o", "header.pch"])
    ));
}

#[test]
fn is_link_header_and_module_modes_not_link() {
    // All `-x` language modes that imply compilation should NOT be link invocations.
    // Header (PCH):
    assert!(!is_link_invocation(
        "clang++",
        &args(&["-x", "c-header", "foo.h", "-o", "foo.gch"])
    ));
    assert!(!is_link_invocation(
        "clang++",
        &args(&["-x", "c++-header", "foo.h", "-o", "foo.pch"])
    ));
    // Header unit (C++20):
    assert!(!is_link_invocation(
        "clang++",
        &args(&["-x", "c-header-unit", "foo.h", "-o", "foo.pcm"])
    ));
    assert!(!is_link_invocation(
        "clang++",
        &args(&["-x", "c++-header-unit", "foo.h", "-o", "foo.pcm"])
    ));
    // Module mode does NOT imply compilation — without -c/--precompile, it's still a link.
    assert!(is_link_invocation(
        "clang++",
        &args(&["-x", "c++-module", "interface.cpp", "-o", "interface"])
    ));
    // --precompile is also not a link invocation:
    assert!(!is_link_invocation(
        "clang++",
        &args(&["--precompile", "module.cppm", "-o", "module.pcm"])
    ));
}

#[test]
fn is_link_invocation_unknown_tool() {
    assert!(!is_link_invocation(
        "rustc",
        &args(&["-shared", "-o", "foo.so"])
    ));
}

#[test]
fn is_link_invocation_c_flag_in_response_file() {
    // When -c is inside a response file (e.g. fbuild on Windows puts all
    // flags in @response.rsp), is_link_invocation must expand the response
    // file to find the -c flag and correctly classify it as compilation.
    use std::io::Write;
    let mut rsp = tempfile::NamedTempFile::new().unwrap();
    writeln!(rsp, "-O2 -Wall -c foo.cpp -o foo.o").unwrap();

    let rsp_arg = format!("@{}", rsp.path().display());
    // Without response file expansion, this would incorrectly return true
    assert!(
        !is_link_invocation("gcc", &args(&[&rsp_arg])),
        "-c inside response file must be detected as compilation, not link"
    );
    assert!(
        !is_link_invocation("xtensa-esp32s3-elf-g++", &args(&[&rsp_arg])),
        "xtensa cross-compiler with -c in response file must not be classified as link"
    );
}

#[test]
fn is_link_invocation_response_file_without_c_flag() {
    // A response file that contains link flags (no -c) should still be link
    use std::io::Write;
    let mut rsp = tempfile::NamedTempFile::new().unwrap();
    writeln!(rsp, "-O2 -o a.out main.o").unwrap();

    let rsp_arg = format!("@{}", rsp.path().display());
    assert!(
        is_link_invocation("gcc", &args(&[&rsp_arg])),
        "response file without -c should be classified as link"
    );
}
