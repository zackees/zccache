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

/// zccache#1937: a `staticlib` is named by the *target's* archive convention,
/// not the Unix `lib<name>.a`. MSVC-like targets (`*-windows-msvc`, UEFI)
/// write `<name><extra>.lib` with no `lib` prefix; everything else, including
/// `*-windows-gnu`, writes `lib<name><extra>.a`. With no `--target` the
/// compile is host-native, and a Windows host is taken to be MSVC (rustup's
/// default Windows toolchain).
#[test]
fn staticlib_filename_follows_target_archive_convention() {
    let staticlib = |host: RustcHost, target: Option<&str>| {
        let mut args = vec![
            "src/lib.rs".to_owned(),
            "--crate-name".to_owned(),
            "libobs_rust".to_owned(),
            "--crate-type".to_owned(),
            "staticlib".to_owned(),
            "--out-dir".to_owned(),
            "deps".to_owned(),
            "-C".to_owned(),
            "extra-filename=-d170041734f5a224".to_owned(),
        ];
        if let Some(triple) = target {
            args.push(format!("--target={triple}"));
        }
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        match output(plan(host, RustcPathSyntax::Unix, &args, false)) {
            RustcOutputPlan::InDirectory { filename, .. } => filename,
            other => panic!("unexpected output plan: {other:?}"),
        }
    };

    let msvc = "libobs_rust-d170041734f5a224.lib";
    let unix = "liblibobs_rust-d170041734f5a224.a";
    for host in [RustcHost::Linux, RustcHost::Macos, RustcHost::Windows] {
        assert_eq!(staticlib(host, Some("x86_64-pc-windows-msvc")), msvc);
        assert_eq!(staticlib(host, Some("aarch64-pc-windows-msvc")), msvc);
        assert_eq!(staticlib(host, Some("i686-win7-windows-msvc")), msvc);
        assert_eq!(staticlib(host, Some("x86_64-unknown-uefi")), msvc);
        assert_eq!(staticlib(host, Some("x86_64-pc-windows-gnu")), unix);
        assert_eq!(staticlib(host, Some("x86_64-pc-windows-gnullvm")), unix);
        assert_eq!(staticlib(host, Some("x86_64-unknown-linux-gnu")), unix);
        assert_eq!(staticlib(host, Some("aarch64-apple-darwin")), unix);
        assert_eq!(staticlib(host, Some("wasm32-unknown-unknown")), unix);
    }
    assert_eq!(staticlib(RustcHost::Windows, None), msvc);
    assert_eq!(staticlib(RustcHost::Linux, None), unix);
    assert_eq!(staticlib(RustcHost::Macos, None), unix);
}
