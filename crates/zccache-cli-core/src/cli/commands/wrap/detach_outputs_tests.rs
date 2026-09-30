//! Tests for [`super::detach_outputs`] (#1791).

use super::detach_outputs::*;
use std::path::{Path, PathBuf};

fn args(list: &[&str]) -> Vec<String> {
    list.iter().map(|arg| (*arg).to_owned()).collect()
}

/// A read-only hardlink of `blob` at `output`, as a LINK-mode hit leaves.
fn readonly_link(blob: &Path, output: &Path) {
    std::fs::write(blob, b"cached").unwrap();
    std::fs::hard_link(blob, output).unwrap();
    crate::platform::fs::permissions::set_readonly(output, true).unwrap();
    assert!(std::fs::metadata(output).unwrap().permissions().readonly());
}

#[test]
fn parses_cargo_style_invocation() {
    let parsed = parse(&args(&[
        "--crate-name",
        "probe",
        "--out-dir=/t/deps",
        "-C",
        "extra-filename=-abc",
        "-o",
        "x.o",
    ]));
    assert_eq!(parsed.out_dir, Some(PathBuf::from("/t/deps")));
    assert_eq!(parsed.crate_name.as_deref(), Some("probe"));
    assert_eq!(parsed.extra_filename.as_deref(), Some("-abc"));
    assert_eq!(parsed.output_file, Some(PathBuf::from("x.o")));
    let glued = parse(&args(&["-Cextra-filename=-z", "--out-dir", "d"]));
    assert_eq!(glued.extra_filename.as_deref(), Some("-z"));
    assert_eq!(glued.out_dir, Some(PathBuf::from("d")));
}

#[test]
fn detaches_only_this_crates_readonly_outputs_and_spares_the_blob() {
    let dir = tempfile::tempdir().unwrap();
    let deps = dir.path().join("deps");
    std::fs::create_dir(&deps).unwrap();
    let blob = dir.path().join("blob");
    let mine = deps.join("libprobe-abc.rmeta");
    readonly_link(&blob, &mine);
    // Another crate's read-only output and this crate's writable one.
    let other_blob = dir.path().join("other-blob");
    let other = deps.join("libother-abc.rmeta");
    readonly_link(&other_blob, &other);

    detach_readonly_rustc_outputs(&args(&[
        "--crate-name",
        "probe",
        "-C",
        "extra-filename=-abc",
        "--out-dir",
        deps.to_str().unwrap(),
    ]));

    assert!(
        !std::fs::metadata(&mine).unwrap().permissions().readonly(),
        "the invocation's output must be writable so rustc replaces it"
    );
    assert!(
        crate::platform::fs::identity::file_identity(&mine).unwrap()
            != crate::platform::fs::identity::file_identity(&blob).unwrap(),
        "the output must be an independent copy, not a link to the blob"
    );
    assert_eq!(std::fs::read(&mine).unwrap(), b"cached");
    assert_eq!(std::fs::read(&blob).unwrap(), b"cached");
    assert!(
        std::fs::metadata(&other).unwrap().permissions().readonly(),
        "another crate's output is not this invocation's to touch"
    );
    assert!(!deps.join(".zccache-detach-libprobe-abc.rmeta").exists());
}

#[test]
fn detaches_explicit_output_file() {
    let dir = tempfile::tempdir().unwrap();
    let blob = dir.path().join("blob");
    let out = dir.path().join("prog");
    readonly_link(&blob, &out);
    detach_readonly_rustc_outputs(&args(&["-o", out.to_str().unwrap()]));
    assert!(!std::fs::metadata(&out).unwrap().permissions().readonly());
    assert_eq!(std::fs::read(&out).unwrap(), b"cached");
}

#[test]
fn recognises_rustc_tool_names() {
    assert!(is_rustc(Path::new("/x/bin/rustc")));
    assert!(is_rustc(Path::new("rustc.exe")));
    assert!(!is_rustc(Path::new("clang")));
}
