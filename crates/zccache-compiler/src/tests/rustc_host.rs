//! Host facts belong to the native embedding, not a policy guest's target.
use super::args;
use crate::{parse_rustc_invocation_with_host, ParsedInvocation, RustcHost};

#[test]
fn lexical_path_syntax_is_independent_of_compiler_host() {
    use crate::rustc_path::RustcPathSyntax;
    assert_eq!(
        RustcPathSyntax::Windows.file_stem(r"C:\src\fixture.rs"),
        Some("fixture")
    );
    assert_eq!(
        RustcPathSyntax::Unix.file_stem(r"C:\src\fixture.rs"),
        Some(r"C:\src\fixture")
    );
    assert!(RustcPathSyntax::Windows.is_dylint_library_dir(Some(r"C:\dylint\libraries\out")));
    assert!(!RustcPathSyntax::Unix.is_dylint_library_dir(Some(r"C:\dylint\libraries\out")));
    assert!(RustcPathSyntax::Unix.is_dylint_linker(Some("/tools/dylint-link.exe")));
    assert!(!RustcPathSyntax::Unix.is_dylint_library_dir(Some("dylint/../libraries")));
}

#[test]
fn rustc_policy_preserves_output_plan_before_native_normalization() {
    use crate::parse_rustc::{parse_rustc_plan, RustcOutputPlan, RustcPlan};
    let arguments = [
        "source.rs",
        "--crate-type=rlib",
        "--crate-name=fixture",
        "--out-dir",
        "build/../out",
    ]
    .map(String::from);
    let RustcPlan::Cacheable {
        source,
        output,
        unknown_flags,
    } = parse_rustc_plan("rustc", &arguments, RustcHost::Linux, false)
    else {
        panic!("expected cacheable plan")
    };
    assert_eq!(source, "source.rs");
    assert!(unknown_flags.is_empty());
    assert_eq!(
        output,
        RustcOutputPlan::InDirectory {
            directory: "build/../out".into(),
            filename: "libfixture.rlib".into(),
        }
    );
}

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
fn rustc_output_plans_materialize_with_existing_native_path_semantics() {
    use crate::parse_rustc::{parse_rustc_plan, RustcOutputPlan, RustcPlan};
    use zccache_core::NormalizedPath;

    for host in [RustcHost::Linux, RustcHost::Macos, RustcHost::Windows] {
        for directory in ["build/../out", "", r"C:\build\..\out", "./out//nested"] {
            for explicit in [false, true] {
                let mut arguments = args(&[
                    "source.rs",
                    "--crate-type=rlib",
                    "--crate-name=fixture",
                    "--out-dir",
                    directory,
                    "--future-policy-flag",
                ]);
                if explicit {
                    arguments.extend(args(&["-o", "chosen/../exact.rlib"]));
                }
                let RustcPlan::Cacheable {
                    source,
                    output,
                    unknown_flags,
                } = parse_rustc_plan("rustc", &arguments, host, false)
                else {
                    panic!("expected cacheable plan")
                };
                let expected = if explicit {
                    assert_eq!(
                        output,
                        RustcOutputPlan::Explicit("chosen/../exact.rlib".into())
                    );
                    NormalizedPath::new("chosen/../exact.rlib")
                } else {
                    assert_eq!(
                        output,
                        RustcOutputPlan::InDirectory {
                            directory: directory.into(),
                            filename: "libfixture.rlib".into(),
                        }
                    );
                    // Preserve the pre-extraction normalization/join/normalization sequence.
                    NormalizedPath::new(
                        NormalizedPath::new(directory)
                            .join("libfixture.rlib")
                            .to_string_lossy()
                            .into_owned(),
                    )
                };
                let ParsedInvocation::Cacheable(native) =
                    parse_rustc_invocation_with_host("rustc", &arguments, host, false)
                else {
                    panic!("expected native cacheable result")
                };
                assert_eq!(native.output_file, expected);
                assert_eq!(native.source_file, NormalizedPath::new(source));
                assert_eq!(native.unknown_flags, unknown_flags);
                assert_eq!(native.original_args.as_ref(), arguments.as_slice());
            }
        }
    }
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
