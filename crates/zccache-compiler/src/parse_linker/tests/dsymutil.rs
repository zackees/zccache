use super::*;
use crate::parse_linker::{
    parse_linker_invocation, LinkOutputKind, LinkerFamily, ParsedLinkerInvocation,
};
use zccache_core::NormalizedPath;

#[test]
fn dsymutil_declares_default_directory_bundle() {
    let parsed = parse_linker_invocation("dsymutil", args(&["bin/app"]));
    let ParsedLinkerInvocation::Cacheable(link) = parsed else {
        panic!("expected cacheable dsymutil invocation");
    };
    assert_eq!(link.family, LinkerFamily::Dsymutil);
    assert_eq!(link.output_kind, LinkOutputKind::DirectoryBundle);
    assert_eq!(link.output_file, NormalizedPath::new("bin/app.dSYM"));
    assert_eq!(link.input_files, vec![NormalizedPath::new("bin/app")]);
}

#[test]
fn dsymutil_declares_explicit_output_directory() {
    let parsed = parse_linker_invocation(
        "/usr/bin/dsymutil",
        args(&["--arch", "arm64", "-o", "symbols/app.dSYM", "bin/app"]),
    );
    let ParsedLinkerInvocation::Cacheable(link) = parsed else {
        panic!("expected cacheable dsymutil invocation");
    };
    assert_eq!(link.output_file, NormalizedPath::new("symbols/app.dSYM"));
    assert_eq!(link.cache_relevant_flags, args(&["--arch", "arm64"]));
}

#[test]
fn dsymutil_opaque_and_signed_modes_fall_back() {
    for mode in ["--codesign", "--embed-resource", "--update", "--flat"] {
        assert!(matches!(
            parse_linker_invocation("dsymutil", args(&[mode, "bin/app"])),
            ParsedLinkerInvocation::NonCacheable { .. }
        ));
    }
}

#[test]
fn dsymutil_requires_one_input_and_complete_options() {
    for invocation in [args(&[]), args(&["a", "b"]), args(&["--arch"])] {
        assert!(matches!(
            parse_linker_invocation("dsymutil", invocation),
            ParsedLinkerInvocation::NonCacheable { .. }
        ));
    }
}
