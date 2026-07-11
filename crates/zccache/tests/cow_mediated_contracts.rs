//! Stable cc/c++ wrapper contract acceptance for #1039.

#![allow(clippy::expect_used)]

use std::process::Command;

#[test]
#[ignore = "integration: requires cc and c++ on PATH and starts a daemon"]
fn cc_and_cxx_preserve_argv_exit_and_streams() {
    let zccache = env!("CARGO_BIN_EXE_zccache");
    let cache = tempfile::tempdir().expect("cache");
    for compiler in ["cc", "c++"] {
        let Ok(direct) = Command::new(compiler).arg("--version").output() else {
            eprintln!("SKIP {compiler}: compiler not on PATH");
            continue;
        };
        let wrapped = if compiler == "cc" {
            Command::new(zccache)
                .args(["cc", "--version"])
                .env("ZCCACHE_CACHE_DIR", cache.path())
                .output()
                .expect("run zccache cc")
        } else {
            Command::new(zccache)
                .args(["c++", "--version"])
                .env("ZCCACHE_CACHE_DIR", cache.path())
                .output()
                .expect("run zccache c++")
        };
        assert_eq!(
            wrapped.status.code(),
            direct.status.code(),
            "{compiler} exit"
        );
        assert_eq!(wrapped.stdout, direct.stdout, "{compiler} stdout");
        assert_eq!(wrapped.stderr, direct.stderr, "{compiler} stderr");
    }
    let _ = Command::new(zccache)
        .arg("stop")
        .env("ZCCACHE_CACHE_DIR", cache.path())
        .output();
}
