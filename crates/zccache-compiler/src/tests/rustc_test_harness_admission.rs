//! Request-scoped `--test` harness admission through the public parse entry
//! (zccache#1550): the policy is an argument, never the process environment.

use super::super::{parse_invocation_with_admission, ParsedInvocation};
use super::args;
use zccache_core::config::TestHarnessAdmission;

/// Cargo's shape for a unit-test harness: `--test`, no `--crate-type`.
fn cargo_harness_args() -> Vec<String> {
    args(&[
        "--crate-name",
        "zccache_core",
        "--edition=2021",
        "src/lib.rs",
        "--emit=dep-info,link",
        "--test",
        "-C",
        "debuginfo=2",
        "-C",
        "metadata=0123456789abcdef",
        "-C",
        "extra-filename=-0123456789abcdef",
        "--out-dir",
        "/w/target/debug/deps",
    ])
}

#[test]
fn shared_only_refuses_the_harness_and_classifies_it() {
    let parsed = parse_invocation_with_admission(
        "rustc",
        &cargo_harness_args(),
        TestHarnessAdmission::SharedOnly,
    );
    assert!(
        matches!(parsed, ParsedInvocation::NonCacheable { .. }),
        "shared-only must refuse the harness: {parsed:?}"
    );
    assert!(parsed.is_rustc_test_harness());
}

#[test]
fn all_admits_the_harness_with_test_in_the_key_material() {
    let parsed =
        parse_invocation_with_admission("rustc", &cargo_harness_args(), TestHarnessAdmission::All);
    let ParsedInvocation::Cacheable(compilation) = &parsed else {
        panic!("`all` must admit the harness: {parsed:?}");
    };
    assert!(compilation.is_rustc_test_harness());
    assert!(parsed.is_rustc_test_harness());
}

#[test]
fn ordinary_library_units_are_cacheable_under_either_policy() {
    let lib = args(&[
        "--crate-name",
        "zccache_core",
        "--edition=2021",
        "src/lib.rs",
        "--crate-type",
        "lib",
        "--emit=dep-info,metadata,link",
        "--out-dir",
        "/w/target/debug/deps",
    ]);
    for admission in [TestHarnessAdmission::SharedOnly, TestHarnessAdmission::All] {
        let parsed = parse_invocation_with_admission("rustc", &lib, admission);
        assert!(
            matches!(parsed, ParsedInvocation::Cacheable(_)),
            "{admission:?}: {parsed:?}"
        );
        assert!(!parsed.is_rustc_test_harness());
    }
}

#[test]
fn non_harness_refusals_are_not_classified_as_harnesses() {
    let cdylib = args(&[
        "--crate-name",
        "plugin",
        "src/lib.rs",
        "--crate-type",
        "cdylib",
        "--out-dir",
        "/w/target/debug/deps",
    ]);
    let parsed = parse_invocation_with_admission("rustc", &cdylib, TestHarnessAdmission::All);
    assert!(matches!(parsed, ParsedInvocation::NonCacheable { .. }));
    assert!(!parsed.is_rustc_test_harness());
}
