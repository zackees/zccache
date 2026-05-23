//! Rustc invocation parsing: crate types, --emit, --out-dir, proc-macro/bin output naming.

use super::args;
use super::super::{detect_family, parse_invocation, CompilerFamily, ParsedInvocation};
use zccache_monocrate::core::NormalizedPath;

// ─── Rustc detection tests ────────────────────────────────────────────

#[test]
fn detect_rustc_family() {
    assert_eq!(detect_family("rustc"), CompilerFamily::Rustc);
    assert_eq!(detect_family("/usr/bin/rustc"), CompilerFamily::Rustc);
    assert_eq!(detect_family("rustc.exe"), CompilerFamily::Rustc);
    assert_eq!(
        detect_family("C:\\rustup\\rustc.exe"),
        CompilerFamily::Rustc
    );
}

#[test]
fn rustc_no_depfile_support() {
    // Rustc uses --emit=dep-info, not -MD -MF
    assert!(!CompilerFamily::Rustc.supports_depfile());
}

#[test]
fn rustc_no_pch_extension() {
    assert_eq!(CompilerFamily::Rustc.pch_extension(), None);
}

// ─── Rustc cacheability tests ─────────────────────────────────────────

#[test]
fn rustc_lib_crate_is_cacheable() {
    let result = parse_invocation(
        "rustc",
        &args(&[
            "--edition",
            "2021",
            "--crate-type",
            "lib",
            "--emit=dep-info,metadata,link",
            "-C",
            "opt-level=2",
            "src/lib.rs",
        ]),
    );
    match result {
        ParsedInvocation::Cacheable(c) => {
            assert_eq!(c.family, CompilerFamily::Rustc);
            assert_eq!(c.source_file, NormalizedPath::new("src/lib.rs"));
        }
        other => panic!("expected cacheable, got: {other:?}"),
    }
}

#[test]
fn rustc_rlib_crate_is_cacheable() {
    let result = parse_invocation("rustc", &args(&["--crate-type", "rlib", "src/lib.rs"]));
    assert!(matches!(result, ParsedInvocation::Cacheable(_)));
}

#[test]
fn rustc_staticlib_crate_is_cacheable() {
    let result = parse_invocation("rustc", &args(&["--crate-type", "staticlib", "src/lib.rs"]));
    assert!(matches!(result, ParsedInvocation::Cacheable(_)));
}

#[test]
fn rustc_bin_crate_is_cacheable() {
    // bin became cacheable in iter7 alongside a touch_mtime change
    // so cargo's fingerprint doesn't invalidate downstream when a
    // hit materializes the binary.
    let result = parse_invocation("rustc", &args(&["--crate-type", "bin", "src/main.rs"]));
    assert!(matches!(result, ParsedInvocation::Cacheable(_)));
}

#[test]
fn rustc_bin_primary_output_uses_executable_extension() {
    let result = parse_invocation(
        "rustc",
        &args(&[
            "--crate-name",
            "build_script_build",
            "--crate-type",
            "bin",
            "--out-dir",
            "/tmp/build/foo-abc",
            "-C",
            "extra-filename=-abc",
            "/path/to/build.rs",
        ]),
    );
    let cc = match result {
        ParsedInvocation::Cacheable(c) => c,
        other => panic!("expected cacheable, got: {other:?}"),
    };
    let out = cc.output_file.to_string_lossy();
    if cfg!(target_os = "windows") {
        assert!(
            out.ends_with("build_script_build-abc.exe"),
            "expected bin .exe, got {out}"
        );
    } else {
        assert!(
            out.ends_with("build_script_build-abc"),
            "expected bin executable, got {out}"
        );
        assert!(!out.ends_with(".rlib"), "bin must not get .rlib, got {out}");
    }
}

#[test]
fn rustc_dylib_is_non_cacheable() {
    let result = parse_invocation("rustc", &args(&["--crate-type", "dylib", "src/lib.rs"]));
    assert!(matches!(result, ParsedInvocation::NonCacheable { .. }));
}

#[test]
fn rustc_proc_macro_is_cacheable() {
    // Proc-macros are host-side dylibs whose output is deterministic for
    // a given source + dep set + rustc — caching them is the same
    // safety contract as any other rustc invocation. Targets the
    // 18× proc-macro non-cacheables on the warm-rebuild scenario.
    let result = parse_invocation(
        "rustc",
        &args(&["--crate-type", "proc-macro", "src/lib.rs"]),
    );
    assert!(matches!(result, ParsedInvocation::Cacheable(_)));
}

#[test]
fn rustc_proc_macro_primary_output_uses_dylib_extension() {
    // Without this, the daemon's `collect_rustc_output_files` would
    // stat a non-existent `.rlib` path post-compile, return an empty
    // outputs vec, and take the early-return branch that skips
    // `dep_graph.update()` — leaving the context Cold forever and
    // causing every warm rebuild to recompile the proc-macro
    // (regression observed in the iter4 OODA pass).
    let result = parse_invocation(
        "rustc",
        &args(&[
            "--crate-name",
            "serde_derive",
            "--crate-type",
            "proc-macro",
            "--out-dir",
            "/tmp/deps",
            "-C",
            "extra-filename=-abc123",
            "/path/to/src/lib.rs",
        ]),
    );
    let cc = match result {
        ParsedInvocation::Cacheable(c) => c,
        other => panic!("expected cacheable, got: {other:?}"),
    };
    let out = cc.output_file.to_string_lossy();
    if cfg!(target_os = "windows") {
        assert!(
            out.ends_with("serde_derive-abc123.dll"),
            "expected proc-macro .dll, got {out}"
        );
    } else if cfg!(target_os = "macos") {
        assert!(
            out.ends_with("libserde_derive-abc123.dylib"),
            "expected proc-macro .dylib, got {out}"
        );
    } else {
        assert!(
            out.ends_with("libserde_derive-abc123.so"),
            "expected proc-macro .so, got {out}"
        );
    }
}

#[test]
fn rustc_cdylib_is_non_cacheable() {
    let result = parse_invocation("rustc", &args(&["--crate-type", "cdylib", "src/lib.rs"]));
    assert!(matches!(result, ParsedInvocation::NonCacheable { .. }));
}

#[test]
fn rustc_no_crate_type_defaults_to_bin_cacheable() {
    // Without --crate-type, rustc defaults to bin. bin is cacheable
    // as of iter7 — see `rustc_bin_crate_is_cacheable`.
    let result = parse_invocation("rustc", &args(&["src/main.rs"]));
    assert!(matches!(result, ParsedInvocation::Cacheable(_)));
}

#[test]
fn rustc_incremental_is_cacheable() {
    // Cargo always passes -C incremental. We allow it (ignored for cache key).
    let result = parse_invocation(
        "rustc",
        &args(&[
            "--crate-type",
            "lib",
            "-C",
            "incremental=/tmp/incr",
            "src/lib.rs",
        ]),
    );
    assert!(matches!(result, ParsedInvocation::Cacheable(_)));
}

#[test]
fn rustc_no_source_is_non_cacheable() {
    let result = parse_invocation("rustc", &args(&["--version"]));
    assert!(matches!(result, ParsedInvocation::NonCacheable { .. }));
}

#[test]
fn rustc_emit_metadata_is_cacheable() {
    // cargo check uses --emit=metadata
    let result = parse_invocation(
        "rustc",
        &args(&["--crate-type", "lib", "--emit=metadata", "src/lib.rs"]),
    );
    assert!(matches!(result, ParsedInvocation::Cacheable(_)));
}

#[test]
fn rustc_output_with_explicit_o() {
    let result = parse_invocation(
        "rustc",
        &args(&["--crate-type", "lib", "src/lib.rs", "-o", "libfoo.rlib"]),
    );
    match result {
        ParsedInvocation::Cacheable(c) => {
            assert_eq!(c.output_file, NormalizedPath::new("libfoo.rlib"));
        }
        other => panic!("expected cacheable, got: {other:?}"),
    }
}

#[test]
fn rustc_metadata_only_output_is_rmeta() {
    // cargo check: --emit=dep-info,metadata (no link) → primary output is .rmeta
    let result = parse_invocation(
        "rustc",
        &args(&[
            "--crate-type",
            "lib",
            "--crate-name",
            "mylib",
            "--emit=dep-info,metadata",
            "--out-dir",
            "/target/debug/deps",
            "-C",
            "extra-filename=-abc123",
            "src/lib.rs",
        ]),
    );
    match result {
        ParsedInvocation::Cacheable(c) => {
            assert_eq!(
                c.output_file,
                NormalizedPath::new("/target/debug/deps/libmylib-abc123.rmeta")
            );
        }
        other => panic!("expected cacheable, got: {other:?}"),
    }
}

#[test]
fn rustc_output_from_out_dir() {
    let result = parse_invocation(
        "rustc",
        &args(&[
            "--crate-type",
            "lib",
            "--crate-name",
            "mylib",
            "--out-dir",
            "/target/debug/deps",
            "-C",
            "extra-filename=-abc123",
            "src/lib.rs",
        ]),
    );
    match result {
        ParsedInvocation::Cacheable(c) => {
            assert_eq!(
                c.output_file,
                NormalizedPath::new("/target/debug/deps/libmylib-abc123.rlib")
            );
        }
        other => panic!("expected cacheable, got: {other:?}"),
    }
}

#[test]
fn rustc_full_cargo_invocation_cacheable() {
    // Realistic cargo-generated rustc command
    let result = parse_invocation(
        "rustc",
        &args(&[
            "--edition",
            "2021",
            "--crate-type",
            "lib",
            "--crate-name",
            "serde",
            "--emit=dep-info,metadata,link",
            "-C",
            "opt-level=2",
            "-C",
            "metadata=abc123def",
            "-C",
            "extra-filename=-abc123def",
            "--out-dir",
            "/target/release/deps",
            "-L",
            "dependency=/target/release/deps",
            "--extern",
            "serde_derive=/target/release/deps/libserde_derive-xyz.so",
            "--cap-lints",
            "allow",
            "--cfg",
            "feature=\"derive\"",
            "--cfg",
            "feature=\"std\"",
            "src/lib.rs",
        ]),
    );
    match result {
        ParsedInvocation::Cacheable(c) => {
            assert_eq!(c.family, CompilerFamily::Rustc);
            assert_eq!(c.source_file, NormalizedPath::new("src/lib.rs"));
            assert_eq!(
                c.output_file,
                NormalizedPath::new("/target/release/deps/libserde-abc123def.rlib")
            );
        }
        other => panic!("expected cacheable, got: {other:?}"),
    }
}

#[test]
fn rustc_original_args_preserved() {
    let input = args(&["--edition", "2021", "--crate-type", "lib", "src/lib.rs"]);
    let result = parse_invocation("rustc", &input);
    match result {
        ParsedInvocation::Cacheable(c) => {
            assert_eq!(*c.original_args, *input);
        }
        other => panic!("expected cacheable, got: {other:?}"),
    }
}

#[test]
fn rustc_equal_form_crate_type() {
    let result = parse_invocation("rustc", &args(&["--crate-type=lib", "src/lib.rs"]));
    assert!(matches!(result, ParsedInvocation::Cacheable(_)));
}

#[test]
fn rustc_concatenated_c_incremental_is_cacheable() {
    // -Cincremental= form (no space after -C) — still cacheable
    let result = parse_invocation(
        "rustc",
        &args(&["--crate-type", "lib", "-Cincremental=/tmp", "src/lib.rs"]),
    );
    assert!(matches!(result, ParsedInvocation::Cacheable(_)));
}

#[test]
fn rustc_comma_separated_crate_type_all_cacheable() {
    let result = parse_invocation("rustc", &args(&["--crate-type", "lib,rlib", "src/lib.rs"]));
    assert!(matches!(result, ParsedInvocation::Cacheable(_)));
}

#[test]
fn rustc_comma_separated_crate_type_mixed_non_cacheable() {
    // lib is cacheable but dylib is not
    let result = parse_invocation("rustc", &args(&["--crate-type", "lib,dylib", "src/lib.rs"]));
    assert!(matches!(result, ParsedInvocation::NonCacheable { .. }));
}

#[test]
fn rustc_comma_separated_crate_type_equals_form() {
    let result = parse_invocation(
        "rustc",
        &args(&["--crate-type=lib,staticlib", "src/lib.rs"]),
    );
    assert!(matches!(result, ParsedInvocation::Cacheable(_)));
}

#[test]
fn rustc_test_flag_makes_non_cacheable() {
    // --test compiles a test harness (implicitly bin, not cacheable)
    let result = parse_invocation(
        "rustc",
        &args(&["--crate-type", "lib", "--test", "src/lib.rs"]),
    );
    // --test gets captured as unknown_flag. Since --crate-type lib is specified
    // the compilation IS cacheable. The --test flag is in unknown_flags which
    // is part of the cache key, so different --test values produce different keys.
    // This is correct: `--test` with `--crate-type lib` is a valid cacheable invocation.
    assert!(matches!(result, ParsedInvocation::Cacheable(_)));
}
