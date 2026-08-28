//! `detect_family` + `supports_depfile` for clang/gcc/msvc/emcc.

use super::super::{detect_family, is_msvc_cl, CompilerFamily};

#[test]
fn detect_clang_family() {
    assert_eq!(detect_family("clang++"), CompilerFamily::Clang);
    assert_eq!(detect_family("/usr/bin/clang"), CompilerFamily::Clang);
    assert_eq!(detect_family("gcc"), CompilerFamily::Gcc);
    assert_eq!(detect_family("g++"), CompilerFamily::Gcc);
}

#[test]
fn detect_bare_cc_falls_through_to_gcc() {
    // Issue #391: the `zccache cc` subcommand routes invocations through
    // the existing wrap path, which relies on `detect_family("cc")` to
    // classify bare `cc` as a Gcc-style frontend. cc-rs build scripts
    // (libsqlite3-sys etc.) spawn whatever `$CC` points to, often the
    // platform's `cc` symlink — we must classify it.
    assert_eq!(detect_family("cc"), CompilerFamily::Gcc);
    assert_eq!(detect_family("/usr/bin/cc"), CompilerFamily::Gcc);
    assert_eq!(detect_family("cc.exe"), CompilerFamily::Gcc);
}

#[test]
fn detect_emcc_family() {
    assert_eq!(detect_family("emcc"), CompilerFamily::Clang);
    assert_eq!(detect_family("em++"), CompilerFamily::Clang);
    assert_eq!(detect_family("/usr/bin/emcc"), CompilerFamily::Clang);
    assert_eq!(detect_family("emcc.exe"), CompilerFamily::Clang);
    // emcc supports -MD -MF (same as clang)
    assert!(CompilerFamily::Clang.supports_depfile());
}

#[test]
fn detect_msvc_family() {
    assert_eq!(detect_family("cl"), CompilerFamily::Msvc);
    assert_eq!(detect_family("C:\\MSVC\\cl"), CompilerFamily::Msvc);
}

#[test]
fn detect_msvc_case_insensitive() {
    // MSVC cl.exe is commonly invoked in uppercase on Windows.
    // Bug: detect_family used case-sensitive `name == "cl"`, so
    // CL.EXE was misclassified as Gcc.
    assert_eq!(detect_family("CL"), CompilerFamily::Msvc);
    assert_eq!(detect_family("CL.EXE"), CompilerFamily::Msvc);
    assert_eq!(detect_family("Cl.exe"), CompilerFamily::Msvc);
    assert_eq!(detect_family("C:\\MSVC\\CL.EXE"), CompilerFamily::Msvc);
    assert_eq!(
        detect_family("C:\\Program Files\\MSVC\\cl.EXE"),
        CompilerFamily::Msvc
    );
}

#[test]
fn gcc_supports_depfile() {
    assert!(CompilerFamily::Gcc.supports_depfile());
}

#[test]
fn clang_supports_depfile() {
    assert!(CompilerFamily::Clang.supports_depfile());
}

#[test]
fn msvc_no_depfile() {
    assert!(!CompilerFamily::Msvc.supports_depfile());
}

// --- Issue #1530: cl.exe vs clang-cl for system-include discovery ---------

#[test]
fn is_msvc_cl_matches_only_microsofts_driver() {
    for compiler in [
        "cl",
        "cl.exe",
        "CL.EXE",
        "Cl.exe",
        r"C:\Program Files\MSVC\bin\cl.EXE",
        "/mnt/c/MSVC/cl",
    ] {
        assert!(is_msvc_cl(compiler), "expected cl.exe: {compiler}");
    }
}

#[test]
fn is_msvc_cl_excludes_clang_cl_and_others() {
    // clang-cl parses MSVC syntax (family Msvc) but is a clang driver: it
    // answers the `-v -E` include probe, so it must keep using it.
    for compiler in [
        "clang-cl",
        "clang-cl.exe",
        r"C:\LLVM\bin\clang-cl.exe",
        "clang-cl-17",
        "gcc",
        "clang",
        "cl-wrapper",
        "mycl.exe",
    ] {
        assert!(!is_msvc_cl(compiler), "expected not cl.exe: {compiler}");
    }
}
