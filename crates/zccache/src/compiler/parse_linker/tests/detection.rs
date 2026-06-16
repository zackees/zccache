//! Detection tests: `detect_family`, `is_linker`, `is_compiler_driver`.

use super::args;
use super::super::detect::{detect_family, is_compiler_driver};
use super::super::{is_link_invocation, is_linker, LinkerFamily};

#[test]
fn detect_gnu_ld() {
    assert_eq!(detect_family("ld"), Some(LinkerFamily::Ld));
    assert_eq!(detect_family("/usr/bin/ld"), Some(LinkerFamily::Ld));
    assert_eq!(detect_family("ld.bfd"), Some(LinkerFamily::Ld));
    assert_eq!(detect_family("ld.gold"), Some(LinkerFamily::Ld));
    assert_eq!(detect_family("x86_64-linux-gnu-ld"), Some(LinkerFamily::Ld));
    assert_eq!(
        detect_family("aarch64-linux-gnu-ld"),
        Some(LinkerFamily::Ld)
    );
}

#[test]
fn detect_llvm_lld() {
    assert_eq!(detect_family("lld"), Some(LinkerFamily::Lld));
    assert_eq!(detect_family("lld-17"), Some(LinkerFamily::Lld));
    assert_eq!(detect_family("ld.lld"), Some(LinkerFamily::Lld));
    assert_eq!(detect_family("ld.lld-17"), Some(LinkerFamily::Lld));
    assert_eq!(detect_family("/usr/bin/lld"), Some(LinkerFamily::Lld));
}

#[test]
fn detect_wasm_ld() {
    assert_eq!(detect_family("wasm-ld"), Some(LinkerFamily::Lld));
    assert_eq!(detect_family("wasm-ld.exe"), Some(LinkerFamily::Lld));
    assert_eq!(detect_family("/usr/bin/wasm-ld"), Some(LinkerFamily::Lld));
    assert_eq!(
        detect_family("C:\\emsdk\\upstream\\bin\\wasm-ld.exe"),
        Some(LinkerFamily::Lld)
    );
}

#[test]
fn detect_msvc_link() {
    assert_eq!(detect_family("link"), Some(LinkerFamily::MsvcLink));
    assert_eq!(detect_family("link.exe"), Some(LinkerFamily::MsvcLink));
    assert_eq!(detect_family("LINK"), Some(LinkerFamily::MsvcLink));
    assert_eq!(detect_family("LINK.EXE"), Some(LinkerFamily::MsvcLink));
}

#[test]
fn detect_unknown_tool() {
    assert_eq!(detect_family("gcc"), None);
    assert_eq!(detect_family("clang"), None);
    assert_eq!(detect_family("ar"), None);
    assert_eq!(detect_family("lib.exe"), None);
}

#[test]
fn is_linker_works() {
    assert!(is_linker("ld"));
    assert!(is_linker("lld"));
    assert!(is_linker("link.exe"));
    assert!(!is_linker("gcc"));
    assert!(!is_linker("ar"));
    assert!(!is_linker("lib.exe"));
}

#[test]
fn compiler_driver_detection() {
    assert!(is_compiler_driver("gcc"));
    assert!(is_compiler_driver("g++"));
    assert!(is_compiler_driver("clang"));
    assert!(is_compiler_driver("clang++"));
    assert!(is_compiler_driver("clang-17"));
    assert!(is_compiler_driver("cc"));
    assert!(is_compiler_driver("c++"));
    assert!(is_compiler_driver("/usr/bin/gcc"));
    assert!(is_compiler_driver("x86_64-w64-mingw32-gcc"));
    assert!(is_compiler_driver("x86_64-w64-mingw32-g++"));
    assert!(is_compiler_driver("emcc"));
    assert!(is_compiler_driver("em++"));
    assert!(is_compiler_driver("/usr/bin/emcc"));
    assert!(!is_compiler_driver("ld"));
    assert!(!is_compiler_driver("ar"));
    assert!(!is_compiler_driver("rustc"));
}

#[test]
fn is_link_invocation_emcc() {
    // emcc without -c is a link invocation (compiler driver linking)
    assert!(is_link_invocation(
        "emcc",
        &args(&["-o", "output.js", "a.o", "b.o"])
    ));
    assert!(is_link_invocation(
        "em++",
        &args(&["-o", "output.html", "main.o"])
    ));
    // emcc with -c is NOT a link invocation (compile only)
    assert!(!is_link_invocation(
        "emcc",
        &args(&["-c", "foo.c", "-o", "foo.o"])
    ));
}
