//! Tests for rustc dep-info parsing (zccache#1021): `# env-dep:` lines
//! must be collected as env-dep names, comment lines must never be
//! treated as dependency rules, and file-dep extraction is unchanged.

use std::path::Path;

use super::super::rustc::parse_rustc_depinfo;

#[test]
fn env_dep_lines_are_collected_as_names() {
    let content = "\
target/debug/deps/libenvdep.rlib: src/lib.rs src/gen.rs

src/lib.rs:
src/gen.rs:

# env-dep:VERGEN_GIT_SHA=abc123
# env-dep:MAYBE_UNSET
# env-dep:CARGO_PKG_VERSION=1.0.0
";
    let result = parse_rustc_depinfo(content, Path::new("src/lib.rs"), Path::new("/nonexistent"));

    assert_eq!(
        result.env_dep_names,
        vec![
            "CARGO_PKG_VERSION".to_string(),
            "MAYBE_UNSET".to_string(),
            "VERGEN_GIT_SHA".to_string(),
        ],
        "env-dep NAMES are extracted (sorted, deduped); values come from \
         the request env at key time",
    );
}

#[test]
fn env_dep_lines_are_not_treated_as_file_deps() {
    // Pre-#1021 the `# env-dep:NAME=value` line fell through the rule
    // parser: `NAME=value` became a dep token and was only dropped by the
    // exists() filter. Assert comment lines never contribute deps even
    // when a matching file exists on disk.
    let dir = tempfile::tempdir().expect("tempdir");
    let trap = dir.path().join("TRAP=1");
    std::fs::write(&trap, b"x").expect("write trap file");

    let content = "# env-dep:TRAP=1\n";
    let result = parse_rustc_depinfo(content, Path::new("src/lib.rs"), dir.path());

    assert!(
        result.scan.resolved.is_empty(),
        "comment lines must never resolve to file deps: {:?}",
        result.scan.resolved,
    );
    assert_eq!(result.env_dep_names, vec!["TRAP".to_string()]);
}

#[test]
fn env_dep_names_are_deduplicated() {
    let content = "\
# env-dep:STAMP=a
# env-dep:STAMP=a
";
    let result = parse_rustc_depinfo(content, Path::new("src/lib.rs"), Path::new("/nonexistent"));
    assert_eq!(result.env_dep_names, vec!["STAMP".to_string()]);
}

#[test]
fn file_dep_extraction_is_unchanged() {
    let dir = tempfile::tempdir().expect("tempdir");
    let dep = dir.path().join("gen.rs");
    std::fs::write(&dep, b"pub fn g() {}").expect("write dep");

    let content = format!(
        "target/libx.rlib: src/lib.rs {}\n\n# env-dep:STAMP=v\n",
        dep.display()
    );
    let result = parse_rustc_depinfo(&content, Path::new("src/lib.rs"), dir.path());

    assert_eq!(result.scan.resolved.len(), 1, "one existing file dep");
    assert_eq!(result.env_dep_names, vec!["STAMP".to_string()]);
}
