//! Single owner of which environment variables may reach a cache key (#1806).
//!
//! Dropping a variable that changes compiler output makes two different
//! builds share a key and serves the wrong artifact, so every entry below
//! carries a one-line justification. Variables in neither list are *not*
//! decided here.
//!
//! rustc keying is precise: what a crate reads through `env!`/`option_env!`
//! comes from dep-info `# env-dep:` lines (folded into the artifact key, and
//! replayed on the fast path from the prior compile's recorded list), so the
//! context key carries only what rustc itself reads ([`RUSTC_ENV_KEYED`],
//! [`APPLE_ENV_KEYED`], [`LINK_ENV_KEYED`]). The one conservative exception:
//! a loaded proc-macro dylib can read any variable through an untracked
//! `std::env::var`, so [`RustcEnvFacts::may_read_env_untracked`] keeps the
//! non-volatile `CARGO_*` set keyed.
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
    ("TVOS_DEPLOYMENT_TARGET", "Apple deployment target"),
    ("WATCHOS_DEPLOYMENT_TARGET", "Apple deployment target"),
    ("XROS_DEPLOYMENT_TARGET", "Apple deployment target"),
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
    ("VSLANG", "selects the diagnostic language cl/link emit"),
];

/// Variables that select the diagnostic language. zccache replays a hit's
/// stored stderr verbatim, so for a compiler that localizes (gcc through
/// gettext) they change what a hit hands back; see [`gcc_message_locale`].
/// The request fingerprint keys them raw (finer than the context key is safe).
pub const LOCALE_ENV_KEYED: &[(&str, &str)] = &[
    ("LC_ALL", "gettext locale override"),
    ("LC_MESSAGES", "gettext message locale"),
    ("LANG", "gettext default locale"),
    ("LANGUAGE", "gettext language priority list"),
];

/// The raw [`LOCALE_ENV_KEYED`] variables present in `env`.
#[must_use]
pub fn keyed_locale_env(env: &[(String, String)]) -> Vec<(&'static str, &str)> {
    let mut out = Vec::new();
    collect(env, LOCALE_ENV_KEYED, false, &mut out);
    out
}

/// Canonical message locale gcc would resolve from `env`, or `None` when its
/// diagnostics are the untranslated English msgids.
///
/// gettext precedence is `LC_ALL` > `LC_MESSAGES` > `LANG`; `LANGUAGE` only
/// applies when that locale is not `C`/`POSIX`. The codeset (`.UTF-8`) never
/// changes the catalog, and gcc ships no `en_US` catalog, so `C`, `POSIX`,
/// `en` and `en_US` all yield `None` and keep sharing artifacts.
#[must_use]
pub fn gcc_message_locale(env: &[(String, String)]) -> Option<String> {
    let get = |name: &str| {
        env.iter()
            .rev()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
            .filter(|v| !v.is_empty())
    };
    let locale = get("LC_ALL")
        .or_else(|| get("LC_MESSAGES"))
        .or_else(|| get("LANG"))?;
    let (main, modifier) = locale.split_once('@').unwrap_or((locale, ""));
    let base = main.split('.').next().unwrap_or(main);
    if matches!(base, "" | "C" | "POSIX") {
        return None;
    }
    let language = get("LANGUAGE");
    if language.is_none() && modifier.is_empty() && matches!(base, "en" | "en_US") {
        return None;
    }
    Some(format!("{base}@{modifier}|{}", language.unwrap_or("")))
}

/// `CARGO_*` environment variables that must NOT participate in the cache key.
///
/// These are volatile (absolute paths or build-host transients) and either do
/// not affect compiled output or affect it only via paths that should already
/// be normalized elsewhere. Including them cascades cache invalidation across
/// the entire dep graph whenever the workspace is moved, cloned, or re-checked
/// out at a different on-disk location.
///
/// What stays in the key (everything else starting with `CARGO_`):
/// - `CARGO_PKG_VERSION`, `CARGO_PKG_NAME`, `CARGO_PKG_AUTHORS`,
///   `CARGO_PKG_DESCRIPTION`, `CARGO_PKG_HOMEPAGE`, `CARGO_PKG_REPOSITORY`,
///   `CARGO_PKG_LICENSE`, `CARGO_PKG_RUST_VERSION`, `CARGO_CRATE_NAME`, etc.
///   These feed `env!()` macros and are baked into the compiled artifact.
///
/// Already excluded earlier in the filter (orthogonal reasons):
/// - `CARGO_MAKEFLAGS` (job-server token, transient).
/// - `CARGO_INCREMENTAL` (handled by stripping `-C incremental` from args).
///
/// Filtered here (this list):
/// - `CARGO_MANIFEST_DIR` — absolute path to the crate dir; changes per
///   checkout location. Cascades the cache.
/// - `CARGO_MANIFEST_PATH` — absolute path to `Cargo.toml`; same issue.
/// - `CARGO_TARGET_DIR` — output-placement state set by cargo. Two worktrees
///   that share a zccache cache but pick different relative target-dir leaf
///   names (e.g. `parent-cache-main-target` vs `parent-cache-sub-target`)
///   otherwise cold-miss every rustc compilation even with
///   `ZCCACHE_PATH_REMAP=auto`. Filtering is sound because `CARGO_TARGET_DIR`
///   only directs cargo where to place build output — it is not embedded in
///   rustc output via `env!()` in normal builds, and `--out-dir` / `-L` /
///   `--extern` directory prefixes that cargo derives from it are already
///   non-cache-key state (out_dir excluded; search_paths excluded; extern
///   paths reduced to file-name identity). See issue #396.
/// - `CARGO` / `CARGO_HOME` — Cargo's executable and package/tool state
///   directory. Soldr deliberately relocates both beneath each cache root, so
///   restoring an otherwise identical cache into a new root must not re-key
///   every rustc invocation. They affect orchestration and dependency lookup,
///   not the bytes rustc emits for a resolved invocation. See issue #1625.
/// - `CARGO_TARGET_<TRIPLE>_LINKER` — Cargo consumes this selector and turns
///   it into rustc linker arguments. Soldr points it at a cache-root-local
///   shim, so the raw environment path is orchestration state and must not
///   independently re-key unrelated compile units.
const VOLATILE_CARGO_ENV_VARS: &[&str] = &[
    "CARGO",
    "CARGO_HOME",
    "CARGO_MANIFEST_DIR",
    "CARGO_MANIFEST_PATH",
    "CARGO_TARGET_DIR",
];

/// Whether a Cargo environment variable describes orchestration or relocated
/// tool state rather than compiler output identity.
#[must_use]
pub fn is_volatile_cargo_env_var(key: &str) -> bool {
    VOLATILE_CARGO_ENV_VARS.contains(&key)
        || key
            .strip_prefix("CARGO_TARGET_")
            .is_some_and(|target_key| target_key.ends_with("_LINKER"))
}

/// Whether `name` is a `CARGO_*` variable that stays keyed when code in the
/// compiler process may read it invisibly ([`RustcEnvFacts::may_read_env_untracked`],
/// the Dylint input hash): not volatile and not on the never-keyed list.
#[must_use]
pub fn is_conservatively_keyed_cargo_env(name: &str) -> bool {
    name.starts_with("CARGO_") && !is_never_keyed_env(name) && !is_volatile_cargo_env_var(name)
}

/// Variables rustc itself reads that change what it accepts or emits.
pub const RUSTC_ENV_KEYED: &[(&str, &str)] = &[(
    "RUSTC_BOOTSTRAP",
    "unlocks nightly features on stable and changes accepted programs",
)];

/// Apple variables rustc reads when the target is an Apple triple.
pub const APPLE_ENV_KEYED: &[(&str, &str)] = &[
    ("SDKROOT", "Apple sysroot selection"),
    ("MACOSX_DEPLOYMENT_TARGET", "macOS minimum-OS load command"),
    ("IPHONEOS_DEPLOYMENT_TARGET", "iOS minimum-OS load command"),
    ("TVOS_DEPLOYMENT_TARGET", "tvOS minimum-OS load command"),
    (
        "WATCHOS_DEPLOYMENT_TARGET",
        "watchOS minimum-OS load command",
    ),
    ("XROS_DEPLOYMENT_TARGET", "visionOS minimum-OS load command"),
];

/// Variables the linker rustc spawns reads; they shape bin / dylib / cdylib /
/// proc-macro output (search dirs, subprogram lookup) but not an rlib.
pub const LINK_ENV_KEYED: &[(&str, &str)] = &[
    ("LIBRARY_PATH", "extra gcc/clang link search dirs"),
    ("COMPILER_PATH", "changes which subprograms gcc runs"),
    ("GCC_EXEC_PREFIX", "changes which subprograms gcc finds"),
    ("LIB", "MSVC link.exe library search dirs"),
    ("LINK", "extra arguments prepended to link.exe argv"),
    ("_LINK_", "extra arguments appended to link.exe argv"),
];

/// What a rustc invocation makes environment-sensitive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RustcEnvFacts {
    /// Target is Apple (or unknown, i.e. the host): Apple vars apply.
    pub apple: bool,
    /// A link step runs: linker vars apply.
    pub links: bool,
    /// Environment reads may be invisible to dep-info, so the `CARGO_*` set
    /// must stay keyed: no dep-info is emitted (direct `rustc` runs record no
    /// `# env-dep:` lines at all), a proc-macro dylib is loaded (it can call
    /// `std::env::var` untracked), or the driver is not plain `rustc`
    /// (clippy reads `CARGO_PKG_RUST_VERSION` for its MSRV, Dylint loads lint
    /// libraries).
    pub may_read_env_untracked: bool,
}

impl RustcEnvFacts {
    /// Derive the facts from parsed rustc arguments.
    #[must_use]
    pub fn from_parts<'a>(
        target: Option<&str>,
        crate_types: &[String],
        emit_types: &[String],
        extern_paths: impl IntoIterator<Item = &'a str>,
        plain_rustc: bool,
    ) -> Self {
        let apple = target.is_none_or(|t| t.contains("apple"));
        let output_links = crate_types.is_empty()
            || crate_types
                .iter()
                .flat_map(|t| t.split(','))
                .any(|t| matches!(t.trim(), "bin" | "dylib" | "cdylib" | "proc-macro"));
        let emits_link = emit_types.is_empty() || emit_types.iter().any(|e| e == "link");
        Self {
            apple,
            links: output_links && emits_link,
            may_read_env_untracked: !plain_rustc
                || !emit_types.iter().any(|e| e == "dep-info")
                || extern_paths.into_iter().any(is_dylib_path),
        }
    }
}

/// Whether `compiler` is the plain `rustc` driver (basename `rustc`, with or
/// without `.exe`). `clippy-driver`, `dylint-driver` and any wrapper can
/// execute code that reads the environment invisibly.
#[must_use]
pub fn is_plain_rustc(compiler: &str) -> bool {
    let base = compiler.rsplit(['/', '\\']).next().unwrap_or(compiler);
    base.strip_suffix(".exe").unwrap_or(base) == "rustc"
}

/// Whether `path` names a dynamic library, the only form a proc-macro extern
/// takes (`.so`, `.dylib`, `.dll`).
#[must_use]
pub fn is_dylib_path(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    [".so", ".dylib", ".dll"]
        .iter()
        .any(|ext| lower.ends_with(ext))
}

/// Whether the rustc argv emits dep-info (`--emit=dep-info[=path]` in any of
/// its spellings), the only source of `# env-dep:` lines. Mirrors the parser
/// in `zccache-depgraph`; must be exact, not loose, because a wrong `true`
/// would make the request fingerprint coarser than the context key.
#[must_use]
pub fn argv_emits_dep_info(args: &[String]) -> bool {
    let mut i = 0;
    while i < args.len() {
        let value = if args[i] == "--emit" {
            i += 1;
            args.get(i).map(String::as_str)
        } else {
            args[i].strip_prefix("--emit=")
        };
        if value.is_some_and(|v| {
            v.split(',')
                .any(|part| part.split('=').next().unwrap_or(part) == "dep-info")
        }) {
            return true;
        }
        i += 1;
    }
    false
}

/// Loose superset of "this argv loads a proc-macro": any argument naming a
/// dynamic library. Used by the request fingerprint, which must never be
/// coarser than the context key ([`RustcEnvFacts::may_read_env_untracked`]).
#[must_use]
pub fn argv_may_load_proc_macro(args: &[String]) -> bool {
    args.iter().any(|a| is_dylib_path(a))
}

/// The rustc-relevant variables present in `env` for `facts`, in table order
/// with last-wins values. Excludes the conservative `CARGO_*` set, which the
/// caller adds when `facts.may_read_env_untracked`.
#[must_use]
pub fn keyed_rustc_env(
    env: &[(String, String)],
    facts: RustcEnvFacts,
) -> Vec<(&'static str, &str)> {
    let mut out = Vec::new();
    collect(env, RUSTC_ENV_KEYED, false, &mut out);
    if facts.apple {
        collect(env, APPLE_ENV_KEYED, false, &mut out);
    }
    if facts.links {
        collect_link_table(env, &mut out);
    }
    out
}

/// Variables a link / archive invocation reads: the linker search and
/// subprogram variables plus the Apple deployment set (#1806).
#[must_use]
pub fn keyed_link_env(env: &[(String, String)]) -> Vec<(&'static str, &str)> {
    let mut out = Vec::new();
    collect_link_table(env, &mut out);
    collect(env, APPLE_ENV_KEYED, false, &mut out);
    out
}

/// Every variable any rustc invocation may key: the request fingerprint uses
/// this superset because a finer fingerprint only costs a request-cache miss,
/// while a coarser one would serve the wrong context.
#[must_use]
pub fn keyed_rustc_env_superset(env: &[(String, String)]) -> Vec<(&'static str, &str)> {
    keyed_rustc_env(
        env,
        RustcEnvFacts {
            apple: true,
            links: true,
            may_read_env_untracked: false,
        },
    )
}

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
        collect(env, table, std::ptr::eq(table, MSVC_CC_ENV_KEYED), &mut out);
    }
    out
}

/// Linker variables are case-insensitive on Windows and exact elsewhere; the
/// key must not depend on the host, so an exact name wins and a differently
/// cased one is used only when no exact match exists (a stray `lib` on Unix
/// costs at most an extra split).
fn collect_link_table<'a>(env: &'a [(String, String)], out: &mut Vec<(&'static str, &'a str)>) {
    for (name, _) in LINK_ENV_KEYED {
        let find = |exact: bool| {
            env.iter().rev().find_map(|(k, v)| {
                let hit = if exact {
                    k == name
                } else {
                    k.eq_ignore_ascii_case(name)
                };
                hit.then_some(v.as_str())
            })
        };
        if let Some(value) = find(true).or_else(|| find(false)) {
            out.push((*name, value));
        }
    }
}

fn collect<'a>(
    env: &'a [(String, String)],
    table: &'static [(&'static str, &'static str)],
    case_insensitive: bool,
    out: &mut Vec<(&'static str, &'a str)>,
) {
    for (name, _) in table {
        let found = env.iter().rev().find_map(|(k, v)| {
            let hit = if case_insensitive {
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

    fn strs(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn rustc_facts_follow_target_crate_type_emit_and_externs() {
        let f = |target, types: &[&str], emit: &[&str], ext: &[&str]| {
            let mut emit = emit.to_vec();
            emit.push("dep-info");
            RustcEnvFacts::from_parts(
                target,
                &strs(types),
                &strs(&emit),
                ext.iter().copied(),
                true,
            )
        };
        assert!(f(Some("aarch64-apple-ios"), &["lib"], &[], &[]).apple);
        assert!(
            f(None, &["lib"], &[], &[]).apple,
            "unknown host may be Apple"
        );
        assert!(!f(Some("x86_64-pc-windows-msvc"), &["lib"], &[], &[]).apple);
        assert!(
            f(None, &[], &["link"], &[]).links,
            "no crate type defaults to bin"
        );
        assert!(
            RustcEnvFacts::from_parts(None, &[], &[], [], true).links,
            "no --emit defaults to link"
        );
        assert!(f(None, &["lib,cdylib"], &["link"], &[]).links);
        assert!(!f(None, &["lib", "staticlib"], &["link"], &[]).links);
        assert!(!f(None, &["bin"], &["metadata"], &[]).links);
        assert!(f(None, &["lib"], &[], &["/d/libm.so"]).may_read_env_untracked);
        assert!(!f(None, &["lib"], &[], &["/d/libm.rlib", "/d/libm.rmeta"]).may_read_env_untracked);
        let no_dep_info = RustcEnvFacts::from_parts(None, &[], &strs(&["link"]), [], true);
        assert!(
            no_dep_info.may_read_env_untracked,
            "without dep-info nothing reports env reads"
        );
        let clippy = RustcEnvFacts::from_parts(None, &[], &strs(&["dep-info"]), [], false);
        assert!(clippy.may_read_env_untracked, "non-rustc drivers read env");
    }

    #[test]
    fn rustc_env_is_gated_by_facts() {
        let e = env(&[
            ("RUSTC_BOOTSTRAP", "1"),
            ("SDKROOT", "/sdk"),
            ("LIBRARY_PATH", "/l"),
            ("CARGO_PKG_NAME", "x"),
        ]);
        let all = RustcEnvFacts {
            apple: true,
            links: true,
            may_read_env_untracked: false,
        };
        let names = |f| {
            keyed_rustc_env(&e, f)
                .iter()
                .map(|(n, _)| *n)
                .collect::<Vec<_>>()
        };
        assert_eq!(names(all), ["RUSTC_BOOTSTRAP", "SDKROOT", "LIBRARY_PATH"]);
        let none = RustcEnvFacts {
            apple: false,
            links: false,
            may_read_env_untracked: false,
        };
        assert_eq!(names(none), ["RUSTC_BOOTSTRAP"]);
        assert_eq!(keyed_rustc_env_superset(&e), keyed_rustc_env(&e, all));
    }

    #[test]
    fn dep_info_emission_is_detected_in_every_spelling() {
        let yes: &[&[&str]] = &[
            &["--emit=dep-info,metadata,link"],
            &["--emit", "link,dep-info"],
            &["--emit=dep-info=/t/x.d"],
            &["--emit=link", "--emit", "dep-info"],
        ];
        for a in yes {
            assert!(argv_emits_dep_info(&strs(a)), "{a:?}");
        }
        let no: &[&[&str]] = &[
            &[],
            &["--emit=link"],
            &["--emit", "metadata"],
            &["-o", "dep-info", "x.rs"],
            &["--emit"],
        ];
        for a in no {
            assert!(!argv_emits_dep_info(&strs(a)), "{a:?}");
        }
    }

    #[test]
    fn plain_rustc_detection() {
        for c in ["rustc", "/tc/bin/rustc", r"C:\tc\rustc.exe"] {
            assert!(is_plain_rustc(c), "{c}");
        }
        for c in [
            "clippy-driver",
            "/x/dylint-driver",
            "rustc-wrapper",
            "sccache",
        ] {
            assert!(!is_plain_rustc(c), "{c}");
        }
    }

    #[test]
    fn link_env_prefers_exact_case_and_falls_back_to_any_case() {
        let e = env(&[("LIB", "exact"), ("lib", "other")]);
        assert_eq!(keyed_link_env(&e), vec![("LIB", "exact")]);
        assert_eq!(keyed_link_env(&env(&[("Lib", "x")])), vec![("LIB", "x")]);
        assert!(keyed_link_env(&env(&[("LIBS", "x")])).is_empty());
    }

    #[test]
    fn dylib_detection_is_case_insensitive_and_extension_exact() {
        for p in ["/a/libm.so", r"C:\a\M.DLL", "libm.dylib"] {
            assert!(is_dylib_path(p), "{p}");
        }
        for p in ["libm.rlib", "libm.rmeta", "libm.so.1", "dir.so/file.rs"] {
            assert!(!is_dylib_path(p), "{p}");
        }
        assert!(argv_may_load_proc_macro(&strs(&[
            "--extern",
            "d=/x/libd.so"
        ])));
        assert!(!argv_may_load_proc_macro(&strs(&[
            "--extern",
            "d=/x/libd.rlib"
        ])));
    }

    #[test]
    fn gcc_message_locale_resolves_like_gettext() {
        let loc = |p: &[(&str, &str)]| gcc_message_locale(&env(p));
        assert_eq!(loc(&[]), None);
        assert_eq!(loc(&[("LANG", "C.UTF-8")]), None);
        assert_eq!(loc(&[("LANG", "en_US.UTF-8")]), None);
        assert_eq!(
            loc(&[("LANG", "de_DE.UTF-8")]),
            loc(&[("LANG", "de_DE.ISO-8859-1")])
        );
        assert_ne!(loc(&[("LANG", "de_DE")]), None);
        assert_ne!(loc(&[("LANG", "sr_RS@latin")]), loc(&[("LANG", "sr_RS")]));
        assert_ne!(loc(&[("LANG", "en_US"), ("LANGUAGE", "de")]), None);
        assert_eq!(
            loc(&[("LC_ALL", "C"), ("LANG", "de_DE"), ("LANGUAGE", "de")]),
            None
        );
        assert_eq!(loc(&[("LANG", "")]), None, "empty counts as unset");
    }
}
