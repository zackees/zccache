//! `clippy-driver` detection + caching (re-uses rustc parser).

use super::super::{detect_family, parse_invocation, CompilerFamily, ParsedInvocation};
use super::args;
use zccache_core::NormalizedPath;

#[test]
fn detect_clippy_driver_family() {
    assert_eq!(detect_family("clippy-driver"), CompilerFamily::Rustc);
    assert_eq!(
        detect_family(
            "/home/user/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin/clippy-driver"
        ),
        CompilerFamily::Rustc
    );
    assert_eq!(
        detect_family("C:\\Users\\user\\.rustup\\toolchains\\stable-x86_64-pc-windows-msvc\\bin\\clippy-driver.exe"),
        CompilerFamily::Rustc
    );
}

#[test]
fn detect_clippy_driver_versioned() {
    // Versioned clippy-driver (e.g., from rustup with custom toolchains)
    assert_eq!(detect_family("clippy-driver-1.78"), CompilerFamily::Rustc);
}

#[test]
fn clippy_driver_cacheable_lib() {
    // cargo clippy invokes: clippy-driver --crate-type lib --crate-name foo src/lib.rs ...
    let result = parse_invocation(
        "clippy-driver",
        &args(&[
            "--crate-name",
            "mycrate",
            "--crate-type",
            "lib",
            "--emit=metadata,dep-info",
            "--out-dir",
            "target/debug/deps",
            "-C",
            "extra-filename=-abc123",
            "src/lib.rs",
        ]),
    );
    match result {
        ParsedInvocation::Cacheable(c) => {
            assert_eq!(c.family, CompilerFamily::Rustc);
            assert_eq!(c.source_file, NormalizedPath::new("src/lib.rs"));
            // metadata-only emit → .rmeta extension
            assert!(c.output_file.to_str().unwrap().ends_with(".rmeta"));
        }
        other => panic!("expected cacheable, got: {other:?}"),
    }
}

#[test]
fn clippy_driver_bin_is_cacheable() {
    // bin became cacheable in iter7. clippy-driver follows the same
    // allowlist as rustc.
    let result = parse_invocation(
        "clippy-driver",
        &args(&[
            "--crate-name",
            "mybin",
            "--crate-type",
            "bin",
            "src/main.rs",
        ]),
    );
    assert!(matches!(result, ParsedInvocation::Cacheable(_)));
}

#[test]
fn clippy_driver_with_lint_flags() {
    // clippy-specific lint flags are standard rustc -W/-A/-D flags
    let result = parse_invocation(
        "clippy-driver",
        &args(&[
            "--crate-name",
            "mycrate",
            "--crate-type",
            "lib",
            "-W",
            "clippy::all",
            "-D",
            "clippy::unwrap_used",
            "-A",
            "clippy::too_many_arguments",
            "src/lib.rs",
        ]),
    );
    assert!(matches!(result, ParsedInvocation::Cacheable(_)));
}
