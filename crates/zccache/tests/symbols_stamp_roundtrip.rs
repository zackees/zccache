//! Spawn `zccache-stamp` against a fixture binary then assert the
//! reader sees exactly the fields we wrote. This is the contract the
//! CI workflow relies on — produced by the stamp binary, consumed by
//! `read_marker_from_path` in the daemon and CLI.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::panic_in_result_fn, clippy::unwrap_in_result)]

use std::io::Write;
use zccache::symbols::marker::{read_marker_from_path, MARKER_SIZE};

#[test]
fn stamp_then_read_returns_same_fields() {
    let mut tmp = tempfile::NamedTempFile::new().expect("tempfile");
    tmp.write_all(b"pretend this is a real release binary")
        .unwrap();
    let path = tmp.into_temp_path();

    let stamp_bin = env!("CARGO_BIN_EXE_zccache-stamp");
    let status = std::process::Command::new(stamp_bin)
        .arg("--binary")
        .arg(&path)
        .arg("--sha")
        .arg("032432c000000000000000000000000000000000")
        .arg("--version")
        .arg("1.7.2")
        .arg("--triple")
        .arg("x86_64-pc-windows-msvc")
        .arg("--timestamp")
        .arg("1700000000")
        .status()
        .expect("spawn zccache-stamp");
    assert!(
        status.success(),
        "zccache-stamp exited non-zero: {status:?}"
    );

    let marker = read_marker_from_path(&path).expect("marker should be present");
    assert_eq!(marker.git_sha, "032432c000000000000000000000000000000000");
    assert_eq!(marker.version, "1.7.2");
    assert_eq!(marker.triple, "x86_64-pc-windows-msvc");
    assert_eq!(marker.build_timestamp, 1_700_000_000);

    let size = std::fs::metadata(&path).unwrap().len();
    assert!(
        size as usize >= MARKER_SIZE,
        "file should be at least MARKER_SIZE bytes after stamping"
    );
}

#[test]
fn unstamped_file_returns_none() {
    let mut tmp = tempfile::NamedTempFile::new().expect("tempfile");
    // Pad with enough bytes that the read_marker_from_path doesn't
    // short-circuit on the "too small" path — we want it to actually
    // look at the last 128 bytes and find no magic.
    tmp.write_all(&vec![0u8; 256]).unwrap();
    let path = tmp.into_temp_path();
    assert!(read_marker_from_path(&path).is_none());
}
