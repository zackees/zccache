//! `detect_family` + `supports_depfile` for clang/gcc/msvc/emcc.

use crate::{detect_family, CompilerFamily};

#[test]
fn detect_clang_family() {
    assert_eq!(detect_family("clang++"), CompilerFamily::Clang);
    assert_eq!(detect_family("/usr/bin/clang"), CompilerFamily::Clang);
    assert_eq!(detect_family("gcc"), CompilerFamily::Gcc);
    assert_eq!(detect_family("g++"), CompilerFamily::Gcc);
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
