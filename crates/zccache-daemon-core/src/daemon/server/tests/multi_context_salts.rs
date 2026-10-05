//! #1901: the multi-source `CompileContext` builder must carry the SAME key
//! salts the single-source builder carries.
//!
//! The single-source path (`super::super::rustc`'s `build_cc_compile_context`)
//! folds two extra inputs into `ctx.flags` after `from_parsed_args`:
//! `keys::cc_env_key_flags` (the `#1806` compiler-env allowlist plus gcc's
//! message locale) and `keys::msvc_show_includes_key_flags` (the `#1530`
//! caller-passed `/showIncludes` salt). The multi-source builder computed what
//! is supposed to be the same context and stopped short, so `check_unit_cache`
//! cloned an unsalted `shared_base` into every unit of the batch.
//!
//! Every test here targets the MULTI-source builder specifically. The
//! single-source salts are already covered by `keys.rs`'s
//! `show_includes_key_tests` and `keys_env_tests.rs`; those pass today and
//! prove nothing about this path. The invariant under test is one line: a
//! multi-source key must not alias a single-source key, so the two builders
//! must agree on which inputs go into the context — and must re-sort `flags`
//! afterwards, because the key consumes the flag list in order.

use super::super::*;
use super::multi_restart_context_key::write_fake_multi_cc;
use super::CacheDirEnvGuard;
use crate::compiler::CompilerFamily;

fn env(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
    pairs
        .iter()
        .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
        .collect()
}

fn args(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| (*s).to_string()).collect()
}

fn compile_hash() -> ContentHash {
    crate::hash::hash_bytes(b"multi-source-context-salt-test")
}

/// The fixture cwd: absolute and spelled for the host platform, so the GNU and
/// MSVC arg parsers both see a real root (a POSIX `/multi-salts-work` literal is
/// not absolute on Windows, which would silently exercise a different branch).
fn fixture_cwd() -> crate::core::NormalizedPath {
    crate::core::NormalizedPath::new(Path::new(&zccache_test_support::from_root(
        "multi-salts-work",
    )))
}

/// Build the one shared multi-source context `handle_compile_multi` clones
/// into every unit of the batch, through the production builder itself.
fn multi_ctx(
    family: CompilerFamily,
    original_args: &[&str],
    client_env: &[(String, String)],
) -> Arc<CompileContext> {
    let argv = args(original_args);
    let (ctx, _dep_flags) = super::super::handle_compile_multi::build_multi_base_context(
        family,
        &argv,
        &fixture_cwd(),
        compile_hash(),
        &[],
        client_env,
        DependencyDiscoveryMode::AllHeaders,
    );
    ctx
}

/// `CPATH` is on the `#1806` GNU allowlist precisely because it "adds extra
/// include search dirs": a `#include` that resolves through `CPATH=/first`
/// must not share a cached object with the same source compiled under
/// `CPATH=/second`. Before the fix the multi-source builder dropped the salt,
/// so both landed on one context key — and therefore one `ArtifactKey`.
#[test]
fn multi_shared_base_context_salts_cpath() {
    let argv = &["-c", "a.c", "b.c"][..];

    let first = multi_ctx(CompilerFamily::Gcc, argv, &env(&[("CPATH", "/first")]));
    let second = multi_ctx(CompilerFamily::Gcc, argv, &env(&[("CPATH", "/second")]));
    let unset = multi_ctx(CompilerFamily::Gcc, argv, &[]);

    assert_ne!(
        first.flags, second.flags,
        "two multi-source `CPATH` values must not share a context key"
    );
    assert_ne!(
        unset.flags, first.flags,
        "an unset `CPATH` must not share a key with a set one"
    );
    assert_ne!(unset.flags, second.flags);

    // Pin the exact spelling `keys::cc_env_key_flags` produces, so a rename of
    // the salt prefix cannot silently keep both populations apart for the
    // wrong reason (or collapse them back together).
    assert!(
        first
            .flags
            .iter()
            .any(|flag| flag.as_str() == "zccache:env:CPATH=/first"),
        "first flags: {:?}",
        first.flags
    );
    assert!(
        second
            .flags
            .iter()
            .any(|flag| flag.as_str() == "zccache:env:CPATH=/second"),
        "second flags: {:?}",
        second.flags
    );
}

/// gcc localizes diagnostics and a cache hit replays the stored stderr
/// verbatim, so the effective message locale must split gcc's key. The
/// semantics asserted here mirror `keys_env_tests.rs` exactly (including the
/// neutral-locale case) so this test cannot drift from the single-source
/// expectation it is meant to agree with.
#[test]
fn multi_shared_base_context_salts_gcc_message_locale() {
    let argv = &["-c", "a.c", "b.c"][..];

    let de = multi_ctx(CompilerFamily::Gcc, argv, &env(&[("LANG", "de_DE.UTF-8")]));
    let fr = multi_ctx(CompilerFamily::Gcc, argv, &env(&[("LANG", "fr_FR.UTF-8")]));
    assert_ne!(
        de.flags, fr.flags,
        "two multi-source gcc message locales must not share a context key"
    );

    // `LC_ALL` outranks `LANG` (gettext precedence), so these are ONE locale.
    let all = multi_ctx(
        CompilerFamily::Gcc,
        argv,
        &env(&[("LC_ALL", "de_DE.UTF-8"), ("LANG", "fr_FR.UTF-8")]),
    );
    assert_eq!(de.flags, all.flags);

    // A neutral locale is english-equivalent to no locale at all: CI's
    // `C.UTF-8` must keep sharing artifacts with an unset environment. This is
    // the guard against a fix that salts every locale unconditionally.
    let neutral = multi_ctx(CompilerFamily::Gcc, argv, &env(&[("LANG", "C.UTF-8")]));
    let unset = multi_ctx(CompilerFamily::Gcc, argv, &[]);
    assert_eq!(
        neutral.flags, unset.flags,
        "a neutral locale must not split gcc's key"
    );

    // clang does not localize diagnostics, so locale must not split its key.
    let clang_de = multi_ctx(
        CompilerFamily::Clang,
        argv,
        &env(&[("LC_ALL", "de_DE.UTF-8")]),
    );
    let clang_unset = multi_ctx(CompilerFamily::Clang, argv, &[]);
    assert_eq!(
        clang_de.flags, clang_unset.flags,
        "clang does not localize, so locale must not salt its key"
    );
}

/// Issue #1530 on the multi-source path: `parse_msvc_args` consumes
/// `/showIncludes` without recording it, so without the salt `cl /c a.c b.c`
/// and `cl /showIncludes /c a.c b.c` share one entry — and their stored stdout
/// is not interchangeable (the daemon strips the note lines when it injects the
/// flag and keeps them when the caller passes it, so a replayed stripped entry
/// hands a CMake + Ninja caller an empty depfile).
#[test]
fn multi_shared_base_context_salts_msvc_show_includes() {
    let injected = multi_ctx(CompilerFamily::Msvc, &["/c", "a.c", "b.c"], &[]);
    let caller = multi_ctx(
        CompilerFamily::Msvc,
        &["/c", "/showIncludes", "a.c", "b.c"],
        &[],
    );
    let user_only = multi_ctx(
        CompilerFamily::Msvc,
        &["/c", "/showIncludes:user", "a.c", "b.c"],
        &[],
    );
    // Same flag, `-` spelling: the parser normalizes, so the salt must too.
    let dash_spelling = multi_ctx(
        CompilerFamily::Msvc,
        &["/c", "-showincludes", "a.c", "b.c"],
        &[],
    );

    assert_ne!(
        injected.flags, caller.flags,
        "a caller-passed `/showIncludes` must not share a key with an injected one"
    );
    assert_ne!(injected.flags, user_only.flags);
    assert_ne!(caller.flags, user_only.flags);

    assert!(
        caller
            .flags
            .iter()
            .any(|flag| flag.as_str() == "zccache:msvc-showincludes:all"),
        "caller flags: {:?}",
        caller.flags
    );
    assert!(
        user_only
            .flags
            .iter()
            .any(|flag| flag.as_str() == "zccache:msvc-showincludes:user"),
        "user-only flags: {:?}",
        user_only.flags
    );
    // Guards against a fix that keys on the RAW spelling and so fragments the
    // cache between two spellings of one flag.
    let caller_all = caller
        .flags
        .iter()
        .find(|flag| flag.as_str() == "zccache:msvc-showincludes:all")
        .expect("caller spelling must carry the `:all` salt");
    assert!(
        dash_spelling.flags.contains(caller_all),
        "`-showincludes` must produce the same salt as `/showIncludes`: {:?}",
        dash_spelling.flags
    );
}

/// `CompileContext` hashing consumes the flag list, so the order the salts
/// land in must not change the key. `from_parsed_args` sorts BEFORE the salts
/// are appended, so every fix has to re-sort — `rustc.rs` does, and so must the
/// multi-source builder.
///
/// The three salts cannot coexist in one context: `cc_env_key_flags` scopes the
/// env allowlist per family (GNU vars for gcc/clang, MSVC vars for `cl`) and
/// only gcc carries the locale salt, while `/showIncludes` is MSVC-only. So
/// assert sortedness on the densest real combination per family instead: two
/// GNU allowlist vars plus gcc's locale, and two MSVC vars plus the
/// `/showIncludes` salt.
#[test]
fn multi_shared_base_context_flags_are_sorted() {
    let gnu = multi_ctx(
        CompilerFamily::Gcc,
        &["-O2", "-c", "a.c", "b.c"],
        &env(&[
            ("CPATH", "/first"),
            ("SOURCE_DATE_EPOCH", "1700000000"),
            ("LANG", "de_DE.UTF-8"),
        ]),
    );
    let msvc = multi_ctx(
        CompilerFamily::Msvc,
        &["/c", "/showIncludes", "a.c", "b.c"],
        &env(&[("CL", "/W4"), ("INCLUDE", "C:\\sdk\\include")]),
    );

    for (label, ctx) in [("gnu", &gnu), ("msvc", &msvc)] {
        assert!(
            ctx.flags.windows(2).all(|w| w[0] <= w[1]),
            "{label} flags must stay sorted after the salts land: {:?}",
            ctx.flags
        );
        // Also assert the sort actually had work to do: the salts are present.
        assert!(
            ctx.flags.iter().any(|flag| flag.starts_with("zccache:")),
            "{label} fixture must actually carry salts, got {:?}",
            ctx.flags
        );
    }
}

/// The strongest statement of the invariant, and the one that keeps the two
/// paths from drifting apart again: for one fixed compile, the multi-source
/// builder and the single-source builder must produce IDENTICAL flag vectors.
/// Any input one of them salts and the other does not shows up here as a diff.
#[test]
fn multi_shared_base_context_matches_single_source_salts() {
    let tmp = tempfile::tempdir().unwrap();
    let compiler = tmp.path().join("cc");
    let source = tmp.path().join("a.c");
    let output = tmp.path().join("a.o");
    std::fs::write(&compiler, b"fake cc").unwrap();
    std::fs::write(&source, b"int a(void) { return 0; }").unwrap();

    let argv = args(&["-c", "a.c", "b.c"]);
    let client_env = env(&[("CPATH", "/first"), ("LANG", "de_DE.UTF-8")]);

    let compilation = crate::compiler::CacheableCompilation {
        compiler: compiler.into(),
        family: CompilerFamily::Gcc,
        source_file: source.into(),
        output_file: output.into(),
        original_args: std::sync::Arc::from(argv.clone()),
        unknown_flags: Vec::new(),
    };
    let compiler_hash_cache = CompilerHashCache::new();

    let cwd: crate::core::NormalizedPath = tmp.path().into();
    let (multi, _dep_flags) = super::super::handle_compile_multi::build_multi_base_context(
        CompilerFamily::Gcc,
        &argv,
        &cwd,
        compile_hash(),
        &[],
        &client_env,
        DependencyDiscoveryMode::AllHeaders,
    );
    let single = match super::super::rustc::build_compile_context(
        &compilation,
        tmp.path(),
        &[],
        &client_env,
        &compiler_hash_cache,
    ) {
        BuildContextResult::Cc { ctx, .. } => ctx,
        BuildContextResult::Rustc { .. } => panic!("expected a Cc context for a gcc compile"),
    };

    assert_eq!(
        multi.flags, single.flags,
        "the multi-source and single-source builders must agree on every key input"
    );
}

/// End-to-end RED test through the real multi-source compile path (two `.c`
/// operands in ONE invocation), which is why the single-source tests in
/// `keys_env_tests.rs` never caught this: they only ever built one-source
/// contexts.
///
/// Before the fix the second compile is served `CPATH=/first`'s objects as a
/// WRONG HIT. The third run repeats `CPATH=/second` to prove the salt does not
/// simply disable multi-source caching outright — a fix that makes every
/// multi-source compile miss forever is not a fix.
#[tokio::test]
// Holding the env-policy lock across the whole async test IS the point: it
// serializes the process-global staged-artifact policy for the test's full
// duration. Single-threaded test runtime, no lock-ordering hazard.
#[allow(clippy::await_holding_lock)]
async fn multi_source_cpath_change_is_a_miss() {
    let tmp = tempfile::tempdir().unwrap();
    // Explicit cache root.  Do not set ZCCACHE_CACHE_DIR: unrelated parallel
    // tests that call `DaemonServer::bind()` would then adopt this cache.
    let cache_root: crate::core::NormalizedPath = tmp.path().join("zccache-cache").into();
    let _env_lock = CacheDirEnvGuard::lock();

    let cc = write_fake_multi_cc(tmp.path());
    let work = tmp.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    let a = work.join("multi_a.c");
    let b = work.join("multi_b.c");
    std::fs::write(&a, "int multi_a(void) { return 2; }\n").unwrap();
    std::fs::write(&b, "int multi_b(void) { return 3; }\n").unwrap();

    let server =
        DaemonServer::bind_with_cache_dir(&crate::ipc::unique_test_endpoint(), &cache_root)
            .unwrap();
    let compile_args = vec![
        "-c".to_string(),
        a.to_string_lossy().into_owned(),
        b.to_string_lossy().into_owned(),
    ];

    assert!(
        !compile_multi_with_cpath(&server, &cc, &work, &compile_args, "/first").await,
        "the first multi-source compile is a cold miss"
    );

    // THE BUG: identical sources and argv, only `CPATH` differs, so the
    // header search path differs. Before the fix this was `cached: true` and
    // the caller was handed objects resolved against the other `CPATH`.
    assert!(
        !compile_multi_with_cpath(&server, &cc, &work, &compile_args, "/second").await,
        "a `CPATH` change must be a MISS on the multi-source path — a hit here \
         serves objects whose headers were resolved under a different search path"
    );

    assert!(
        compile_multi_with_cpath(&server, &cc, &work, &compile_args, "/second").await,
        "repeating `CPATH=/second` must hit, so the salt does not break \
         multi-source caching outright"
    );
}

/// Run the two-source compile under `cpath` and report whether the daemon
/// answered from cache.
///
/// The client env starts from the test process's own environment and appends
/// `CPATH` LAST, so the value under test always wins `keyed_cc_env`'s
/// last-wins rule even if the host already exports one — while the Windows
/// `.cmd` shim still gets the `SystemRoot`/`PATH` it needs to run.
async fn compile_multi_with_cpath(
    server: &DaemonServer,
    cc: &Path,
    work: &Path,
    compile_args: &[String],
    cpath: &str,
) -> bool {
    let mut client_env: Vec<(String, String)> = std::env::vars().collect();
    client_env.push(("CPATH".to_string(), cpath.to_string()));
    match handle_compile_ephemeral(
        &server.state,
        std::process::id(),
        work,
        cc,
        compile_args,
        work,
        Some(client_env),
        Vec::new(),
    )
    .await
    {
        Response::CompileResult {
            exit_code, cached, ..
        } => {
            assert_eq!(exit_code, 0, "CPATH={cpath} compile must succeed");
            cached
        }
        other => panic!("expected CompileResult for CPATH={cpath}, got {other:?}"),
    }
}
