//! Small standalone-util tests — currently just `flag_truthy`. Add new
//! `super::super::util::*` pure-fn tests here as they appear.

use super::super::util::flag_truthy;

/// Exercises both branches of the setup-soldr-compatible bool grammar.
/// Tests the pure function so we don't have to mutate process env vars
/// — that's a documented foot-gun in cargo's parallel test runner.
#[test]
fn flag_truthy_matches_setup_soldr_normalization() {
    // Truthy variants
    for v in ["1", "true", "True", "TRUE", "yes", "YES", "on", "On"] {
        assert!(flag_truthy(Some(v)), "expected truthy: {v:?}");
    }
    // Whitespace tolerated
    assert!(flag_truthy(Some("  true  ")));

    // Falsy / "leave behavior unchanged" variants
    assert!(!flag_truthy(None));
    for v in [
        "", "0", "false", "False", "no", "off", "OFF", "garbage", "2",
    ] {
        assert!(!flag_truthy(Some(v)), "expected falsy: {v:?}");
    }
}
