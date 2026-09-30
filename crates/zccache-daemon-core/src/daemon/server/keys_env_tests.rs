//! #1806: env-filtering stability and collision-guard tests for cache keys.

use super::{cc_env_key_flags, request_env_fingerprint_vars as fingerprint_vars};
use crate::compiler::CompilerFamily;
use zccache_core::key_env::{GNU_CC_ENV_KEYED, MSVC_CC_ENV_KEYED};

fn env(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
    pairs
        .iter()
        .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
        .collect()
}

#[test]
fn gnu_allowlisted_vars_change_key_flags() {
    for family in [CompilerFamily::Gcc, CompilerFamily::Clang] {
        for (name, _) in GNU_CC_ENV_KEYED {
            let a = cc_env_key_flags(family, &env(&[(name, "1")]));
            let b = cc_env_key_flags(family, &env(&[(name, "2")]));
            assert_eq!(a.len(), 1, "{name} must be keyed");
            assert_ne!(a, b, "{name}: differing values must not share a key");
            assert_ne!(a, cc_env_key_flags(family, &[]), "{name} vs unset");
        }
    }
}

#[test]
fn msvc_allowlisted_vars_change_key_flags() {
    for (name, _) in MSVC_CC_ENV_KEYED {
        let a = cc_env_key_flags(CompilerFamily::Msvc, &env(&[(name, "1")]));
        let b = cc_env_key_flags(CompilerFamily::Msvc, &env(&[(name, "2")]));
        assert_ne!(a, b, "{name}");
    }
}

#[test]
fn noise_env_leaves_cc_key_flags_identical() {
    let noisy = env(&[
        ("CI", "1"),
        ("TERM", "xterm"),
        ("MAKEFLAGS", "-j8"),
        ("SOLDR_X", "1"),
        ("ZCCACHE_DISABLE", "1"),
        ("GITHUB_SHA", "abc"),
        ("LANG", "C"),
    ]);
    for family in [
        CompilerFamily::Gcc,
        CompilerFamily::Clang,
        CompilerFamily::Msvc,
    ] {
        assert_eq!(
            cc_env_key_flags(family, &noisy),
            cc_env_key_flags(family, &[])
        );
    }
}

#[test]
fn rustc_family_gets_no_cc_flags() {
    assert!(cc_env_key_flags(CompilerFamily::Rustc, &env(&[("CPATH", "/x")])).is_empty());
}

#[test]
fn request_fingerprint_keys_cc_allowlist_and_drops_noise() {
    for (name, _) in GNU_CC_ENV_KEYED.iter().chain(MSVC_CC_ENV_KEYED) {
        let e = env(&[(name, "v")]);
        assert_eq!(fingerprint_vars(Some(&e), "clang", &[]), vec![(*name, "v")]);
    }
    let noisy = env(&[
        ("CARGO_TERM_COLOR", "always"),
        ("CARGO_MAKEFLAGS", "x"),
        ("CI", "1"),
        ("SOLDR_X", "1"),
        ("ZCCACHE_DISABLE", "1"),
    ]);
    assert!(fingerprint_vars(Some(&noisy), "clang", &[]).is_empty());
}

/// A cargo-shaped rustc argv: dep-info is emitted, no proc-macro.
const CARGO_ARGS: [&str; 3] = ["--crate-name", "demo", "--emit=dep-info,metadata,link"];

#[test]
fn request_fingerprint_keeps_control_vars_and_drops_unread_cargo_vars() {
    let e = env(&[
        ("CARGO_PKG_VERSION", "1.0.0"),
        ("ZCCACHE_FAST", "1"),
        ("CARGO_MANIFEST_DIR", "/volatile"),
    ]);
    assert_eq!(
        fingerprint_vars(Some(&e), "rustc", &CARGO_ARGS.map(String::from)),
        vec![("ZCCACHE_FAST", "1")]
    );
}

/// The fingerprint must never be coarser than the rustc context key, so it
/// keeps `CARGO_*` whenever the argv could load a proc-macro dylib.
#[test]
fn request_fingerprint_keeps_cargo_vars_next_to_a_proc_macro() {
    let e = env(&[("CARGO_PKG_VERSION", "1.0.0"), ("CARGO_MANIFEST_DIR", "/v")]);
    let args: Vec<String> = [
        "--emit=dep-info,link",
        "--extern",
        "derive=/deps/libderive-abc.so",
    ]
    .map(String::from)
    .into();
    assert_eq!(
        fingerprint_vars(Some(&e), "rustc", &args),
        vec![("CARGO_PKG_VERSION", "1.0.0")]
    );
}

/// The fingerprint keys every variable the rustc context key may key.
#[test]
fn request_fingerprint_keys_rustc_env() {
    for name in [
        "RUSTC_BOOTSTRAP",
        "LIBRARY_PATH",
        "LIB",
        "LINK",
        "_LINK_",
        "TVOS_DEPLOYMENT_TARGET",
        "XROS_DEPLOYMENT_TARGET",
    ] {
        let e = env(&[(name, "v")]);
        assert_eq!(
            fingerprint_vars(Some(&e), "clang", &[]),
            vec![(name, "v")],
            "{name}"
        );
    }
}

/// gcc localizes diagnostics and zccache replays the stored stderr verbatim,
/// so the effective message locale must split gcc's key.
#[test]
fn gcc_message_locale_splits_key_flags() {
    let english = cc_env_key_flags(CompilerFamily::Gcc, &[]);
    for e in [
        env(&[("LC_ALL", "de_DE.UTF-8")]),
        env(&[("LC_MESSAGES", "fr_FR.UTF-8")]),
        env(&[("LANG", "ja_JP.UTF-8")]),
        env(&[("LANG", "en_GB.UTF-8")]),
        env(&[("LANG", "en_US.UTF-8"), ("LANGUAGE", "de")]),
    ] {
        assert_ne!(cc_env_key_flags(CompilerFamily::Gcc, &e), english, "{e:?}");
    }
    let de = cc_env_key_flags(CompilerFamily::Gcc, &env(&[("LANG", "de_DE.UTF-8")]));
    let fr = cc_env_key_flags(CompilerFamily::Gcc, &env(&[("LANG", "fr_FR.UTF-8")]));
    assert_ne!(de, fr);
    // LC_ALL outranks LANG (gettext precedence).
    let all = env(&[("LC_ALL", "de_DE.UTF-8"), ("LANG", "fr_FR.UTF-8")]);
    assert_eq!(cc_env_key_flags(CompilerFamily::Gcc, &all), de);
}

/// English-equivalent spellings must share one key so CI (`C.UTF-8`) and dev
/// machines (`en_US.UTF-8`) keep sharing artifacts.
#[test]
fn english_equivalent_locales_share_gcc_key_flags() {
    let english = cc_env_key_flags(CompilerFamily::Gcc, &[]);
    for e in [
        env(&[("LANG", "C")]),
        env(&[("LANG", "C.UTF-8")]),
        env(&[("LANG", "POSIX")]),
        env(&[("LANG", "en_US.UTF-8")]),
        env(&[("LC_ALL", "en_US.UTF-8"), ("LANG", "de_DE.UTF-8")]),
        // gettext ignores LANGUAGE under the C locale.
        env(&[("LC_ALL", "C"), ("LANGUAGE", "de")]),
        env(&[("LC_CTYPE", "de_DE.UTF-8")]),
    ] {
        assert_eq!(cc_env_key_flags(CompilerFamily::Gcc, &e), english, "{e:?}");
    }
}

/// clang does not localize diagnostics, so locale must not split its key.
#[test]
fn clang_ignores_message_locale() {
    let e = env(&[("LC_ALL", "de_DE.UTF-8")]);
    assert_eq!(
        cc_env_key_flags(CompilerFamily::Clang, &e),
        cc_env_key_flags(CompilerFamily::Clang, &[])
    );
}

#[test]
fn request_fingerprint_keeps_raw_locale_vars() {
    for name in ["LC_ALL", "LC_MESSAGES", "LANG", "LANGUAGE", "VSLANG"] {
        let e = env(&[(name, "de")]);
        assert_eq!(
            fingerprint_vars(Some(&e), "clang", &[]),
            vec![(name, "de")],
            "{name}"
        );
    }
}

/// clippy-driver and other non-plain drivers read `CARGO_*` in-process;
/// C/C++ compilers never do, even when their argv names a shared library.
#[test]
fn request_fingerprint_cargo_keying_follows_the_compiler() {
    let e = env(&[("CARGO_PKG_RUST_VERSION", "1.80")]);
    let keyed = vec![("CARGO_PKG_RUST_VERSION", "1.80")];
    let cargo = CARGO_ARGS.map(String::from);
    assert_eq!(
        fingerprint_vars(Some(&e), "/tc/bin/clippy-driver", &cargo),
        keyed
    );
    assert_eq!(
        fingerprint_vars(Some(&e), "/x/dylint-driver", &cargo),
        keyed
    );
    assert!(fingerprint_vars(Some(&e), "/tc/bin/rustc", &cargo).is_empty());
    // No dep-info: nothing reports `env!` reads, so CARGO_* stays keyed.
    assert_eq!(fingerprint_vars(Some(&e), "/tc/bin/rustc", &[]), keyed);
    let so: Vec<String> = ["-shared", "-o", "libx.so"].map(String::from).into();
    assert!(fingerprint_vars(Some(&e), "clang", &so).is_empty());
}
