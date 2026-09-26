//! #1649: which argv shapes are compiler-wrapper invocations.

use super::super::is_compiler_wrapper_invocation;

fn argv(args: &[&str]) -> Vec<String> {
    args.iter().map(ToString::to_string).collect()
}

#[test]
fn compiler_invocations_are_wrapper_invocations() {
    for args in [
        &["zccache", "clang++", "-c", "a.cpp"][..],
        &["zccache", "/usr/bin/rustc", "-vV"],
        &["zccache", "cc", "-c", "a.c"],
        &["zccache", "c++", "-c", "a.cpp"],
    ] {
        assert!(is_compiler_wrapper_invocation(&argv(args)), "{args:?}");
    }
}

#[test]
fn subcommands_and_flags_are_not_wrapper_invocations() {
    for args in [
        &["zccache"][..],
        &["zccache", "status"],
        &["zccache", "stop"],
        &["zccache", "--version"],
        &["zccache", "--help"],
    ] {
        assert!(!is_compiler_wrapper_invocation(&argv(args)), "{args:?}");
    }
}
