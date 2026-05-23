//! Integration tests for the `zccache cache-root` introspection
//! subcommand. Issue #275: soldr (and any other wrapper) shells out to
//! this to verify at runtime that its `ZCCACHE_CACHE_DIR` redirect was
//! honored by the binary on PATH.

use std::path::Path;
use std::process::{Command, Stdio};

use zccache::core::NormalizedPath;

fn zccache_bin() -> NormalizedPath {
    let mut path = std::env::current_exe()
        .expect("current_exe")
        .parent()
        .expect("parent of test binary")
        .parent()
        .expect("target dir")
        .to_path_buf();
    if cfg!(windows) {
        path.push("zccache.exe");
    } else {
        path.push("zccache");
    }
    assert!(
        path.exists(),
        "zccache binary not found at {path:?}. Run `cargo build` first."
    );
    NormalizedPath::new(path)
}

fn run_cache_root(cache_dir: Option<&Path>, json: bool) -> std::process::Output {
    let bin = zccache_bin();
    let mut cmd = Command::new(bin.as_path());
    match cache_dir {
        Some(p) => {
            cmd.env("ZCCACHE_CACHE_DIR", p);
        }
        None => {
            cmd.env_remove("ZCCACHE_CACHE_DIR");
        }
    }
    cmd.env_remove("ZCCACHE_COLOCATE");
    cmd.arg("cache-root");
    if json {
        cmd.arg("--json");
    }
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    cmd.output().expect("spawn zccache cache-root")
}

#[test]
fn cache_root_default_prints_absolute_path() {
    let temp = tempfile::tempdir().expect("tempdir");
    let want = temp.path().join("zc");
    let out = run_cache_root(Some(&want), false);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let want_str = want.to_string_lossy().to_string();
    assert_eq!(
        stdout, want_str,
        "stdout `{stdout}` should equal `{want_str}`"
    );
}

#[test]
fn cache_root_env_branch_reports_env_source() {
    let temp = tempfile::tempdir().expect("tempdir");
    let want = temp.path().join("zc");
    let out = run_cache_root(Some(&want), true);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let v: serde_json::Value =
        serde_json::from_str(stdout.trim()).expect("--json must emit valid JSON");
    assert_eq!(v["source"], "env:ZCCACHE_CACHE_DIR");
    let got = v["cache_root"].as_str().expect("cache_root is a string");
    assert_eq!(Path::new(got), want.as_path());
}

#[test]
fn cache_root_default_branch_reports_default_source_when_env_unset() {
    let out = run_cache_root(None, true);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let v: serde_json::Value =
        serde_json::from_str(stdout.trim()).expect("--json must emit valid JSON");
    assert_eq!(v["source"], "default:platform_dirs");
    assert!(
        v["cache_root"].as_str().is_some(),
        "cache_root must be present and stringy"
    );
}

#[test]
fn cache_root_relative_env_path_is_absolute_in_output() {
    // `ZCCACHE_CACHE_DIR=./relative-zc` should still print an absolute
    // path so wrappers don't have to re-resolve it against the cwd.
    let temp = tempfile::tempdir().expect("tempdir");
    let bin = zccache_bin();
    let out = Command::new(bin.as_path())
        .env("ZCCACHE_CACHE_DIR", "relative-zc")
        .env_remove("ZCCACHE_COLOCATE")
        .current_dir(temp.path())
        .arg("cache-root")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("spawn zccache cache-root");
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
    assert!(
        Path::new(&stdout).is_absolute(),
        "stdout `{stdout}` must be an absolute path"
    );
    assert!(
        stdout.ends_with("relative-zc"),
        "stdout `{stdout}` should end with the relative override stem"
    );
}
