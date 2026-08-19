//! Distribution contract for #1000: the shipped `zccache` executable hosts
//! both daemon entry points under exact argv[0] names.

use std::process::Command;

#[test]
fn zccache_copy_dispatches_as_download_daemon() {
    let source = std::path::Path::new(env!("CARGO_BIN_EXE_zccache"));
    let temp = tempfile::tempdir().expect("create isolated deploy directory");
    let deployed = temp.path().join(if cfg!(windows) {
        "zccache-download-daemon.exe"
    } else {
        "zccache-download-daemon"
    });
    std::fs::copy(source, &deployed).expect("copy multicall binary under download-daemon name");

    let output = Command::new(&deployed)
        .arg("--version")
        .output()
        .expect("run self-deployed download daemon copy");

    assert!(
        output.status.success(),
        "download-daemon dispatch failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).starts_with("zccache-download-daemon "),
        "argv[0] did not select the download-daemon entry: {}",
        String::from_utf8_lossy(&output.stdout)
    );
}
