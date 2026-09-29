//! #1806: env-filtering stability and collision-guard tests for cache keys.

use super::{cc_env_key_flags, request_env_fingerprint_vars};
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
        assert_eq!(request_env_fingerprint_vars(Some(&e)), vec![(*name, "v")]);
    }
    let noisy = env(&[
        ("CARGO_TERM_COLOR", "always"),
        ("CARGO_MAKEFLAGS", "x"),
        ("CI", "1"),
        ("SOLDR_X", "1"),
        ("ZCCACHE_DISABLE", "1"),
    ]);
    assert!(request_env_fingerprint_vars(Some(&noisy)).is_empty());
}

#[test]
fn request_fingerprint_keeps_cargo_and_control_vars() {
    let e = env(&[
        ("CARGO_PKG_VERSION", "1.0.0"),
        ("ZCCACHE_FAST", "1"),
        ("CARGO_MANIFEST_DIR", "/volatile"),
    ]);
    assert_eq!(
        request_env_fingerprint_vars(Some(&e)),
        vec![("CARGO_PKG_VERSION", "1.0.0"), ("ZCCACHE_FAST", "1")]
    );
}
