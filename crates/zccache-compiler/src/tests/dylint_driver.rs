//! Dylint's nested driver adapter.

use super::super::{
    detect_family, parse_invocation, prepare_dylint_cache_env, CompilerFamily, ParsedInvocation,
    DYLINT_CACHE_INPUT_HASH_ENV, DYLINT_LIBS_ENV,
};
use zccache_core::NormalizedPath;

fn args(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| (*value).to_string()).collect()
}

#[test]
fn detects_only_the_explicit_dylint_driver_as_rust() {
    assert_eq!(detect_family("dylint-driver"), CompilerFamily::Rustc);
    assert_eq!(
        detect_family("/tmp/dylint/driver/dylint-driver"),
        CompilerFamily::Rustc
    );
    assert_eq!(
        detect_family(r"C:\dylint\driver\dylint-driver.exe"),
        CompilerFamily::Rustc
    );
    assert_ne!(detect_family("arbitrary-driver"), CompilerFamily::Rustc);
}

#[test]
fn parses_inner_rustc_args_but_preserves_nested_execution_argv() {
    let nested = args(&[
        "/toolchains/nightly/bin/rustc",
        "--crate-name",
        "violating_fixture",
        "--crate-type",
        "lib",
        "--emit",
        "metadata",
        "--out-dir",
        "target",
        "src/lib.rs",
    ]);

    match parse_invocation("/tmp/dylint-driver", &nested) {
        ParsedInvocation::Cacheable(compilation) => {
            assert_eq!(compilation.family, CompilerFamily::Rustc);
            assert_eq!(compilation.source_file, NormalizedPath::new("src/lib.rs"));
            assert_eq!(
                compilation.original_args.as_ref(),
                nested.as_slice(),
                "the daemon must execute the driver with the inner rustc path intact"
            );
        }
        other => panic!("expected cacheable nested Dylint invocation, got {other:?}"),
    }
}

#[test]
fn rejects_dylint_driver_without_an_inner_rustc() {
    let invocation = parse_invocation(
        "dylint-driver",
        &args(&["--crate-name", "fixture", "src/lib.rs"]),
    );
    assert!(matches!(
        invocation,
        ParsedInvocation::NonCacheable { ref reason }
            if reason.contains("inner rustc")
    ));
}

fn cache_input_hash(env: &[(String, String)]) -> &str {
    env.iter()
        .find_map(|(key, value)| (key == DYLINT_CACHE_INPUT_HASH_ENV).then_some(value.as_str()))
        .expect("prepared Dylint env must contain a content hash")
}

#[test]
fn library_content_and_output_environment_invalidate_the_input_hash() {
    let temp = tempfile::tempdir().unwrap();
    let driver = temp.path().join("dylint-driver");
    let rustc = temp.path().join("rustc");
    let library = temp.path().join("libfixture.so");
    std::fs::write(&driver, b"driver").unwrap();
    std::fs::write(&rustc, b"rustc").unwrap();
    std::fs::write(&library, b"lint-v1").unwrap();
    let args = vec![
        rustc.to_string_lossy().into_owned(),
        "--crate-name".into(),
        "fixture".into(),
        "--crate-type".into(),
        "lib".into(),
        "src/lib.rs".into(),
    ];
    let libs = serde_json::to_string(std::slice::from_ref(&library)).unwrap();
    let make_env =
        |metadata: &str, no_deps: &str, rustup_home: &str, toolchain: &str, docs_links: &str| {
            vec![
                (DYLINT_LIBS_ENV.into(), libs.clone()),
                ("DYLINT_METADATA".into(), metadata.into()),
                ("DYLINT_NO_DEPS".into(), no_deps.into()),
                ("RUSTUP_HOME".into(), rustup_home.into()),
                ("RUSTUP_TOOLCHAIN".into(), toolchain.into()),
                ("CLIPPY_DISABLE_DOCS_LINKS".into(), docs_links.into()),
            ]
        };

    let mut baseline = make_env(
        r#"{"mode":"one"}"#,
        "0",
        "/rustup/fixture",
        "nightly-fixture",
        "0",
    );
    prepare_dylint_cache_env(
        &NormalizedPath::new(&driver),
        &args,
        temp.path(),
        &mut baseline,
    )
    .unwrap();
    let baseline_hash = cache_input_hash(&baseline).to_string();

    std::fs::write(&driver, b"driver-v2").unwrap();
    let mut changed_driver = make_env(
        r#"{"mode":"one"}"#,
        "0",
        "/rustup/fixture",
        "nightly-fixture",
        "0",
    );
    prepare_dylint_cache_env(
        &NormalizedPath::new(&driver),
        &args,
        temp.path(),
        &mut changed_driver,
    )
    .unwrap();
    assert_ne!(baseline_hash, cache_input_hash(&changed_driver));

    std::fs::write(&driver, b"driver").unwrap();
    std::fs::write(&rustc, b"rustc-v2").unwrap();
    let mut changed_rustc = make_env(
        r#"{"mode":"one"}"#,
        "0",
        "/rustup/fixture",
        "nightly-fixture",
        "0",
    );
    prepare_dylint_cache_env(
        &NormalizedPath::new(&driver),
        &args,
        temp.path(),
        &mut changed_rustc,
    )
    .unwrap();
    assert_ne!(baseline_hash, cache_input_hash(&changed_rustc));

    std::fs::write(&rustc, b"rustc").unwrap();
    std::fs::write(&library, b"lint-v2").unwrap();
    let mut changed_library = make_env(
        r#"{"mode":"one"}"#,
        "0",
        "/rustup/fixture",
        "nightly-fixture",
        "0",
    );
    prepare_dylint_cache_env(
        &NormalizedPath::new(&driver),
        &args,
        temp.path(),
        &mut changed_library,
    )
    .unwrap();
    assert_ne!(baseline_hash, cache_input_hash(&changed_library));

    std::fs::write(&library, b"lint-v1").unwrap();
    let renamed_library = temp.path().join("librenamed_fixture.so");
    std::fs::write(&renamed_library, b"lint-v1").unwrap();
    let mut changed_library_name = make_env(
        r#"{"mode":"one"}"#,
        "0",
        "/rustup/fixture",
        "nightly-fixture",
        "0",
    );
    let renamed_libs = serde_json::to_string(&[renamed_library]).unwrap();
    changed_library_name
        .iter_mut()
        .find(|(name, _)| name == DYLINT_LIBS_ENV)
        .unwrap()
        .1 = renamed_libs;
    prepare_dylint_cache_env(
        &NormalizedPath::new(&driver),
        &args,
        temp.path(),
        &mut changed_library_name,
    )
    .unwrap();
    assert_ne!(
        baseline_hash,
        cache_input_hash(&changed_library_name),
        "the Dylint library name controls the injected dylint_lib cfg"
    );

    for (label, mut changed_environment) in [
        (
            "DYLINT_METADATA",
            make_env(
                r#"{"mode":"two"}"#,
                "0",
                "/rustup/fixture",
                "nightly-fixture",
                "0",
            ),
        ),
        (
            "DYLINT_NO_DEPS",
            make_env(
                r#"{"mode":"one"}"#,
                "1",
                "/rustup/fixture",
                "nightly-fixture",
                "0",
            ),
        ),
        (
            "RUSTUP_HOME",
            make_env(
                r#"{"mode":"one"}"#,
                "0",
                "/rustup/other",
                "nightly-fixture",
                "0",
            ),
        ),
        (
            "RUSTUP_TOOLCHAIN",
            make_env(
                r#"{"mode":"one"}"#,
                "0",
                "/rustup/fixture",
                "nightly-fixture-2",
                "0",
            ),
        ),
        (
            "CLIPPY_DISABLE_DOCS_LINKS",
            make_env(
                r#"{"mode":"one"}"#,
                "0",
                "/rustup/fixture",
                "nightly-fixture",
                "1",
            ),
        ),
    ] {
        prepare_dylint_cache_env(
            &NormalizedPath::new(&driver),
            &args,
            temp.path(),
            &mut changed_environment,
        )
        .unwrap();
        assert_ne!(
            baseline_hash,
            cache_input_hash(&changed_environment),
            "{label} must invalidate the Dylint identity"
        );
    }
}

#[test]
fn library_state_fails_open_when_missing_malformed_or_unhashable() {
    let temp = tempfile::tempdir().unwrap();
    let driver = NormalizedPath::new(temp.path().join("dylint-driver"));
    let rustc = temp.path().join("rustc");
    std::fs::write(&driver, b"driver").unwrap();
    std::fs::write(&rustc, b"rustc").unwrap();
    let args = vec![rustc.to_string_lossy().into_owned(), "src/lib.rs".into()];

    let mut missing_env = Vec::new();
    let missing =
        prepare_dylint_cache_env(&driver, &args, temp.path(), &mut missing_env).unwrap_err();
    assert!(missing.contains(DYLINT_LIBS_ENV));

    let mut malformed_env = vec![(DYLINT_LIBS_ENV.into(), "not-json".into())];
    let malformed =
        prepare_dylint_cache_env(&driver, &args, temp.path(), &mut malformed_env).unwrap_err();
    assert!(malformed.contains("JSON"));

    let absent_path = temp.path().join("missing.so");
    let mut unhashable = vec![(
        DYLINT_LIBS_ENV.into(),
        serde_json::to_string(&[absent_path]).unwrap(),
    )];
    let error = prepare_dylint_cache_env(&driver, &args, temp.path(), &mut unhashable).unwrap_err();
    assert!(error.contains("uncached"));
    assert!(error.contains("missing.so"));
}
