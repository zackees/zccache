//! GNU/LLD `--out-implib` secondary-output tests.

use super::super::{parse_linker_invocation, LinkerFamily, ParsedLinkerInvocation};
use super::args;
use crate::core::NormalizedPath;

#[test]
fn gnu_ld_out_implib_equals() {
    let result = parse_linker_invocation(
        "ld",
        args(&[
            "-shared",
            "--out-implib=libfoo.dll.a",
            "-o",
            "libfoo.dll",
            "a.o",
        ]),
    );
    match result {
        ParsedLinkerInvocation::Cacheable(c) => {
            assert_eq!(c.secondary_outputs.len(), 1);
            assert_eq!(c.secondary_outputs[0], NormalizedPath::new("libfoo.dll.a"));
        }
        other => panic!("expected cacheable, got: {other:?}"),
    }
}

#[test]
fn gnu_ld_out_implib_separate() {
    let result = parse_linker_invocation(
        "ld",
        args(&[
            "-shared",
            "--out-implib",
            "libfoo.dll.a",
            "-o",
            "libfoo.dll",
            "a.o",
        ]),
    );
    match result {
        ParsedLinkerInvocation::Cacheable(c) => {
            assert_eq!(c.secondary_outputs.len(), 1);
            assert_eq!(c.secondary_outputs[0], NormalizedPath::new("libfoo.dll.a"));
        }
        other => panic!("expected cacheable, got: {other:?}"),
    }
}

#[test]
fn gnu_ld_out_implib_with_path() {
    // Meson passes relative paths like ci/meson/native\fastled.dll.a
    let result = parse_linker_invocation(
        "ld",
        args(&[
            "-shared",
            "--out-implib=ci/meson/native/fastled.dll.a",
            "-o",
            "ci/meson/native/fastled.dll",
            "a.o",
        ]),
    );
    match result {
        ParsedLinkerInvocation::Cacheable(c) => {
            assert_eq!(c.secondary_outputs.len(), 1);
            assert_eq!(
                c.secondary_outputs[0],
                NormalizedPath::new("ci/meson/native/fastled.dll.a")
            );
        }
        other => panic!("expected cacheable, got: {other:?}"),
    }
}

#[test]
fn compiler_driver_wl_out_implib() {
    // clang++ -shared -Wl,--out-implib=foo.dll.a -o foo.dll a.o
    let result = parse_linker_invocation(
        "clang++",
        args(&[
            "-shared",
            "-Wl,--out-implib=foo.dll.a",
            "-o",
            "foo.dll",
            "a.o",
        ]),
    );
    match result {
        ParsedLinkerInvocation::Cacheable(c) => {
            assert_eq!(c.family, LinkerFamily::CompilerDriver);
            assert_eq!(c.secondary_outputs.len(), 1);
            assert_eq!(c.secondary_outputs[0], NormalizedPath::new("foo.dll.a"));
        }
        other => panic!("expected cacheable, got: {other:?}"),
    }
}

#[test]
fn compiler_driver_wl_out_implib_with_path() {
    // Real-world meson invocation:
    // clang++ -shared -Wl,--out-implib=ci/meson/native\fastled.dll.a -o fastled.dll a.o
    let result = parse_linker_invocation(
        "clang++",
        args(&[
            "-shared",
            "-Wl,--start-group",
            "-Wl,--out-implib=ci/meson/native\\fastled.dll.a",
            "-fuse-ld=lld",
            "-o",
            "ci/meson/native/fastled.dll",
            "a.o",
        ]),
    );
    match result {
        ParsedLinkerInvocation::Cacheable(c) => {
            assert_eq!(c.secondary_outputs.len(), 1);
            assert_eq!(
                c.secondary_outputs[0],
                NormalizedPath::new("ci/meson/native\\fastled.dll.a")
            );
        }
        other => panic!("expected cacheable, got: {other:?}"),
    }
}

#[test]
fn compiler_driver_no_implib_no_secondary() {
    // Without --out-implib, no secondary outputs
    let result = parse_linker_invocation("clang++", args(&["-shared", "-o", "foo.dll", "a.o"]));
    match result {
        ParsedLinkerInvocation::Cacheable(c) => {
            assert!(c.secondary_outputs.is_empty());
        }
        other => panic!("expected cacheable, got: {other:?}"),
    }
}
