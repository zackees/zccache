//! MSVC `link.exe` argument-parsing tests.

use super::super::{parse_linker_invocation, LinkerFamily, ParsedLinkerInvocation};
use super::args;
use zccache_core::NormalizedPath;

#[test]
fn basic_msvc_dll() {
    let result = parse_linker_invocation(
        "link.exe",
        args(&["/DLL", "/OUT:foo.dll", "a.obj", "b.obj"]),
    );
    match result {
        ParsedLinkerInvocation::Cacheable(c) => {
            assert_eq!(c.family, LinkerFamily::MsvcLink);
            assert_eq!(c.output_file, NormalizedPath::new("foo.dll"));
            assert_eq!(c.input_files.len(), 2);
            assert_eq!(c.input_files[0], NormalizedPath::new("a.obj"));
            assert_eq!(c.input_files[1], NormalizedPath::new("b.obj"));
            assert!(c.non_deterministic); // no /DETERMINISTIC
        }
        other => panic!("expected cacheable, got: {other:?}"),
    }
}

#[test]
fn msvc_dll_with_deterministic() {
    let result = parse_linker_invocation(
        "link.exe",
        args(&["/DLL", "/DETERMINISTIC", "/OUT:foo.dll", "a.obj"]),
    );
    match result {
        ParsedLinkerInvocation::Cacheable(c) => {
            assert!(!c.non_deterministic); // /DETERMINISTIC present
        }
        other => panic!("expected cacheable, got: {other:?}"),
    }
}

#[test]
fn msvc_exe_cacheable() {
    // Executable linking (no /DLL) should be cacheable
    let result = parse_linker_invocation("link.exe", args(&["/OUT:foo.exe", "main.obj"]));
    match result {
        ParsedLinkerInvocation::Cacheable(c) => {
            assert_eq!(c.family, LinkerFamily::MsvcLink);
            assert_eq!(c.output_file, NormalizedPath::new("foo.exe"));
            assert_eq!(c.input_files, vec![NormalizedPath::new("main.obj")]);
        }
        other => panic!("expected cacheable, got: {other:?}"),
    }
}

#[test]
fn msvc_exe_default_output_name() {
    // Without /OUT: and without /DLL, defaults to first input with .exe extension
    let result = parse_linker_invocation("link.exe", args(&["main.obj", "util.obj"]));
    match result {
        ParsedLinkerInvocation::Cacheable(c) => {
            assert_eq!(c.output_file, NormalizedPath::new("main.exe"));
        }
        other => panic!("expected cacheable, got: {other:?}"),
    }
}

#[test]
fn msvc_dll_no_inputs() {
    let result = parse_linker_invocation("link.exe", args(&["/DLL", "/OUT:foo.dll"]));
    assert!(matches!(
        result,
        ParsedLinkerInvocation::NonCacheable { .. }
    ));
}

#[test]
fn msvc_dll_default_output_name() {
    // Without /OUT:, defaults to first input with .dll extension
    let result = parse_linker_invocation("link.exe", args(&["/DLL", "a.obj", "b.obj"]));
    match result {
        ParsedLinkerInvocation::Cacheable(c) => {
            assert_eq!(c.output_file, NormalizedPath::new("a.dll"));
        }
        other => panic!("expected cacheable, got: {other:?}"),
    }
}

#[test]
fn msvc_dll_preserves_input_order() {
    let result = parse_linker_invocation(
        "link.exe",
        args(&["/DLL", "/OUT:foo.dll", "z.obj", "a.obj", "m.obj"]),
    );
    match result {
        ParsedLinkerInvocation::Cacheable(c) => {
            assert_eq!(c.input_files[0], NormalizedPath::new("z.obj"));
            assert_eq!(c.input_files[1], NormalizedPath::new("a.obj"));
            assert_eq!(c.input_files[2], NormalizedPath::new("m.obj"));
        }
        other => panic!("expected cacheable, got: {other:?}"),
    }
}

#[test]
fn msvc_dll_with_implib() {
    let result = parse_linker_invocation(
        "link.exe",
        args(&["/DLL", "/OUT:foo.dll", "/IMPLIB:foo.lib", "a.obj"]),
    );
    match result {
        ParsedLinkerInvocation::Cacheable(c) => {
            assert!(c
                .cache_relevant_flags
                .contains(&"/IMPLIB:foo.lib".to_string()));
            // /IMPLIB: extracts secondary outputs: .lib + inferred .exp
            assert_eq!(c.secondary_outputs.len(), 2);
            assert_eq!(c.secondary_outputs[0], NormalizedPath::new("foo.lib"));
            assert_eq!(c.secondary_outputs[1], NormalizedPath::new("foo.exp"));
        }
        other => panic!("expected cacheable, got: {other:?}"),
    }
}

#[test]
fn msvc_dll_without_implib_no_secondary() {
    let result = parse_linker_invocation("link.exe", args(&["/DLL", "/OUT:foo.dll", "a.obj"]));
    match result {
        ParsedLinkerInvocation::Cacheable(c) => {
            assert!(c.secondary_outputs.is_empty());
        }
        other => panic!("expected cacheable, got: {other:?}"),
    }
}

#[test]
fn msvc_implib_dash_syntax() {
    let result = parse_linker_invocation(
        "link.exe",
        args(&["/DLL", "-IMPLIB:mylib.lib", "/OUT:mylib.dll", "a.obj"]),
    );
    match result {
        ParsedLinkerInvocation::Cacheable(c) => {
            assert_eq!(c.secondary_outputs.len(), 2);
            assert_eq!(c.secondary_outputs[0], NormalizedPath::new("mylib.lib"));
            assert_eq!(c.secondary_outputs[1], NormalizedPath::new("mylib.exp"));
        }
        other => panic!("expected cacheable, got: {other:?}"),
    }
}

#[test]
fn msvc_dll_with_flags() {
    let result = parse_linker_invocation(
        "link.exe",
        args(&["/DLL", "/NOLOGO", "/MACHINE:X64", "/OUT:foo.dll", "a.obj"]),
    );
    match result {
        ParsedLinkerInvocation::Cacheable(c) => {
            assert!(c.cache_relevant_flags.contains(&"/NOLOGO".to_string()));
            assert!(c.cache_relevant_flags.contains(&"/MACHINE:X64".to_string()));
        }
        other => panic!("expected cacheable, got: {other:?}"),
    }
}

#[test]
fn msvc_dll_dash_syntax() {
    // link.exe also accepts - prefix for flags
    let result = parse_linker_invocation(
        "link.exe",
        args(&["-DLL", "-OUT:foo.dll", "-DETERMINISTIC", "a.obj"]),
    );
    match result {
        ParsedLinkerInvocation::Cacheable(c) => {
            assert_eq!(c.output_file, NormalizedPath::new("foo.dll"));
            assert!(!c.non_deterministic);
        }
        other => panic!("expected cacheable, got: {other:?}"),
    }
}

#[test]
fn msvc_no_args() {
    let result = parse_linker_invocation("link.exe", args(&[]));
    assert!(matches!(
        result,
        ParsedLinkerInvocation::NonCacheable { .. }
    ));
}

#[test]
fn msvc_def_file_as_flag() {
    let result = parse_linker_invocation(
        "link.exe",
        args(&["/DLL", "/DEF:foo.def", "/OUT:foo.dll", "a.obj"]),
    );
    match result {
        ParsedLinkerInvocation::Cacheable(c) => {
            assert!(c.cache_relevant_flags.contains(&"/DEF:foo.def".to_string()));
        }
        other => panic!("expected cacheable, got: {other:?}"),
    }
}

#[test]
fn msvc_case_insensitive_dll_flag() {
    let result = parse_linker_invocation("link.exe", args(&["/dll", "/out:foo.dll", "a.obj"]));
    match result {
        ParsedLinkerInvocation::Cacheable(c) => {
            assert_eq!(c.output_file, NormalizedPath::new("foo.dll"));
        }
        other => panic!("expected cacheable, got: {other:?}"),
    }
}
