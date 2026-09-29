//! Single owner of which environment variables may reach a cache key (#1806).
//!
//! Dropping a variable that changes compiler output makes two different
//! builds share a key and serves the wrong artifact, so every entry below
//! carries a one-line justification. Variables in neither list are *not*
//! decided here: callers keep their existing conservative behaviour (the
//! rustc path keys every non-excluded `CARGO_*` variable) until classified.
//!
//! Precise tracking still wins over the never-keyed list: a crate that reads
//! `env!("CARGO_TERM_COLOR")` reports it as a rustc `# env-dep:` and is keyed
//! for that crate only.

/// Which compiler family's allowlist to consult.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CcEnvScope {
    /// gcc / clang.
    Gnu,
    /// MSVC `cl.exe` (names are case-insensitive on Windows).
    Msvc,
    /// Union of both; used by the compiler-agnostic request fingerprint.
    Any,
}

/// Variables gcc/clang read that change output or the include set.
pub const GNU_CC_ENV_KEYED: &[(&str, &str)] = &[
    ("CPATH", "extra include search dirs"),
    ("C_INCLUDE_PATH", "extra C include search dirs"),
    ("CPLUS_INCLUDE_PATH", "extra C++ include search dirs"),
    ("OBJC_INCLUDE_PATH", "extra Objective-C include search dirs"),
    ("SDKROOT", "Apple sysroot selection"),
    ("MACOSX_DEPLOYMENT_TARGET", "Apple deployment target"),
    ("IPHONEOS_DEPLOYMENT_TARGET", "Apple deployment target"),
    (
        "GCC_EXEC_PREFIX",
        "changes which subprograms and headers gcc finds",
    ),
    ("COMPILER_PATH", "changes which subprograms gcc runs"),
    ("SOURCE_DATE_EPOCH", "changes __DATE__ / __TIME__ expansion"),
    ("DEPENDENCIES_OUTPUT", "changes the emitted depfile"),
    ("SUNPRO_DEPENDENCIES", "changes the emitted depfile"),
];

/// Variables `cl.exe` reads that change output or the include set.
pub const MSVC_CC_ENV_KEYED: &[(&str, &str)] = &[
    ("CL", "extra arguments prepended to argv"),
    ("_CL_", "extra arguments appended to argv"),
    ("INCLUDE", "system include search dirs"),
];

/// Non-`CARGO_*` control variables that stay keyed until scanning is proven
/// to re-validate on its own (#1806 item 3).
pub const REQUEST_CONTROL_ENV_KEYED: &[&str] = &[
    "ZCCACHE_FAST",
    "ZCCACHE_SCAN_SYSTEM_HEADERS",
    "ZCCACHE_DYLINT_CACHE_INPUT_HASH",
];

/// Exact names that never reach a key.
const NEVER_KEYED_EXACT: &[(&str, &str)] = &[
    ("CI", "CI marker, no effect on output"),
    ("TERM", "terminal capability noise"),
    ("MAKEFLAGS", "jobserver, not output"),
    ("CARGO_MAKEFLAGS", "jobserver, not output"),
    ("CARGO_INCREMENTAL", "handled by stripping -C incremental"),
];

/// Prefixes that never reach a key.
const NEVER_KEYED_PREFIXES: &[(&str, &str)] = &[
    ("GITHUB_", "CI runner noise"),
    ("SOLDR_", "soldr control variables"),
    (
        "CARGO_TERM_",
        "cargo terminal styling; cargo passes explicit flags",
    ),
];

/// Whether `name` is on the never-keyed list.
///
/// `ZCCACHE_*` is never keyed except [`REQUEST_CONTROL_ENV_KEYED`].
#[must_use]
pub fn is_never_keyed_env(name: &str) -> bool {
    if REQUEST_CONTROL_ENV_KEYED.contains(&name) {
        return false;
    }
    name.starts_with("ZCCACHE_")
        || NEVER_KEYED_EXACT.iter().any(|(n, _)| *n == name)
        || NEVER_KEYED_PREFIXES
            .iter()
            .any(|(p, _)| name.starts_with(p))
}

fn scope_tables(scope: CcEnvScope) -> [&'static [(&'static str, &'static str)]; 2] {
    const NONE: &[(&str, &str)] = &[];
    match scope {
        CcEnvScope::Gnu => [GNU_CC_ENV_KEYED, NONE],
        CcEnvScope::Msvc => [MSVC_CC_ENV_KEYED, NONE],
        CcEnvScope::Any => [GNU_CC_ENV_KEYED, MSVC_CC_ENV_KEYED],
    }
}

/// Return the allowlisted C/C++ variables present in `env`, in allowlist
/// order, each with its canonical name and the *last* matching value (mirrors
/// `Command::env` replacement). MSVC names match case-insensitively.
#[must_use]
pub fn keyed_cc_env(env: &[(String, String)], scope: CcEnvScope) -> Vec<(&'static str, &str)> {
    let mut out = Vec::new();
    for table in scope_tables(scope) {
        let ci = std::ptr::eq(table, MSVC_CC_ENV_KEYED);
        for (name, _) in table {
            let found = env.iter().rev().find_map(|(k, v)| {
                let hit = if ci {
                    k.eq_ignore_ascii_case(name)
                } else {
                    k == name
                };
                hit.then_some(v.as_str())
            });
            if let Some(value) = found {
                out.push((*name, value));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).into(), (*v).into()))
            .collect()
    }

    #[test]
    fn every_gnu_allowlisted_var_is_collected_and_value_sensitive() {
        for (name, _) in GNU_CC_ENV_KEYED {
            let (ea, eb) = (env(&[(name, "a")]), env(&[(name, "b")]));
            let a = keyed_cc_env(&ea, CcEnvScope::Gnu);
            let b = keyed_cc_env(&eb, CcEnvScope::Gnu);
            assert_eq!(a, vec![(*name, "a")], "{name} must be keyed");
            assert_ne!(a, b, "{name} value must change the key input");
        }
    }

    #[test]
    fn every_msvc_allowlisted_var_is_collected_case_insensitively() {
        for (name, _) in MSVC_CC_ENV_KEYED {
            let lower = name.to_ascii_lowercase();
            let e = env(&[(&lower, "x")]);
            let got = keyed_cc_env(&e, CcEnvScope::Msvc);
            assert_eq!(got, vec![(*name, "x")], "{name}");
        }
    }

    #[test]
    fn last_duplicate_wins() {
        let e = env(&[("CL", "1"), ("cl", "2")]);
        let got = keyed_cc_env(&e, CcEnvScope::Msvc);
        assert_eq!(got, vec![("CL", "2")]);
    }

    #[test]
    fn scope_isolation_and_union() {
        let e = env(&[("CPATH", "/a"), ("INCLUDE", "/b")]);
        assert_eq!(keyed_cc_env(&e, CcEnvScope::Gnu), vec![("CPATH", "/a")]);
        assert_eq!(keyed_cc_env(&e, CcEnvScope::Msvc), vec![("INCLUDE", "/b")]);
        assert_eq!(keyed_cc_env(&e, CcEnvScope::Any).len(), 2);
    }

    #[test]
    fn noise_is_not_collected() {
        let e = env(&[
            ("CI", "1"),
            ("PATH", "/x"),
            ("SOLDR_FOO", "1"),
            ("HOME", "/h"),
        ]);
        assert!(keyed_cc_env(&e, CcEnvScope::Any).is_empty());
    }

    #[test]
    fn never_keyed_list() {
        for n in [
            "CI",
            "TERM",
            "MAKEFLAGS",
            "CARGO_MAKEFLAGS",
            "CARGO_INCREMENTAL",
            "GITHUB_SHA",
            "SOLDR_CACHE",
            "CARGO_TERM_COLOR",
            "ZCCACHE_DISABLE",
        ] {
            assert!(is_never_keyed_env(n), "{n}");
        }
    }

    #[test]
    fn collision_guard_output_affecting_vars_are_never_excluded() {
        let all = GNU_CC_ENV_KEYED
            .iter()
            .chain(MSVC_CC_ENV_KEYED)
            .map(|(n, _)| *n)
            .chain(REQUEST_CONTROL_ENV_KEYED.iter().copied())
            .chain([
                "CARGO_PKG_VERSION",
                "CARGO_CRATE_NAME",
                "CARGO_PKG_NAME",
                "RUSTC_BOOTSTRAP",
            ]);
        for n in all {
            assert!(
                !is_never_keyed_env(n),
                "{n} affects output and must stay keyable"
            );
        }
    }
}
