//! #1806: which client environment variables reach the rustc context key.
//!
//! `CARGO_*` no longer keys blindly: a crate that reads a variable reports it
//! as a dep-info `# env-dep:` line, which the artifact key folds in. The
//! context key keeps only what rustc itself reads (`RUSTC_BOOTSTRAP`, Apple
//! deployment vars, linker env) plus, defensively, every non-volatile
//! `CARGO_*` when a proc-macro dylib is loaded (its untracked
//! `std::env::var` reads are invisible to dep-info).

use std::path::Path;

use crate::rustc_args::parse_rustc_args;

use super::super::{ContextKey, RustcCompileContext};
use super::test_compiler_hash;

/// Context key for `args` (cargo-shaped: dep-info is emitted unless the
/// caller names its own `--emit`).
fn key(args: &[&str], env: &[(&str, &str)]) -> ContextKey {
    key_with(args, env, true)
}

fn key_with(args: &[&str], env: &[(&str, &str)], default_emit: bool) -> ContextKey {
    let mut argv: Vec<String> = args.iter().map(|a| (*a).to_string()).collect();
    if default_emit && !args.iter().any(|a| a.starts_with("--emit")) {
        argv.push("--emit=dep-info,metadata,link".to_string());
    }
    argv.push("src/lib.rs".to_string());
    let parsed = parse_rustc_args(&argv, Path::new("/workspace"));
    let env: Vec<(String, String)> = env
        .iter()
        .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
        .collect();
    RustcCompileContext::from_parsed_args(&parsed, &env, test_compiler_hash()).context_key()
}

const PROC_MACRO: &[&str] = &["--extern", "derive=/t/deps/libderive-abc.so"];

/// Stability: a crate that never reads a variable must share one key across
/// every value of it (no proc-macro is loaded, so nothing can read it
/// invisibly).
#[test]
fn unread_cargo_env_does_not_split_the_key() {
    let base = key(&[], &[("CARGO_PKG_NAME", "demo")]);
    for name in [
        "CARGO_PKG_DESCRIPTION",
        "CARGO_PKG_AUTHORS",
        "CARGO_PKG_VERSION",
        "CARGO_CRATE_NAME",
        "CARGO_PKG_HOMEPAGE",
    ] {
        let other = key(&[], &[("CARGO_PKG_NAME", "renamed"), (name, "x")]);
        assert_eq!(base, other, "{name} must not split an env-free crate's key");
    }
    assert_eq!(key(&[], &[]), base);
}

/// Stability: never-keyed variables leave the key identical even with a
/// proc-macro loaded.
#[test]
fn never_keyed_env_does_not_change_rustc_context_key() {
    let base = key(PROC_MACRO, &[("CARGO_PKG_NAME", "demo")]);
    for name in [
        "CARGO_TERM_COLOR",
        "CARGO_TERM_PROGRESS_WHEN",
        "CARGO_MAKEFLAGS",
        "CARGO_INCREMENTAL",
        "CARGO_MANIFEST_DIR",
        "SOLDR_CACHE",
        "CI",
    ] {
        let noisy = key(PROC_MACRO, &[("CARGO_PKG_NAME", "demo"), (name, "x")]);
        assert_eq!(base, noisy, "{name} must not split the key");
    }
}

/// Collision guard: with a proc-macro loaded, `CARGO_*` reads that dep-info
/// cannot see still split keys.
#[test]
fn cargo_env_splits_key_when_a_proc_macro_is_loaded() {
    for name in [
        "CARGO_PKG_VERSION",
        "CARGO_PKG_DESCRIPTION",
        "CARGO_CRATE_NAME",
    ] {
        let a = key(PROC_MACRO, &[(name, "1")]);
        let b = key(PROC_MACRO, &[(name, "2")]);
        assert_ne!(a, b, "{name} must change the key next to a proc-macro");
    }
}

/// `.dylib` and `.dll` proc-macros are recognised too.
#[test]
fn every_dylib_extension_counts_as_a_proc_macro() {
    for path in ["/t/libd.dylib", "/t/d.dll", "/t/libd.so"] {
        let arg = format!("d={path}");
        let a = key(&["--extern", &arg], &[("CARGO_PKG_VERSION", "1")]);
        let b = key(&["--extern", &arg], &[("CARGO_PKG_VERSION", "2")]);
        assert_ne!(a, b, "{path}");
    }
    let rlib = ["--extern", "d=/t/libd-1.rlib"];
    assert_eq!(
        key(&rlib, &[("CARGO_PKG_VERSION", "1")]),
        key(&rlib, &[("CARGO_PKG_VERSION", "2")]),
        "an rlib extern cannot expand macros"
    );
}

/// Collision guard: `RUSTC_BOOTSTRAP` changes what rustc accepts and emits.
#[test]
fn rustc_bootstrap_splits_the_key() {
    let off = key(&[], &[]);
    let on = key(&[], &[("RUSTC_BOOTSTRAP", "1")]);
    let other = key(&[], &[("RUSTC_BOOTSTRAP", "demo")]);
    assert_ne!(off, on);
    assert_ne!(on, other);
}

/// Collision guard: Apple deployment/sysroot vars split keys for Apple
/// targets (and the unknown host) but not for unrelated targets.
#[test]
fn apple_env_splits_the_key_only_for_apple_targets() {
    for name in [
        "SDKROOT",
        "MACOSX_DEPLOYMENT_TARGET",
        "IPHONEOS_DEPLOYMENT_TARGET",
        "TVOS_DEPLOYMENT_TARGET",
        "WATCHOS_DEPLOYMENT_TARGET",
        "XROS_DEPLOYMENT_TARGET",
    ] {
        let apple = ["--target", "aarch64-apple-darwin"];
        assert_ne!(
            key(&apple, &[(name, "11.0")]),
            key(&apple, &[(name, "12.0")]),
            "{name}"
        );
        assert_ne!(key(&[], &[(name, "11.0")]), key(&[], &[(name, "12.0")]));
        let linux = ["--target", "x86_64-unknown-linux-gnu"];
        assert_eq!(
            key(&linux, &[(name, "11.0")]),
            key(&linux, &[(name, "12.0")]),
            "{name} is irrelevant to a linux target"
        );
    }
}

/// Collision guard: the linker's environment shapes bin/dylib output but is
/// irrelevant to an rlib.
#[test]
fn linker_env_splits_only_linked_outputs() {
    for name in [
        "LIBRARY_PATH",
        "COMPILER_PATH",
        "GCC_EXEC_PREFIX",
        "LIB",
        "LINK",
        "_LINK_",
    ] {
        for crate_type in ["bin", "cdylib", "dylib", "proc-macro"] {
            let ct = ["--crate-type", crate_type];
            assert_ne!(
                key(&ct, &[(name, "/a")]),
                key(&ct, &[(name, "/b")]),
                "{name} must split a {crate_type}"
            );
        }
        let lib = ["--crate-type", "lib"];
        assert_eq!(
            key(&lib, &[(name, "/a")]),
            key(&lib, &[(name, "/b")]),
            "{name} must not split an rlib"
        );
        let meta = ["--crate-type", "bin", "--emit=dep-info,metadata"];
        assert_eq!(
            key(&meta, &[(name, "/a")]),
            key(&meta, &[(name, "/b")]),
            "{name} must not split a metadata-only (check) compile"
        );
    }
}

/// Collision guard: `clippy-driver` reads `CARGO_PKG_RUST_VERSION` (its MSRV)
/// inside the process, invisibly to dep-info, so a non-plain driver keeps the
/// `CARGO_*` set keyed even without a proc-macro.
#[test]
fn non_plain_rustc_driver_keeps_cargo_env_keyed() {
    let parsed = parse_rustc_args(
        &["--emit=dep-info,link".to_string(), "src/lib.rs".to_string()],
        Path::new("/workspace"),
    );
    let key = |plain: bool, msrv: &str| {
        let env = vec![("CARGO_PKG_RUST_VERSION".to_string(), msrv.to_string())];
        RustcCompileContext::from_parsed_args_with_driver(
            &parsed,
            &env,
            test_compiler_hash(),
            plain,
        )
        .context_key()
    };
    assert_ne!(key(false, "1.70"), key(false, "1.80"));
    assert_eq!(key(true, "1.70"), key(true, "1.80"));
}

/// Collision guard: env-deps come only from dep-info. A direct `rustc` run
/// that emits none has no record of what `env!()` read, so the `CARGO_*` set
/// must stay keyed or two builds would share one artifact.
#[test]
fn cargo_env_splits_key_when_no_dep_info_is_emitted() {
    for emit in [&["--emit=link"][..], &["--emit=metadata,link"], &[]] {
        assert_ne!(
            key_with(emit, &[("CARGO_PKG_VERSION", "1")], false),
            key_with(emit, &[("CARGO_PKG_VERSION", "2")], false),
            "{emit:?}"
        );
    }
}
