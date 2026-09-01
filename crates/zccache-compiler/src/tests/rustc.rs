//! Rustc invocation parsing: crate types, --emit, --out-dir, proc-macro/bin output naming.

use super::super::parse_rustc::parse_rustc_invocation_with_policy;
use super::super::{detect_family, parse_invocation, CompilerFamily, ParsedInvocation};
use super::args;
use zccache_core::{config::CACHE_TEST_BINS_ENV, NormalizedPath};

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

// Issue #517: the system-include discovery probe spawns the compiler with
// C/C++ preprocessor flags (`-v -E -x c++ NUL`). Doing that for rustc on
// the cold path adds ~30-50 ms per first-after-clear compile while
// returning no useful data — rust has no concept of system includes.
#[test]
fn needs_system_include_discovery_truth_table() {
    assert!(CompilerFamily::Gcc.needs_system_include_discovery());
    assert!(CompilerFamily::Clang.needs_system_include_discovery());
    assert!(CompilerFamily::Msvc.needs_system_include_discovery());
    assert!(!CompilerFamily::Rustc.needs_system_include_discovery());
    assert!(!CompilerFamily::Rustfmt.needs_system_include_discovery());
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
fn rustc_cross_windows_bin_uses_executable_extension() {
    let result = parse_invocation(
        "rustc",
        &args(&[
            "--target",
            "x86_64-pc-windows-msvc",
            "--crate-name",
            "soldr",
            "--crate-type",
            "bin",
            "--out-dir",
            "/tmp/target/deps",
            "/path/to/main.rs",
        ]),
    );
    let cc = match result {
        ParsedInvocation::Cacheable(c) => c,
        other => panic!("expected cacheable, got: {other:?}"),
    };
    assert!(
        cc.output_file.to_string_lossy().ends_with("soldr.exe"),
        "cross-target Windows bins must use .exe: {}",
        cc.output_file.to_string_lossy()
    );
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

/// soldr#2349: this used to be `#[cfg(not(target_os = "windows"))]` because
/// `is_dylint_cdylib` refused every cdylib on a Windows host outright. The
/// gate is now host-independent (see `parse_rustc::is_dylint_cdylib`), so
/// this runs — and must pass — on every host in the CI matrix, including a
/// real Windows runner where the primary output has no `lib` prefix and
/// ends in `.dll` rather than `.so`/`.dylib`.
#[test]
fn rustc_dylint_library_cdylib_is_cacheable() {
    let result = parse_invocation(
        "rustc",
        &args(&[
            "--crate-name",
            "lint",
            "--crate-type",
            "cdylib",
            "--emit=link",
            "--out-dir",
            "/tmp/target/dylint/libraries/nightly/release/deps",
            "-C",
            "linker=/tools/dylint-link",
            "src/lib.rs",
        ]),
    );
    let ParsedInvocation::Cacheable(compilation) = result else {
        panic!("expected Dylint cdylib to be cacheable");
    };
    let expected = if cfg!(target_os = "windows") {
        "lint.dll".to_string()
    } else if cfg!(target_os = "macos") {
        "liblint.dylib".to_string()
    } else {
        "liblint.so".to_string()
    };
    assert!(
        compilation.output_file.ends_with(&expected),
        "expected output ending in {expected}, got {}",
        compilation.output_file.to_string_lossy()
    );
}

/// Pure-function coverage for the Windows/macOS/other filename split,
/// exercised directly with an explicit `HostFamily` so all three branches
/// run from a single CI host instead of only the one branch matching
/// whatever host happens to run the test (soldr#2349). Production callers
/// resolve the real host via `HostFamily::current()`.
#[test]
fn rustc_dylint_cdylib_filename_matches_host_convention() {
    use super::super::parse_rustc::{rustc_dylint_cdylib_filename, HostFamily};

    assert_eq!(
        rustc_dylint_cdylib_filename("lint", HostFamily::Windows),
        "lint.dll",
        "Windows cdylibs carry no `lib` prefix"
    );
    assert_eq!(
        rustc_dylint_cdylib_filename("lint", HostFamily::MacOs),
        "liblint.dylib"
    );
    assert_eq!(
        rustc_dylint_cdylib_filename("lint", HostFamily::Other),
        "liblint.so"
    );
}

/// dylint-link on Windows is `dylint-link.exe`; the linker-basename match
/// uses `Path::file_stem()`, which already strips the extension, so this
/// must keep matching (soldr#2349 item 4: preserve linker-key-material
/// behavior on Windows).
///
/// Uses a forward-slash path rather than a `C:\...` backslash path
/// deliberately: `std::path::Path`'s component-splitting is a compile-time
/// choice baked into the `zccache` binary by its own target OS, not the
/// string content, so a backslash path only splits into components when
/// this test happens to run on an actual Windows-built binary. Forward
/// slashes are valid separators on both Windows and Unix `Path`, so they
/// exercise the same `.exe`-stripping behavior from any CI host without
/// depending on which OS is actually running the test.
#[test]
fn rustc_dylint_cdylib_linker_matches_windows_exe_suffix() {
    let result = parse_invocation(
        "rustc",
        &args(&[
            "--crate-name",
            "lint",
            "--crate-type",
            "cdylib",
            "--emit=link",
            "--out-dir",
            "/tmp/target/dylint/libraries/nightly/release/deps",
            "-C",
            "linker=/tools/dylint-link.exe",
            "src/lib.rs",
        ]),
    );
    assert!(
        matches!(result, ParsedInvocation::Cacheable(_)),
        "dylint-link.exe must still match the dylint-link basename gate"
    );
}

/// The dylint exception is keyed on the linker basename, not the out-dir
/// shape alone — a Windows cdylib built with the real MSVC linker (the
/// PyO3/maturin shape) sharing the same `dylint/libraries` out-dir tree by
/// coincidence must stay refused.
#[test]
fn rustc_windows_cdylib_with_non_dylint_linker_is_non_cacheable() {
    let result = parse_invocation(
        "rustc",
        &args(&[
            "--crate-name",
            "extmod",
            "--crate-type",
            "cdylib",
            "--emit=link",
            "--out-dir",
            "/tmp/target/dylint/libraries/nightly/release/deps",
            "-C",
            "linker=/tools/link.exe",
            "src/lib.rs",
        ]),
    );
    assert!(matches!(result, ParsedInvocation::NonCacheable { .. }));
}

/// soldr#2349: this used to be `#[cfg(not(target_os = "windows"))]` — see
/// the note on `rustc_dylint_library_cdylib_is_cacheable` above. None of
/// these mutations depend on host at all (each fails a shape check that
/// applies identically on every platform), so this now runs everywhere.
#[test]
fn rustc_dylint_cdylib_requires_the_complete_narrow_shape() {
    for invocation in [
        vec![
            "--crate-type=cdylib",
            "--out-dir=/tmp/target/release/deps",
            "-Clinker=/tools/dylint-link",
            "src/lib.rs",
        ],
        vec![
            "--crate-type=cdylib",
            "--out-dir=/tmp/target/dylint/libraries/nightly/release/deps",
            "-Clinker=/tools/cc",
            "src/lib.rs",
        ],
        vec![
            "--crate-type=cdylib",
            "--out-dir=/tmp/target/dylint/libraries/nightly/release/deps",
            "-Clinker=/tools/dylint-link",
            "-Cextra-filename=-hash",
            "src/lib.rs",
        ],
        vec![
            "--crate-type=cdylib",
            "--out-dir=/tmp/target/dylint/libraries/nightly/release/deps",
            "-Clinker=/tools/dylint-link",
            "--target=wasm32-unknown-unknown",
            "src/lib.rs",
        ],
        vec![
            "--crate-type=cdylib,rlib",
            "--out-dir=/tmp/target/dylint/libraries/nightly/release/deps",
            "-Clinker=/tools/dylint-link",
            "src/lib.rs",
        ],
    ] {
        let result = parse_invocation("rustc", &args(&invocation));
        assert!(
            matches!(result, ParsedInvocation::NonCacheable { .. }),
            "mutation should remain non-cacheable: {invocation:?}"
        );
    }
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
fn rustc_explicit_emit_link_path_is_the_primary_output() {
    let emit_arg = if cfg!(windows) {
        r"--emit=link=C:\tmp\custom.rlib,dep-info=C:\tmp\custom.d".to_string()
    } else {
        "--emit=link=/tmp/custom.rlib,dep-info=/tmp/custom.d".to_string()
    };
    let result = parse_invocation(
        "rustc",
        &args(&[
            "--crate-type",
            "lib",
            "--crate-name",
            "hello",
            emit_arg.as_str(),
            "lib.rs",
        ]),
    );
    let ParsedInvocation::Cacheable(compilation) = result else {
        panic!("expected cacheable rustc invocation");
    };
    assert_eq!(
        compilation
            .output_file
            .file_name()
            .and_then(|name| name.to_str()),
        Some("custom.rlib")
    );
}

#[test]
fn rustc_non_link_emit_uses_its_real_primary_extension() {
    for (emit, expected) in [
        ("obj", "hello.o"),
        ("asm", "hello.s"),
        ("llvm-ir", "hello.ll"),
        ("llvm-bc", "hello.bc"),
        ("mir", "hello.mir"),
    ] {
        let result = parse_invocation(
            "rustc",
            &args(&[
                "--crate-type",
                "lib",
                "--crate-name",
                "hello",
                "--emit",
                emit,
                "lib.rs",
            ]),
        );
        let ParsedInvocation::Cacheable(compilation) = result else {
            panic!("expected cacheable rustc invocation for {emit}");
        };
        assert_eq!(
            compilation
                .output_file
                .file_name()
                .and_then(|name| name.to_str()),
            Some(expected)
        );
    }
}

#[test]
fn rustc_randomized_autocfg_probe_is_non_cacheable() {
    let result = parse_invocation(
        "rustc",
        &args(&[
            "--crate-name",
            "autocfg_4dddca434e09bacf_0",
            "--crate-type=lib",
            "--out-dir",
            "/tmp/target/release/build/num-traits/out",
            "--emit=llvm-ir",
            "--target",
            "x86_64-unknown-linux-gnu",
            "/tmp/soldr-stdin-af1349b9f5f9a1a6.rs",
        ]),
    );
    match result {
        ParsedInvocation::NonCacheable { reason } => {
            assert!(reason.contains("randomized autocfg probe"));
        }
        other => panic!("expected randomized autocfg probe to be non-cacheable, got: {other:?}"),
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

// ─── `--test` harness admission (zccache#1525) ────────────────────────
//
// These drive `parse_rustc_invocation_with_policy` rather than
// `parse_invocation` so the policy value is explicit in each case. The env
// read is the typed `cache_test_binaries_enabled()` accessor that
// feeds this seam; keeping it out of the assertions means no test has to
// mutate process-global environment state that the rest of the suite races on.

#[test]
fn rustc_test_flag_makes_non_cacheable() {
    // Policy: a `--test` invocation produces a test harness executable, which
    // relinks on any workspace source edit while statically linking the whole
    // dependency graph. Multi-megabyte entries under keys that can never be
    // requested twice, so it is refused at admission (soldr#2931).
    //
    // The explicit `--crate-type lib` here is the deliberate hard case: `lib`
    // is cacheable on its own, but `--test` overrides what rustc actually
    // emits, so the exclusion wins over the declared crate type.
    let result = parse_rustc_invocation_with_policy(
        "rustc",
        &args(&["--crate-type", "lib", "--test", "src/lib.rs"]),
        false,
    );
    match result {
        ParsedInvocation::NonCacheable { reason } => {
            assert!(
                reason.contains("test harness"),
                "expected the test-harness policy reason, got: {reason}"
            );
        }
        other => panic!("expected --test to be non-cacheable, got: {other:?}"),
    }
}

#[test]
fn rustc_cargo_shaped_test_harness_is_non_cacheable() {
    // The real-world shape from zccache#1525: cargo compiles an integration
    // test with `--test` and NO `--crate-type`, so the default-to-`bin`
    // fallback used to admit every linked test executable into the store.
    let result = parse_rustc_invocation_with_policy(
        "rustc",
        &args(&[
            "--edition",
            "2021",
            "--crate-name",
            "integration",
            "--test",
            "--emit=dep-info,link",
            "-C",
            "extra-filename=-abc123",
            "--out-dir",
            "/target/debug/deps",
            "tests/integration.rs",
        ]),
        false,
    );
    match result {
        ParsedInvocation::NonCacheable { reason } => {
            assert!(
                reason.contains("test harness"),
                "expected the test-harness policy reason, got: {reason}"
            );
        }
        other => panic!("expected cargo-shaped --test to be non-cacheable, got: {other:?}"),
    }
}

#[test]
fn rustc_test_harness_opt_in_readmits_the_harness() {
    // ZCCACHE_CACHE_TEST_BINS is the explicit escape hatch for anyone who can
    // demonstrate a real hit rate. `--test` stays in unknown_flags so the
    // harness cannot collide with a non-harness build of the same source.
    let result = parse_rustc_invocation_with_policy(
        "rustc",
        &args(&[
            "--crate-name",
            "integration",
            "--test",
            "--out-dir",
            "/target/debug/deps",
            "tests/integration.rs",
        ]),
        true,
    );
    let cc = match result {
        ParsedInvocation::Cacheable(c) => c,
        other => panic!("expected the opt-in to re-admit the harness, got: {other:?}"),
    };
    assert!(
        cc.unknown_flags.iter().any(|flag| flag == "--test"),
        "--test must stay in the cache key: {:?}",
        cc.unknown_flags
    );
}

#[test]
fn rustc_test_harness_opt_in_uses_the_canonical_owned_flag_grammar() {
    // One grammar for every zccache-owned switch (zccache#1478). Re-asserted
    // here because soldr#2740 is what hand-rolling a sixth truthy parser
    // costs: `FOO=false` ended up turning a switch on.
    assert_eq!(CACHE_TEST_BINS_ENV, "ZCCACHE_CACHE_TEST_BINS");
    for enabled in ["1", "true", "TRUE", " true "] {
        assert!(
            zccache_core::config::owned_flag_enabled(Some(enabled)),
            "{enabled:?} must enable the opt-in"
        );
    }
    for disabled in ["0", "false", "no", "yes", "on", ""] {
        assert!(
            !zccache_core::config::owned_flag_enabled(Some(disabled)),
            "{disabled:?} must leave the exclusion in force"
        );
    }
    assert!(!zccache_core::config::owned_flag_enabled(None));
}

#[test]
fn rustc_lib_without_test_flag_stays_cacheable() {
    // Guard against over-broadening zccache#1525: the exclusion keys on
    // `--test` alone, so an ordinary library compile is untouched even with
    // the opt-in off.
    let result = parse_rustc_invocation_with_policy(
        "rustc",
        &args(&["--crate-type", "lib", "src/lib.rs"]),
        false,
    );
    assert!(matches!(result, ParsedInvocation::Cacheable(_)));
}
