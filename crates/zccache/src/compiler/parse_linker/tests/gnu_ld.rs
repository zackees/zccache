//! GNU ld / LLD argument-parsing tests.

use super::args;
use super::super::{parse_linker_invocation, LinkerFamily, ParsedLinkerInvocation};
use crate::core::NormalizedPath;

// ─── GNU ld shared library parsing ───────────────────────────────

#[test]
fn basic_shared_lib() {
    let result =
        parse_linker_invocation("ld", args(&["-shared", "-o", "libfoo.so", "a.o", "b.o"]));
    match result {
        ParsedLinkerInvocation::Cacheable(c) => {
            assert_eq!(c.family, LinkerFamily::Ld);
            assert_eq!(c.output_file, NormalizedPath::new("libfoo.so"));
            assert_eq!(c.input_files.len(), 2);
            assert_eq!(c.input_files[0], NormalizedPath::new("a.o"));
            assert_eq!(c.input_files[1], NormalizedPath::new("b.o"));
            assert!(!c.non_deterministic); // GNU ld is deterministic by default
        }
        other => panic!("expected cacheable, got: {other:?}"),
    }
}

#[test]
fn shared_lib_with_soname() {
    let result = parse_linker_invocation(
        "ld",
        args(&[
            "-shared",
            "-soname",
            "libfoo.so.1",
            "-o",
            "libfoo.so.1.0",
            "a.o",
        ]),
    );
    match result {
        ParsedLinkerInvocation::Cacheable(c) => {
            assert_eq!(c.output_file, NormalizedPath::new("libfoo.so.1.0"));
            assert!(c.cache_relevant_flags.contains(&"-soname".to_string()));
            assert!(c.cache_relevant_flags.contains(&"libfoo.so.1".to_string()));
        }
        other => panic!("expected cacheable, got: {other:?}"),
    }
}

#[test]
fn shared_lib_with_libraries() {
    let result = parse_linker_invocation(
        "ld",
        args(&[
            "-shared",
            "-o",
            "libfoo.so",
            "a.o",
            "-lm",
            "-lpthread",
            "-L/usr/lib",
        ]),
    );
    match result {
        ParsedLinkerInvocation::Cacheable(c) => {
            assert_eq!(c.input_files, vec![NormalizedPath::new("a.o")]);
            assert!(c.cache_relevant_flags.contains(&"-lm".to_string()));
            assert!(c.cache_relevant_flags.contains(&"-lpthread".to_string()));
            assert!(c.cache_relevant_flags.contains(&"-L/usr/lib".to_string()));
        }
        other => panic!("expected cacheable, got: {other:?}"),
    }
}

#[test]
fn exe_link_cacheable() {
    // Executable linking (no -shared) should be cacheable
    let result = parse_linker_invocation("ld", args(&["-o", "a.out", "main.o"]));
    match result {
        ParsedLinkerInvocation::Cacheable(c) => {
            assert_eq!(c.family, LinkerFamily::Ld);
            assert_eq!(c.output_file, NormalizedPath::new("a.out"));
            assert_eq!(c.input_files, vec![NormalizedPath::new("main.o")]);
            assert!(!c.non_deterministic);
        }
        other => panic!("expected cacheable, got: {other:?}"),
    }
}

#[test]
fn no_output_non_cacheable() {
    let result = parse_linker_invocation("ld", args(&["-shared", "a.o"]));
    assert!(matches!(
        result,
        ParsedLinkerInvocation::NonCacheable { .. }
    ));
}

#[test]
fn no_inputs_non_cacheable() {
    let result = parse_linker_invocation("ld", args(&["-shared", "-o", "libfoo.so"]));
    assert!(matches!(
        result,
        ParsedLinkerInvocation::NonCacheable { .. }
    ));
}

#[test]
fn no_args_non_cacheable() {
    let result = parse_linker_invocation("ld", args(&[]));
    assert!(matches!(
        result,
        ParsedLinkerInvocation::NonCacheable { .. }
    ));
}

#[test]
fn preserves_input_order() {
    let result = parse_linker_invocation(
        "ld",
        args(&["-shared", "-o", "libfoo.so", "z.o", "a.o", "m.o"]),
    );
    match result {
        ParsedLinkerInvocation::Cacheable(c) => {
            assert_eq!(c.input_files[0], NormalizedPath::new("z.o"));
            assert_eq!(c.input_files[1], NormalizedPath::new("a.o"));
            assert_eq!(c.input_files[2], NormalizedPath::new("m.o"));
        }
        other => panic!("expected cacheable, got: {other:?}"),
    }
}

// ─── Non-determinism (timestamps, build-id) ───────────────────

#[test]
fn build_id_uuid_is_non_deterministic() {
    let result = parse_linker_invocation(
        "ld",
        args(&["-shared", "--build-id=uuid", "-o", "libfoo.so", "a.o"]),
    );
    match result {
        ParsedLinkerInvocation::Cacheable(c) => {
            assert!(
                c.non_deterministic,
                "--build-id=uuid produces random output — must be flagged"
            );
        }
        other => panic!("expected cacheable, got: {other:?}"),
    }
}

#[test]
fn build_id_sha1_is_deterministic() {
    let result = parse_linker_invocation(
        "ld",
        args(&["-shared", "--build-id=sha1", "-o", "libfoo.so", "a.o"]),
    );
    match result {
        ParsedLinkerInvocation::Cacheable(c) => {
            assert!(
                !c.non_deterministic,
                "--build-id=sha1 is content-derived — deterministic"
            );
        }
        other => panic!("expected cacheable, got: {other:?}"),
    }
}

#[test]
fn build_id_none_is_deterministic() {
    let result = parse_linker_invocation(
        "ld",
        args(&["-shared", "--build-id=none", "-o", "libfoo.so", "a.o"]),
    );
    match result {
        ParsedLinkerInvocation::Cacheable(c) => {
            assert!(!c.non_deterministic);
        }
        other => panic!("expected cacheable, got: {other:?}"),
    }
}

#[test]
fn default_ld_is_deterministic() {
    // GNU ld without --build-id is deterministic (no random build ID inserted)
    let result = parse_linker_invocation("ld", args(&["-shared", "-o", "libfoo.so", "a.o"]));
    match result {
        ParsedLinkerInvocation::Cacheable(c) => {
            assert!(!c.non_deterministic);
        }
        other => panic!("expected cacheable, got: {other:?}"),
    }
}

// ─── macOS dylib ─────────────────────────────────────────────

#[test]
fn macos_dylib() {
    let result =
        parse_linker_invocation("ld", args(&["-dylib", "-o", "libfoo.dylib", "a.o", "b.o"]));
    match result {
        ParsedLinkerInvocation::Cacheable(c) => {
            assert_eq!(c.output_file, NormalizedPath::new("libfoo.dylib"));
            assert_eq!(c.input_files.len(), 2);
            assert!(!c.non_deterministic);
        }
        other => panic!("expected cacheable, got: {other:?}"),
    }
}

#[test]
fn macos_dylib_with_install_name() {
    let result = parse_linker_invocation(
        "ld",
        args(&[
            "-dylib",
            "-install_name",
            "@rpath/libfoo.dylib",
            "-o",
            "libfoo.dylib",
            "a.o",
        ]),
    );
    match result {
        ParsedLinkerInvocation::Cacheable(c) => {
            assert!(c
                .cache_relevant_flags
                .contains(&"-install_name".to_string()));
            assert!(c
                .cache_relevant_flags
                .contains(&"@rpath/libfoo.dylib".to_string()));
        }
        other => panic!("expected cacheable, got: {other:?}"),
    }
}

// ─── LLD ─────────────────────────────────────────────────────

#[test]
fn lld_shared_lib() {
    let result = parse_linker_invocation(
        "ld.lld",
        args(&["-shared", "-o", "libfoo.so", "a.o", "b.o"]),
    );
    match result {
        ParsedLinkerInvocation::Cacheable(c) => {
            assert_eq!(c.family, LinkerFamily::Lld);
            assert_eq!(c.output_file, NormalizedPath::new("libfoo.so"));
        }
        other => panic!("expected cacheable, got: {other:?}"),
    }
}

// ─── Linker script and version script ────────────────────────

#[test]
fn with_linker_script() {
    let result = parse_linker_invocation(
        "ld",
        args(&["-shared", "-T", "link.ld", "-o", "libfoo.so", "a.o"]),
    );
    match result {
        ParsedLinkerInvocation::Cacheable(c) => {
            // Linker script is an input file (affects output)
            assert!(c.input_files.contains(&NormalizedPath::new("link.ld")));
            assert!(c.input_files.contains(&NormalizedPath::new("a.o")));
        }
        other => panic!("expected cacheable, got: {other:?}"),
    }
}

#[test]
fn with_version_script() {
    let result = parse_linker_invocation(
        "ld",
        args(&[
            "-shared",
            "--version-script=libfoo.map",
            "-o",
            "libfoo.so",
            "a.o",
        ]),
    );
    match result {
        ParsedLinkerInvocation::Cacheable(c) => {
            // Version script is an input file
            assert!(c.input_files.contains(&NormalizedPath::new("libfoo.map")));
        }
        other => panic!("expected cacheable, got: {other:?}"),
    }
}

// ─── Unknown tool ────────────────────────────────────────────

#[test]
fn unknown_tool_non_cacheable() {
    let result = parse_linker_invocation("rustc", args(&["-shared", "-o", "libfoo.so", "a.o"]));
    assert!(matches!(
        result,
        ParsedLinkerInvocation::NonCacheable { .. }
    ));
}

// ─── Cross-compile linker ────────────────────────────────────

#[test]
fn cross_compile_ld() {
    let result = parse_linker_invocation(
        "x86_64-linux-gnu-ld",
        args(&["-shared", "-o", "libfoo.so", "a.o"]),
    );
    match result {
        ParsedLinkerInvocation::Cacheable(c) => {
            assert_eq!(c.family, LinkerFamily::Ld);
            assert_eq!(c.tool, NormalizedPath::new("x86_64-linux-gnu-ld"));
        }
        other => panic!("expected cacheable, got: {other:?}"),
    }
}

// ─── --output= syntax ────────────────────────────────────────

#[test]
fn output_equals_syntax() {
    let result = parse_linker_invocation("ld", args(&["-shared", "--output=libfoo.so", "a.o"]));
    match result {
        ParsedLinkerInvocation::Cacheable(c) => {
            assert_eq!(c.output_file, NormalizedPath::new("libfoo.so"));
        }
        other => panic!("expected cacheable, got: {other:?}"),
    }
}

// ─── Edge cases: -z flags, -rpath, mixed inputs ─────────────

#[test]
fn z_relro_and_now_flags() {
    let result = parse_linker_invocation(
        "ld",
        args(&[
            "-shared",
            "-z",
            "relro",
            "-z",
            "now",
            "-o",
            "libfoo.so",
            "a.o",
        ]),
    );
    match result {
        ParsedLinkerInvocation::Cacheable(c) => {
            assert!(c.cache_relevant_flags.contains(&"-z".to_string()));
            assert!(c.cache_relevant_flags.contains(&"relro".to_string()));
            assert!(c.cache_relevant_flags.contains(&"now".to_string()));
        }
        other => panic!("expected cacheable, got: {other:?}"),
    }
}

#[test]
fn rpath_flag() {
    let result = parse_linker_invocation(
        "ld",
        args(&["-shared", "-rpath", "/usr/lib", "-o", "libfoo.so", "a.o"]),
    );
    match result {
        ParsedLinkerInvocation::Cacheable(c) => {
            // -rpath is consumed as a generic flag
            assert!(c.cache_relevant_flags.contains(&"-rpath".to_string()));
        }
        other => panic!("expected cacheable, got: {other:?}"),
    }
}

#[test]
fn mixed_object_and_archive_inputs() {
    let result = parse_linker_invocation(
        "ld",
        args(&["-shared", "-o", "libfoo.so", "a.o", "libbar.a", "c.o"]),
    );
    match result {
        ParsedLinkerInvocation::Cacheable(c) => {
            assert_eq!(c.input_files.len(), 3);
            assert_eq!(c.input_files[0], NormalizedPath::new("a.o"));
            assert_eq!(c.input_files[1], NormalizedPath::new("libbar.a"));
            assert_eq!(c.input_files[2], NormalizedPath::new("c.o"));
        }
        other => panic!("expected cacheable, got: {other:?}"),
    }
}

#[test]
fn soname_equals_syntax() {
    let result = parse_linker_invocation(
        "ld",
        args(&[
            "-shared",
            "--soname=libfoo.so.1",
            "-o",
            "libfoo.so.1.0",
            "a.o",
        ]),
    );
    match result {
        ParsedLinkerInvocation::Cacheable(c) => {
            assert!(c
                .cache_relevant_flags
                .contains(&"--soname=libfoo.so.1".to_string()));
        }
        other => panic!("expected cacheable, got: {other:?}"),
    }
}

#[test]
fn version_script_separate_args() {
    let result = parse_linker_invocation(
        "ld",
        args(&[
            "-shared",
            "--version-script",
            "libfoo.map",
            "-o",
            "libfoo.so",
            "a.o",
        ]),
    );
    match result {
        ParsedLinkerInvocation::Cacheable(c) => {
            assert!(c.input_files.contains(&NormalizedPath::new("libfoo.map")));
            assert!(c.input_files.contains(&NormalizedPath::new("a.o")));
        }
        other => panic!("expected cacheable, got: {other:?}"),
    }
}

#[test]
fn redundant_shared_flags() {
    // Multiple -shared flags are valid and shouldn't cause issues
    let result = parse_linker_invocation(
        "ld",
        args(&["-shared", "--shared", "-o", "libfoo.so", "a.o"]),
    );
    match result {
        ParsedLinkerInvocation::Cacheable(c) => {
            assert_eq!(c.output_file, NormalizedPath::new("libfoo.so"));
        }
        other => panic!("expected cacheable, got: {other:?}"),
    }
}

#[test]
fn wl_shared_inside_pass_through() {
    // -Wl,-shared inside a -Wl, pass-through should detect shared mode
    let result =
        parse_linker_invocation("ld", args(&["-Wl,-shared", "-o", "libfoo.so", "a.o"]));
    match result {
        ParsedLinkerInvocation::Cacheable(c) => {
            assert_eq!(c.output_file, NormalizedPath::new("libfoo.so"));
        }
        other => panic!("expected cacheable, got: {other:?}"),
    }
}

#[test]
fn gnu_ld_no_secondary_outputs() {
    let result = parse_linker_invocation("ld", args(&["-shared", "-o", "libfoo.so", "a.o"]));
    match result {
        ParsedLinkerInvocation::Cacheable(c) => {
            assert!(c.secondary_outputs.is_empty());
        }
        other => panic!("expected cacheable, got: {other:?}"),
    }
}
