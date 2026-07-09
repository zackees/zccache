//! Tests for `derive_crate_name`, `derive_crate_type`, `derive_output_ext`.

use super::super::{derive_crate_name, derive_crate_type, derive_output_ext};

#[test]
fn derive_crate_name_spaced_form() {
    let args = vec![
        "--edition".to_string(),
        "2021".to_string(),
        "--crate-name".to_string(),
        "foo".to_string(),
        "src/lib.rs".to_string(),
    ];
    assert_eq!(derive_crate_name(&args), Some("foo".to_string()));
}

#[test]
fn derive_crate_name_equals_form() {
    let args = vec!["--crate-name=bar_baz".to_string(), "src/lib.rs".to_string()];
    assert_eq!(derive_crate_name(&args), Some("bar_baz".to_string()));
}

#[test]
fn derive_crate_name_missing_returns_none() {
    let args = vec!["-c".to_string(), "foo.cpp".to_string()];
    assert_eq!(derive_crate_name(&args), None);
}

#[test]
fn derive_crate_name_dangling_flag_returns_none() {
    // `--crate-name` with no following value must not panic / out-of-bounds.
    let args = vec!["--crate-name".to_string()];
    assert_eq!(derive_crate_name(&args), None);
}

#[test]
fn derive_crate_type_lib() {
    let args = vec![
        "--crate-name".to_string(),
        "foo".to_string(),
        "--crate-type".to_string(),
        "lib".to_string(),
    ];
    assert_eq!(derive_crate_type(&args), Some("lib"));
}

#[test]
fn derive_crate_type_bin() {
    let args = vec!["--crate-type".to_string(), "bin".to_string()];
    assert_eq!(derive_crate_type(&args), Some("bin"));
}

#[test]
fn derive_crate_type_proc_macro_normalizes_to_hyphen() {
    // rustc accepts `proc-macro` (canonical). Confirm it stays canonical
    // (no underscore variant emitted).
    let args = vec!["--crate-type=proc-macro".to_string()];
    assert_eq!(derive_crate_type(&args), Some("proc-macro"));
}

#[test]
fn derive_crate_type_build_script_via_crate_name() {
    // Cargo invokes build.rs as `--crate-name build_script_build`.
    // That overrides the literal crate-type (which would be "bin").
    let args = vec![
        "--crate-name".to_string(),
        "build_script_build".to_string(),
        "--crate-type".to_string(),
        "bin".to_string(),
    ];
    assert_eq!(derive_crate_type(&args), Some("build-script"));
}

#[test]
fn derive_crate_type_missing_returns_none() {
    let args = vec!["-c".to_string(), "foo.cpp".to_string()];
    assert_eq!(derive_crate_type(&args), None);
}

#[test]
fn derive_crate_type_unknown_value_returns_none() {
    // An unrecognized crate-type should be dropped, not propagated raw.
    let args = vec!["--crate-type".to_string(), "weirdo".to_string()];
    assert_eq!(derive_crate_type(&args), None);
}

#[test]
fn derive_output_ext_for_each_crate_type() {
    // The full table per the issue: crate_type → output_ext.
    assert_eq!(derive_output_ext(Some("lib")), Some("rlib"));
    assert_eq!(derive_output_ext(Some("bin")), Some("exe"));
    assert_eq!(derive_output_ext(Some("proc-macro")), Some("so"));
    assert_eq!(derive_output_ext(Some("build-script")), Some("exe"));
    assert_eq!(derive_output_ext(Some("test")), Some("exe"));
    assert_eq!(derive_output_ext(Some("bench")), Some("exe"));
    assert_eq!(derive_output_ext(Some("example")), Some("exe"));
    assert_eq!(derive_output_ext(None), None);
}

#[test]
fn derive_output_ext_unknown_returns_none() {
    assert_eq!(derive_output_ext(Some("nonsense")), None);
}
