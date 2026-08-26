//! Public facade coverage for the daemon-free formatter feature.
//!
//! Run with `soldr cargo test -p zccache --no-default-features --features formatter
//! --test formatter_api`.

#![cfg(feature = "formatter")]

#[test]
fn formatter_feature_exposes_runner_api_without_cli_surface() {
    let root = tempfile::tempdir().unwrap();
    let rustfmt = root.path().join("rustfmt-test-bin");
    let source = root.path().join("input.rs");
    std::fs::write(&rustfmt, b"fake formatter identity").unwrap();
    std::fs::write(&source, b"fn main( ) {}\n").unwrap();
    let args = vec![source.display().to_string()];
    let mut called = false;

    let code = zccache::formatter::run_rustfmt_cached_with_runner(
        &rustfmt,
        &args,
        root.path(),
        Some(&root.path().join("cache")),
        |command| {
            called = true;
            assert_eq!(command.get_program(), rustfmt.as_os_str());
            assert_eq!(command.get_current_dir(), Some(root.path()));
            Ok(23)
        },
    )
    .unwrap();

    assert!(called);
    assert_eq!(code, 23);
}
