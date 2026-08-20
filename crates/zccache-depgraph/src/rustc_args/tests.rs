use zccache_core::NormalizedPath;

use super::*;

fn args(s: &[&str]) -> Vec<String> {
    s.iter().map(|x| x.to_string()).collect()
}

fn cwd() -> NormalizedPath {
    NormalizedPath::from("/project")
}

#[test]
fn basic_parse_source_file() {
    let parsed = parse_rustc_args(&args(&["src/lib.rs"]), &cwd());
    assert_eq!(
        parsed.source_file,
        NormalizedPath::from("/project/src/lib.rs")
    );
}

#[test]
fn parse_edition() {
    let parsed = parse_rustc_args(&args(&["--edition", "2021", "src/lib.rs"]), &cwd());
    assert_eq!(parsed.edition.as_deref(), Some("2021"));
}

#[test]
fn parse_edition_equals_form() {
    let parsed = parse_rustc_args(&args(&["--edition=2021", "src/lib.rs"]), &cwd());
    assert_eq!(parsed.edition.as_deref(), Some("2021"));
}

#[test]
fn parse_crate_type() {
    let parsed = parse_rustc_args(
        &args(&["--crate-type", "lib", "--crate-type", "rlib", "src/lib.rs"]),
        &cwd(),
    );
    assert_eq!(parsed.crate_types, vec!["lib", "rlib"]);
}

#[test]
fn parse_crate_name() {
    let parsed = parse_rustc_args(&args(&["--crate-name", "mylib", "src/lib.rs"]), &cwd());
    assert_eq!(parsed.crate_name.as_deref(), Some("mylib"));
}

#[test]
fn parse_emit_types() {
    let parsed = parse_rustc_args(
        &args(&["--emit=dep-info,metadata,link", "src/lib.rs"]),
        &cwd(),
    );
    assert_eq!(parsed.emit_types, vec!["dep-info", "metadata", "link"]);
}

#[test]
fn parse_emit_with_paths() {
    // --emit=dep-info=/path/to/deps.d,metadata,link
    let parsed = parse_rustc_args(
        &args(&["--emit=dep-info=/tmp/deps.d,metadata,link", "src/lib.rs"]),
        &cwd(),
    );
    assert_eq!(parsed.emit_types, vec!["dep-info", "metadata", "link"]);
    assert_eq!(parsed.explicit_emit_paths.len(), 1);
    assert_eq!(parsed.explicit_emit_paths[0].0, "dep-info");
}

#[test]
fn parse_cfg_values() {
    let parsed = parse_rustc_args(
        &args(&["--cfg", "feature=\"derive\"", "--cfg", "unix", "src/lib.rs"]),
        &cwd(),
    );
    // Sorted
    assert_eq!(parsed.cfgs, vec!["feature=\"derive\"", "unix"]);
}

#[test]
fn parse_codegen_flags() {
    let parsed = parse_rustc_args(
        &args(&["-C", "opt-level=2", "-C", "debuginfo=2", "src/lib.rs"]),
        &cwd(),
    );
    // Sorted
    assert!(parsed.codegen_flags.contains(&"debuginfo=2".to_string()));
    assert!(parsed.codegen_flags.contains(&"opt-level=2".to_string()));
}

#[test]
fn parse_codegen_concatenated() {
    let parsed = parse_rustc_args(&args(&["-Copt-level=3", "src/lib.rs"]), &cwd());
    assert!(parsed.codegen_flags.contains(&"opt-level=3".to_string()));
}

#[test]
fn repeated_codegen_flags_preserve_order_and_effective_last_value() {
    let first = parse_rustc_args(
        &args(&["-Copt-level=2", "-C", "opt-level=3", "src/lib.rs"]),
        &cwd(),
    );
    let reversed = parse_rustc_args(
        &args(&["-Copt-level=3", "-C", "opt-level=2", "src/lib.rs"]),
        &cwd(),
    );

    assert_eq!(first.codegen_flags, ["opt-level=2", "opt-level=3"]);
    assert_eq!(reversed.codegen_flags, ["opt-level=3", "opt-level=2"]);
    assert_eq!(first.effective_codegen_value("opt-level"), Some("3"));
    assert_eq!(reversed.effective_codegen_value("opt-level"), Some("2"));
    assert_ne!(first.codegen_flags, reversed.codegen_flags);
}

#[test]
fn additive_codegen_flags_preserve_command_line_order() {
    let parsed = parse_rustc_args(
        &args(&[
            "-Cllvm-args=-first",
            "-C",
            "llvm-args=-second",
            "src/lib.rs",
        ]),
        &cwd(),
    );

    assert_eq!(
        parsed.codegen_flags,
        ["llvm-args=-first", "llvm-args=-second"]
    );
}

#[test]
fn shorthand_codegen_flags_keep_precedence_with_explicit_values() {
    for (alias, key, alias_value) in [("-g", "debuginfo", "2"), ("-O", "opt-level", "2")] {
        let explicit_last =
            parse_rustc_args(&args(&[alias, &format!("-C{key}=0"), "src/lib.rs"]), &cwd());
        let alias_last =
            parse_rustc_args(&args(&[&format!("-C{key}=0"), alias, "src/lib.rs"]), &cwd());

        assert_eq!(explicit_last.effective_codegen_value(key), Some("0"));
        assert_eq!(alias_last.effective_codegen_value(key), Some(alias_value));
        assert_ne!(explicit_last.codegen_flags, alias_last.codegen_flags);
    }
}

#[test]
fn linker_arguments_preserve_command_line_order() {
    let parsed = parse_rustc_args(
        &args(&[
            "-Clink-arg=z-last-lexically",
            "-Clink-arg=a-first-lexically",
            "src/lib.rs",
        ]),
        &cwd(),
    );

    assert_eq!(
        parsed.linker_args,
        ["link-arg=z-last-lexically", "link-arg=a-first-lexically"]
    );
}

#[test]
fn dylint_linker_inputs_have_dedicated_fields() {
    let parsed = parse_rustc_args(
        &args(&[
            "-Clinker=tools/dylint-link",
            "-C",
            "link-arg=-Wl,--build-id=none",
            "-Clink-args=-Wl,-z,now",
            "src/lib.rs",
        ]),
        &cwd(),
    );
    assert_eq!(
        parsed.linker,
        Some(NormalizedPath::from("/project/tools/dylint-link"))
    );
    assert_eq!(
        parsed.linker_args,
        vec![
            "link-arg=-Wl,--build-id=none".to_string(),
            "link-args=-Wl,-z,now".to_string()
        ]
    );
}

#[test]
fn excluded_codegen_not_in_cache_key() {
    let parsed = parse_rustc_args(
        &args(&[
            "-C",
            "metadata=abc123",
            "-C",
            "extra-filename=-abc123",
            "-C",
            "incremental=/tmp/incr",
            "-C",
            "linker=cc",
            "src/lib.rs",
        ]),
        &cwd(),
    );
    // None of these should be in ordinary codegen_flags.
    assert!(parsed.codegen_flags.is_empty());
    // But they should be in their dedicated fields.
    assert_eq!(parsed.cargo_metadata.as_deref(), Some("abc123"));
    assert_eq!(parsed.extra_filename.as_deref(), Some("-abc123"));
    assert_eq!(
        parsed.incremental_dir,
        Some(NormalizedPath::from("/tmp/incr"))
    );
    assert_eq!(parsed.linker, Some(NormalizedPath::from("/project/cc")));
}

#[test]
fn parse_extern_crates() {
    let parsed = parse_rustc_args(
        &args(&[
            "--extern",
            "serde=/target/deps/libserde.rlib",
            "--extern",
            "log=/target/deps/liblog.rmeta",
            "src/lib.rs",
        ]),
        &cwd(),
    );
    assert_eq!(parsed.externs.len(), 2);
    assert_eq!(parsed.externs[0].name, "serde");
    assert_eq!(
        parsed.externs[0].path,
        NormalizedPath::from("/target/deps/libserde.rlib")
    );
    assert_eq!(parsed.externs[1].name, "log");
}

#[test]
fn parse_extern_noprelude() {
    let parsed = parse_rustc_args(
        &args(&[
            "--extern",
            "noprelude:core=/path/libcore.rlib",
            "src/lib.rs",
        ]),
        &cwd(),
    );
    assert_eq!(parsed.externs[0].name, "core");
}

#[test]
fn search_paths_excluded_from_cache_key() {
    let parsed = parse_rustc_args(
        &args(&[
            "-L",
            "dependency=/target/deps",
            "-L",
            "native=/usr/lib",
            "src/lib.rs",
        ]),
        &cwd(),
    );
    assert_eq!(parsed.search_paths.len(), 2);
    // search_paths are stored but NOT in codegen_flags/cfgs/unknown_flags
    assert!(parsed.codegen_flags.is_empty());
    assert!(parsed.unknown_flags.is_empty());
}

#[test]
fn out_dir_excluded_from_cache_key() {
    let parsed = parse_rustc_args(
        &args(&["--out-dir", "/target/debug/deps", "src/lib.rs"]),
        &cwd(),
    );
    assert_eq!(
        parsed.out_dir,
        Some(NormalizedPath::from("/target/debug/deps"))
    );
    assert!(parsed.unknown_flags.is_empty());
}

#[test]
fn cosmetic_flags_excluded() {
    let parsed = parse_rustc_args(
        &args(&[
            "--error-format=json",
            "--json=diagnostic-rendered-ansi",
            "--color=always",
            "--diagnostic-width=80",
            "src/lib.rs",
        ]),
        &cwd(),
    );
    assert_eq!(parsed.error_format.as_deref(), Some("json"));
    assert_eq!(
        parsed.json_format.as_deref(),
        Some("diagnostic-rendered-ansi")
    );
    assert_eq!(parsed.color.as_deref(), Some("always"));
    assert_eq!(parsed.diagnostic_width.as_deref(), Some("80"));
    // None of these should be in unknown_flags
    assert!(parsed.unknown_flags.is_empty());
}

#[test]
fn parse_target() {
    let parsed = parse_rustc_args(
        &args(&["--target", "x86_64-unknown-linux-gnu", "src/lib.rs"]),
        &cwd(),
    );
    assert_eq!(parsed.target.as_deref(), Some("x86_64-unknown-linux-gnu"));
}

#[test]
fn parse_cap_lints() {
    let parsed = parse_rustc_args(&args(&["--cap-lints", "allow", "src/lib.rs"]), &cwd());
    assert_eq!(parsed.cap_lints.as_deref(), Some("allow"));
}

#[test]
fn parse_lint_flags() {
    let parsed = parse_rustc_args(
        &args(&[
            "-A",
            "dead_code",
            "-W",
            "unused",
            "-D",
            "warnings",
            "src/lib.rs",
        ]),
        &cwd(),
    );
    assert_eq!(parsed.lint_flags.len(), 3);
    assert!(parsed.lint_flags.contains(&"-A dead_code".to_string()));
    assert!(parsed.lint_flags.contains(&"-D warnings".to_string()));
    assert!(parsed.lint_flags.contains(&"-W unused".to_string()));
}

#[test]
fn parse_output_file() {
    let parsed = parse_rustc_args(&args(&["-o", "libfoo.rlib", "src/lib.rs"]), &cwd());
    assert_eq!(
        parsed.output_file,
        Some(NormalizedPath::from("/project/libfoo.rlib"))
    );
}

#[test]
fn full_cargo_invocation() {
    let parsed = parse_rustc_args(
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
            "--error-format=json",
            "--json=diagnostic-rendered-ansi,artifacts,future-incompat",
            "--diagnostic-width=211",
            "-C",
            "linker=cc",
            "src/lib.rs",
        ]),
        &cwd(),
    );

    // Cache-key fields populated
    assert_eq!(parsed.edition.as_deref(), Some("2021"));
    assert_eq!(parsed.crate_types, vec!["lib"]);
    assert_eq!(parsed.crate_name.as_deref(), Some("serde"));
    assert_eq!(parsed.emit_types, vec!["dep-info", "metadata", "link"]);
    assert!(parsed.codegen_flags.contains(&"opt-level=2".to_string()));
    assert_eq!(parsed.cap_lints.as_deref(), Some("allow"));
    assert!(parsed.cfgs.contains(&"feature=\"derive\"".to_string()));
    assert!(parsed.cfgs.contains(&"feature=\"std\"".to_string()));
    assert_eq!(parsed.externs.len(), 1);
    assert_eq!(parsed.externs[0].name, "serde_derive");

    // Excluded fields populated but NOT in cache-key collections
    assert_eq!(parsed.cargo_metadata.as_deref(), Some("abc123def"));
    assert_eq!(parsed.extra_filename.as_deref(), Some("-abc123def"));
    assert_eq!(parsed.error_format.as_deref(), Some("json"));
    assert!(parsed.search_paths.len() == 1);
    assert!(parsed.unknown_flags.is_empty());
}

#[test]
fn z_flag_with_value_captured() {
    let parsed = parse_rustc_args(
        &args(&["-Z", "macro-backtrace", "--crate-type", "lib", "src/lib.rs"]),
        &cwd(),
    );
    // -Z and its value should be combined into one entry
    assert!(
        parsed
            .unknown_flags
            .contains(&"-Z macro-backtrace".to_string()),
        "got: {:?}",
        parsed.unknown_flags
    );
}

#[test]
fn z_flag_different_values_different_keys() {
    let parsed1 = parse_rustc_args(&args(&["-Z", "query-threads=4", "src/lib.rs"]), &cwd());
    let parsed2 = parse_rustc_args(&args(&["-Z", "query-threads=8", "src/lib.rs"]), &cwd());
    assert_ne!(parsed1.unknown_flags, parsed2.unknown_flags);
}

#[test]
fn comma_separated_crate_types_split() {
    let parsed = parse_rustc_args(&args(&["--crate-type", "lib,rlib", "src/lib.rs"]), &cwd());
    assert_eq!(parsed.crate_types, vec!["lib", "rlib"]);
}

#[test]
fn relative_paths_resolved_against_cwd() {
    let parsed = parse_rustc_args(&args(&["src/lib.rs"]), &cwd());
    assert_eq!(
        parsed.source_file,
        NormalizedPath::from("/project/src/lib.rs")
    );
}

#[test]
fn absolute_paths_unchanged() {
    let parsed = parse_rustc_args(&args(&["/absolute/src/lib.rs"]), &cwd());
    assert_eq!(
        parsed.source_file,
        NormalizedPath::from("/absolute/src/lib.rs")
    );
}

#[test]
fn check_cfg_parsed() {
    let parsed = parse_rustc_args(
        &args(&["--check-cfg", "cfg(feature, values(\"std\"))", "src/lib.rs"]),
        &cwd(),
    );
    assert_eq!(parsed.check_cfgs.len(), 1);
}

#[test]
fn sysroot_parsed() {
    let parsed = parse_rustc_args(
        &args(&[
            "--sysroot",
            "/home/user/.rustup/toolchains/stable",
            "src/lib.rs",
        ]),
        &cwd(),
    );
    assert_eq!(
        parsed.sysroot,
        Some(NormalizedPath::from("/home/user/.rustup/toolchains/stable"))
    );
}

#[test]
fn remap_path_prefix_parsed() {
    let parsed = parse_rustc_args(
        &args(&["--remap-path-prefix", "/home/user=/anon", "src/lib.rs"]),
        &cwd(),
    );
    assert_eq!(parsed.remap_path_prefixes, vec!["/home/user=/anon"]);
}

#[test]
fn remap_path_prefix_equals_form_parsed() {
    let parsed = parse_rustc_args(
        &args(&["--remap-path-prefix=/home/user=/anon", "src/lib.rs"]),
        &cwd(),
    );
    assert_eq!(parsed.remap_path_prefixes, vec!["/home/user=/anon"]);
}
// ─── zackees/soldr#2313: native link-lib flags must key the unit ─────

#[test]
fn link_lib_value_is_captured_not_dropped() {
    let parsed = parse_rustc_args(
        &args(&["--crate-name", "demo", "src/main.rs", "-l", "dylib=c++"]),
        cwd().as_path(),
    );
    assert!(
        parsed.unknown_flags.contains(&"-l dylib=c++".to_string()),
        "the library NAME must be key material: {:?}",
        parsed.unknown_flags
    );
    // The value must not leak into positional/source handling: the
    // source stays main.rs (host path separators vary).
    assert_eq!(
        parsed
            .source_file
            .as_path()
            .file_name()
            .and_then(|n| n.to_str()),
        Some("main.rs")
    );
}

#[test]
fn fused_link_lib_spelling_is_captured() {
    let parsed = parse_rustc_args(&args(&["src/main.rs", "-lstatic=sqlite3"]), cwd().as_path());
    assert!(parsed
        .unknown_flags
        .contains(&"-l static=sqlite3".to_string()));
}

#[test]
fn changing_the_linked_library_changes_the_context_key() {
    let base = ["--crate-name", "demo", "--crate-type", "bin", "src/main.rs"];
    let key = |extra: &[&str]| {
        let mut all: Vec<&str> = base.to_vec();
        all.extend_from_slice(extra);
        let parsed = parse_rustc_args(&args(&all), cwd().as_path());
        crate::context::RustcCompileContext::from_parsed_args(
            &parsed,
            &[],
            zccache_hash::ContentHash::from_bytes([7u8; 32]),
        )
        .context_key()
    };
    let none = key(&[]);
    let cxx = key(&["-l", "dylib=c++"]);
    let stdcxx = key(&["-l", "dylib=stdc++"]);
    assert_ne!(none, cxx, "adding a native lib must change the key");
    assert_ne!(
            cxx, stdcxx,
            "changing the native lib NAME must change the key — the              soldr#2313 stale-link-hit bug"
        );
}
