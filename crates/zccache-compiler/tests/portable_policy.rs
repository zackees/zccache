use zccache_compiler::{
    detect_family, parse_rustc_plan_with_syntax, CompilerFamily, RustcHost, RustcOutputPlan,
    RustcPathSyntax, RustcPlan,
};

fn plan(host: RustcHost, syntax: RustcPathSyntax, args: &[&str], test_cache: bool) -> RustcPlan {
    let args: Vec<_> = args.iter().map(|arg| (*arg).to_owned()).collect();
    parse_rustc_plan_with_syntax("rustc", &args, host, test_cache, syntax)
}

fn output(plan: RustcPlan) -> RustcOutputPlan {
    match plan {
        RustcPlan::Cacheable { output, .. } => output,
        RustcPlan::NonCacheable { reason } => panic!("unexpected rejection: {reason}"),
    }
}

#[test]
fn portable_host_names_and_requested_target_are_distinct() {
    for (host, expected) in [
        (RustcHost::Linux, "libfixture.so"),
        (RustcHost::Macos, "libfixture.dylib"),
        (RustcHost::Windows, "fixture.dll"),
    ] {
        assert_eq!(
            output(plan(
                host,
                RustcPathSyntax::Unix,
                &[
                    "fixture.rs",
                    "--crate-type=proc-macro",
                    "--target=wasm32-unknown-unknown",
                ],
                false
            )),
            RustcOutputPlan::Explicit(expected.into())
        );
        assert_eq!(
            output(plan(
                host,
                RustcPathSyntax::Unix,
                &[
                    "fixture.rs",
                    "--crate-type=bin",
                    "--target=x86_64-pc-windows-msvc",
                ],
                false
            )),
            RustcOutputPlan::Explicit("fixture.exe".into())
        );
    }
}

#[test]
fn portable_path_syntax_is_not_inferred_from_host() {
    for host in [RustcHost::Linux, RustcHost::Macos, RustcHost::Windows] {
        for (syntax, expected) in [
            (RustcPathSyntax::Unix, r"libC:\src\fixture.rlib"),
            (RustcPathSyntax::Windows, "libfixture.rlib"),
        ] {
            assert_eq!(
                output(plan(
                    host,
                    syntax,
                    &[r"C:\src\fixture.rs", "--crate-type=rlib"],
                    false
                )),
                RustcOutputPlan::Explicit(expected.into())
            );
        }
    }
}

#[test]
fn portable_detection_and_test_cache_opt_in() {
    assert_eq!(
        detect_family(r"C:\toolchain\rustc.exe"),
        CompilerFamily::Rustc
    );
    let args = ["fixture.rs", "--test"];
    assert!(matches!(
        plan(RustcHost::Linux, RustcPathSyntax::Unix, &args, false),
        RustcPlan::NonCacheable { .. }
    ));
    assert!(matches!(
        plan(RustcHost::Linux, RustcPathSyntax::Unix, &args, true),
        RustcPlan::Cacheable { .. }
    ));
}
