//! Integration tests for `zccache meson configure` — issue #627.
//!
//! TDD pins for the configure-cache wrapper. The contract:
//!
//! - First invocation with given (source-dir + meson.build set + meson
//!   version + selected env vars) is a MISS: shells out to real `meson
//!   setup`, captures the resulting build dir, exits with meson's exit
//!   code.
//! - Subsequent invocations with the same inputs are HITs: the captured
//!   build dir is restored from cache; meson is not invoked.
//! - On HIT, `build.ninja` content matches the original run exactly.
//! - On HIT, the cached run completes substantially faster than the cold
//!   run.

use std::path::Path;
use std::process::Command;
use std::time::Instant;

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
        "zccache binary not found at {path:?}. Run `cargo build -p zccache --bin zccache` first."
    );
    NormalizedPath::new(path)
}

fn meson_available() -> bool {
    Command::new("meson")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn write_tiny_meson_project(source_dir: &Path) {
    std::fs::write(
        source_dir.join("meson.build"),
        "project('zccache-mc-test', 'c')\nexecutable('hello', 'main.c')\n",
    )
    .unwrap();
    std::fs::write(source_dir.join("main.c"), "int main(void) { return 0; }\n").unwrap();
}

fn run_zccache_meson_configure(
    cache_dir: &Path,
    source_dir: &Path,
    build_dir: &Path,
) -> std::process::Output {
    let bin = zccache_bin();
    let mut cmd = Command::new(bin.as_path());
    cmd.env("ZCCACHE_CACHE_DIR", cache_dir);
    // Soldr sets ZCCACHE_SESSION_ID on the parent test process; the
    // child would otherwise inherit and try to talk to a daemon at the
    // session's endpoint instead of running the self-contained meson
    // configure cache.
    cmd.env_remove("ZCCACHE_SESSION_ID");
    cmd.arg("meson").arg("configure");
    cmd.arg("--source-dir").arg(source_dir);
    cmd.arg("--build-dir").arg(build_dir);
    cmd.output().expect("spawn zccache meson configure")
}

#[test]
fn first_invocation_misses_and_runs_real_meson() {
    if !meson_available() {
        eprintln!("SKIP: meson not on PATH");
        return;
    }
    let cache = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let source = project.path().join("src");
    let build = project.path().join("build");
    std::fs::create_dir_all(&source).unwrap();
    write_tiny_meson_project(&source);

    let output = run_zccache_meson_configure(cache.path(), &source, &build);
    assert!(
        output.status.success(),
        "first invocation must succeed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    // meson must have populated the build dir
    assert!(
        build.join("build.ninja").exists(),
        "first invocation should produce build.ninja"
    );
    // stderr should report a MISS so an operator can see it in CI logs
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("[zccache-meson] miss"),
        "stderr must mark the miss path; got: {stderr}",
    );
}

#[test]
fn second_invocation_hits_cache_and_restores_build_dir() {
    if !meson_available() {
        eprintln!("SKIP: meson not on PATH");
        return;
    }
    let cache = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let source = project.path().join("src");
    std::fs::create_dir_all(&source).unwrap();
    write_tiny_meson_project(&source);

    let build_a = project.path().join("build-a");
    let cold = Instant::now();
    let out_a = run_zccache_meson_configure(cache.path(), &source, &build_a);
    let cold_elapsed = cold.elapsed();
    assert!(out_a.status.success(), "cold run must succeed");
    let ninja_a = std::fs::read(build_a.join("build.ninja")).unwrap();

    // Wipe the build dir to prove the cache is restoring, not just
    // hitting a no-op "build dir already good" check.
    std::fs::remove_dir_all(&build_a).unwrap();

    let warm = Instant::now();
    let out_b = run_zccache_meson_configure(cache.path(), &source, &build_a);
    let warm_elapsed = warm.elapsed();
    assert!(
        out_b.status.success(),
        "warm run must succeed: stdout={} stderr={}",
        String::from_utf8_lossy(&out_b.stdout),
        String::from_utf8_lossy(&out_b.stderr),
    );

    let stderr_b = String::from_utf8_lossy(&out_b.stderr);
    assert!(
        stderr_b.contains("[zccache-meson] hit"),
        "stderr must mark the hit path; got: {stderr_b}",
    );

    let ninja_b = std::fs::read(build_a.join("build.ninja")).unwrap();
    assert_eq!(
        ninja_a, ninja_b,
        "restored build.ninja must match the cold-run build.ninja byte-for-byte"
    );

    // Warm run must be substantially faster than the cold run. Allow a
    // generous 4× headroom — cold runs typically take 1–5 s on this
    // tiny project; warm should be under 200 ms.
    assert!(
        warm_elapsed.as_millis() * 4 < cold_elapsed.as_millis(),
        "warm run should be much faster than cold: cold={cold_elapsed:?}, warm={warm_elapsed:?}"
    );
}

#[test]
fn changing_meson_build_busts_the_cache() {
    if !meson_available() {
        eprintln!("SKIP: meson not on PATH");
        return;
    }
    let cache = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let source = project.path().join("src");
    std::fs::create_dir_all(&source).unwrap();
    write_tiny_meson_project(&source);

    let build = project.path().join("build");
    let out_a = run_zccache_meson_configure(cache.path(), &source, &build);
    assert!(out_a.status.success());
    std::fs::remove_dir_all(&build).unwrap();

    // Mutate meson.build — must invalidate the cache.
    std::fs::write(
        source.join("meson.build"),
        "project('zccache-mc-test-v2', 'c')\nexecutable('hello', 'main.c')\n",
    )
    .unwrap();

    let out_b = run_zccache_meson_configure(cache.path(), &source, &build);
    assert!(out_b.status.success());
    let stderr_b = String::from_utf8_lossy(&out_b.stderr);
    assert!(
        stderr_b.contains("[zccache-meson] miss"),
        "changing meson.build must produce a fresh miss; got: {stderr_b}",
    );
}
