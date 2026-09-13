//! Host facts belong to the native embedding, not a policy guest's target.
use super::args;
use crate::{parse_rustc_invocation_with_host, ParsedInvocation, RustcHost};

fn output(host: RustcHost, flags: &[&str]) -> String {
    let arguments = args(flags);
    let ParsedInvocation::Cacheable(parsed) =
        parse_rustc_invocation_with_host("rustc", &arguments, host, false)
    else {
        panic!("expected cacheable fixture on {host:?}");
    };
    assert_eq!(parsed.original_args.as_ref(), arguments.as_slice());
    parsed.output_file.to_string_lossy().into_owned()
}

#[test]
fn explicit_host_proc_macro_naming_ignores_requested_target() {
    for (host, expected) in [
        (RustcHost::Linux, "libfixture.so"),
        (RustcHost::Macos, "libfixture.dylib"),
        (RustcHost::Windows, "fixture.dll"),
    ] {
        assert_eq!(
            output(
                host,
                &[
                    "--crate-type",
                    "proc-macro",
                    "--crate-name",
                    "fixture",
                    "fixture.rs",
                    "--target",
                    "wasm32-unknown-unknown"
                ]
            ),
            expected,
        );
    }
}

#[test]
fn explicit_host_bin_naming_yields_to_requested_target() {
    for host in [RustcHost::Linux, RustcHost::Macos, RustcHost::Windows] {
        let base = [
            "--crate-type",
            "bin",
            "--crate-name",
            "fixture",
            "fixture.rs",
        ];
        assert_eq!(
            output(host, &base),
            if host == RustcHost::Windows {
                "fixture.exe"
            } else {
                "fixture"
            }
        );
        for (target, expected) in [
            ("x86_64-pc-windows-msvc", "fixture.exe"),
            ("wasm32-unknown-unknown", "fixture"),
        ] {
            let mut flags = base.to_vec();
            flags.extend(["--target", target]);
            assert_eq!(output(host, &flags), expected);
        }
    }
}

#[test]
fn explicit_host_dylint_policy_and_test_opt_in_preserve_native_decisions() {
    let dylint = args(&[
        "--crate-type",
        "cdylib",
        "--crate-name",
        "fixture",
        "fixture.rs",
        "--out-dir",
        "target/dylint/libraries",
        "-C",
        "linker=dylint-link",
    ]);
    for host in [RustcHost::Linux, RustcHost::Macos, RustcHost::Windows] {
        let parsed = parse_rustc_invocation_with_host("rustc", &dylint, host, false);
        assert_eq!(
            matches!(parsed, ParsedInvocation::Cacheable(_)),
            host != RustcHost::Windows
        );
        let nested = args(&["rustc", "--crate-name", "fixture", "fixture.rs", "--test"]);
        assert!(matches!(
            parse_rustc_invocation_with_host("dylint-driver", &nested, host, false),
            ParsedInvocation::NonCacheable { .. }
        ));
        let ParsedInvocation::Cacheable(parsed) =
            parse_rustc_invocation_with_host("dylint-driver", &nested, host, true)
        else {
            panic!("explicit test opt-in");
        };
        assert_eq!(parsed.original_args.as_ref(), nested.as_slice());
    }
}
