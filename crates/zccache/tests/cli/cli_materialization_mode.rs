//! Executable-level contract for an invalid `ZCCACHE_MODE` (#1683, D4).
//!
//! The daemon resolves the mode per request and only logs an invalid value,
//! so the wrapper must reject it before dispatch: non-zero exit, the
//! variable named in the error, and the wrapped tool never run.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::path::PathBuf;
use std::process::{Command, Stdio};

fn binary_path(stem: &str) -> PathBuf {
    let mut path = std::env::current_exe().expect("current executable");
    path.pop();
    path.pop();
    path.push(if cfg!(windows) {
        format!("{stem}.exe")
    } else {
        stem.to_string()
    });
    path
}

#[test]
#[ignore = "integration test: launches the wrapper binary"]
fn invalid_zccache_mode_fails_before_dispatch() {
    let zccache = binary_path("zccache");
    let echo_shim = binary_path("echo_shim");
    if !zccache.exists() || !echo_shim.exists() {
        eprintln!("skipping: required binaries are not built");
        return;
    }
    let cache_dir = tempfile::tempdir().expect("cache tempdir");
    let output = Command::new(&zccache)
        .arg(&echo_shim)
        .arg("7")
        .env("ZCCACHE_CACHE_DIR", cache_dir.path())
        .env("ZCCACHE_DAEMON_NAMESPACE", "invalid-materialization-mode")
        .env("ZCCACHE_NO_SPAWN", "1")
        .env("ZCCACHE_MODE", "hardlink")
        .env_remove("ZCCACHE_DISABLE")
        .env_remove("ZCCACHE_PROBE_BYPASS")
        .env_remove("ZCCACHE_ENDPOINT")
        .stdin(Stdio::null())
        .output()
        .expect("run wrapper");

    assert_eq!(
        output.status.code(),
        Some(1),
        "an invalid mode is a usage error"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("invalid ZCCACHE_MODE value \"hardlink\""),
        "stderr must name the variable and value: {stderr}"
    );
    assert!(
        stderr.contains("AUTO, LINK, COPY, REFLINK"),
        "stderr must list the valid modes: {stderr}"
    );
    assert!(
        output.stdout.is_empty(),
        "the wrapped tool must not run: {:?}",
        String::from_utf8_lossy(&output.stdout)
    );
}
