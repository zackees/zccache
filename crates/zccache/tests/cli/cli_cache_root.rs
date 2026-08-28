//! Integration tests for the `zccache cache-root` introspection
//! subcommand. Issue #275: soldr (and any other wrapper) shells out to
//! this to verify at runtime that its `ZCCACHE_CACHE_DIR` redirect was
//! honored by the binary on PATH.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::panic_in_result_fn,
    clippy::unwrap_in_result
)]

use std::path::Path;
use std::process::{Command, Stdio};

use zccache::core::config::versioned_subdir;
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

fn expected_dev_namespace(bin: &Path) -> String {
    let hash = running_process::blake3_file(bin).expect("hash development zccache binary");
    format!(
        "{}-{}",
        zccache::core::VERSION,
        &hash.to_hex().as_str()[..16]
    )
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
    cmd.env_remove("ZCCACHE_DAEMON_NAMESPACE");
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
    // Issue #761 / #762 Phase 0: cache-root prints the effective
    // (version-namespaced) root that the daemon actually reads/writes
    // under, so wrappers can compare it to the per-version subdir on
    // disk without re-joining the version segment themselves.
    let temp = tempfile::tempdir().expect("tempdir");
    let want = temp.path().join("zc");
    let out = run_cache_root(Some(&want), false);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let want_str = want.join(versioned_subdir()).to_string_lossy().to_string();
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
    assert_eq!(Path::new(got), want.join(versioned_subdir()).as_path());
    let namespace = expected_dev_namespace(zccache_bin().as_path());
    assert_eq!(v["daemon_namespace"], namespace);
    assert!(
        v["daemon_endpoint"].as_str().is_some(),
        "daemon_endpoint must be present and stringy"
    );
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
    let namespace = expected_dev_namespace(zccache_bin().as_path());
    assert_eq!(v["daemon_namespace"], namespace);
}

#[test]
fn cache_root_json_reports_daemon_namespace_and_derived_endpoint() {
    let temp = tempfile::tempdir().expect("tempdir");
    let want = temp.path().join("zc");
    let bin = zccache_bin();
    let out = Command::new(bin.as_path())
        .env("ZCCACHE_CACHE_DIR", &want)
        .env("ZCCACHE_DAEMON_NAMESPACE", "soldr-dev")
        .env_remove("ZCCACHE_COLOCATE")
        .arg("cache-root")
        .arg("--json")
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
    let stdout = String::from_utf8_lossy(&out.stdout);
    let v: serde_json::Value =
        serde_json::from_str(stdout.trim()).expect("--json must emit valid JSON");
    assert_eq!(v["daemon_namespace"], "soldr-dev");
    let endpoint = v["daemon_endpoint"]
        .as_str()
        .expect("daemon_endpoint is a string");
    assert!(
        endpoint.contains("soldr-dev"),
        "endpoint `{endpoint}` must include namespace"
    );
}

#[test]
fn daemon_namespace_does_not_move_cargo_registry_archives() {
    let temp = tempfile::tempdir().expect("tempdir");
    let cache_root = temp.path().join("zc");
    let cargo_home = temp.path().join("cargo-home");
    let registry_index = cargo_home.join("registry").join("index");
    std::fs::create_dir_all(&registry_index).expect("create fake cargo registry");
    std::fs::write(registry_index.join("marker"), b"registry payload")
        .expect("write fake cargo registry payload");

    let bin = zccache_bin();
    let output = Command::new(bin.as_path())
        .env("ZCCACHE_CACHE_DIR", &cache_root)
        .env("ZCCACHE_DAEMON_NAMESPACE", "dev-binary-hash")
        .env_remove("ZCCACHE_COLOCATE")
        .env_remove("SOLDR_SKIP_CARGO_REGISTRY_SAVE")
        .args([
            "cargo-registry",
            "save",
            "--key",
            "namespace-stable",
            "--cargo-home",
        ])
        .arg(&cargo_home)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("spawn zccache cargo-registry save");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let expected = cache_root
        .join(versioned_subdir())
        .join("cargo-registry")
        .join("namespace-stable.tar.gz");
    assert!(
        expected.is_file(),
        "cargo-registry archive must remain at the stable cache-root path; stdout: {}",
        String::from_utf8_lossy(&output.stdout)
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
        .env_remove("ZCCACHE_DAEMON_NAMESPACE")
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
    let stdout_path = Path::new(&stdout);
    assert!(
        stdout_path.ends_with(versioned_subdir()),
        "stdout `{stdout}` should end with the version subdir `{}`",
        versioned_subdir()
    );
    let parent = stdout_path.parent().expect("stdout has a parent component");
    assert!(
        parent.ends_with("relative-zc"),
        "stdout's parent `{}` should be the relative override stem",
        parent.display()
    );
}
