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

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::panic_in_result_fn,
    clippy::unwrap_in_result
)]

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

    // The hit marker and restored build file above prove correctness; this is
    // only a coarse speed regression guard for slow Windows hosts.
    assert!(
        warm_elapsed.as_millis() * 2 < cold_elapsed.as_millis(),
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

// ============================================================================
// `--input-file` — issue #654.
// ============================================================================
// The wrapper key by default covers meson.build / meson.options /
// meson_options.txt content. Downstream projects whose source-change
// detection lives OUTSIDE the meson.build set (e.g. FastLED's
// metadata-cache layer that hashes test/example/source globs and writes
// a sidecar digest) can extend the key by passing one or more
// `--input-file PATH` flags. Each file's content enters the key; if any
// file's content changes, the next invocation is a fresh miss.

fn run_zccache_meson_configure_with_extra_inputs(
    cache_dir: &Path,
    source_dir: &Path,
    build_dir: &Path,
    extra_input_files: &[&Path],
) -> std::process::Output {
    let bin = zccache_bin();
    let mut cmd = Command::new(bin.as_path());
    cmd.env("ZCCACHE_CACHE_DIR", cache_dir);
    cmd.env_remove("ZCCACHE_SESSION_ID");
    cmd.arg("meson").arg("configure");
    cmd.arg("--source-dir").arg(source_dir);
    cmd.arg("--build-dir").arg(build_dir);
    for f in extra_input_files {
        cmd.arg("--input-file").arg(f);
    }
    cmd.output()
        .expect("spawn zccache meson configure --input-file ...")
}

#[test]
fn input_file_change_busts_the_cache() {
    if !meson_available() {
        eprintln!("SKIP: meson not on PATH");
        return;
    }
    let cache = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let source = project.path().join("src");
    std::fs::create_dir_all(&source).unwrap();
    write_tiny_meson_project(&source);

    // Sidecar digest file the downstream caller maintains.
    let sidecar = project.path().join("sources.hash");
    std::fs::write(&sidecar, "deadbeef-v1").unwrap();

    let build = project.path().join("build");
    let out_a =
        run_zccache_meson_configure_with_extra_inputs(cache.path(), &source, &build, &[&sidecar]);
    assert!(
        out_a.status.success(),
        "cold run must succeed: stdout={} stderr={}",
        String::from_utf8_lossy(&out_a.stdout),
        String::from_utf8_lossy(&out_a.stderr),
    );
    let stderr_a = String::from_utf8_lossy(&out_a.stderr);
    assert!(stderr_a.contains("[zccache-meson] miss"));

    std::fs::remove_dir_all(&build).unwrap();

    // Sidecar content unchanged → expect a hit.
    let out_b =
        run_zccache_meson_configure_with_extra_inputs(cache.path(), &source, &build, &[&sidecar]);
    assert!(out_b.status.success());
    let stderr_b = String::from_utf8_lossy(&out_b.stderr);
    assert!(
        stderr_b.contains("[zccache-meson] hit"),
        "unchanged --input-file content must still hit; got: {stderr_b}",
    );

    std::fs::remove_dir_all(&build).unwrap();

    // Mutate the sidecar → expect a miss even though meson.build is unchanged.
    std::fs::write(&sidecar, "cafef00d-v2").unwrap();

    let out_c =
        run_zccache_meson_configure_with_extra_inputs(cache.path(), &source, &build, &[&sidecar]);
    assert!(out_c.status.success());
    let stderr_c = String::from_utf8_lossy(&out_c.stderr);
    assert!(
        stderr_c.contains("[zccache-meson] miss"),
        "mutating --input-file content must invalidate the cache; got: {stderr_c}",
    );
}

#[test]
fn input_file_is_distinct_from_no_input_file() {
    if !meson_available() {
        eprintln!("SKIP: meson not on PATH");
        return;
    }
    // Same source tree, two cache populations: one with `--input-file`,
    // one without. They must NOT share entries — the with-input-file run
    // is a fresh miss even though the no-input-file run already populated
    // the cache for the same meson.build.
    let cache = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let source = project.path().join("src");
    std::fs::create_dir_all(&source).unwrap();
    write_tiny_meson_project(&source);

    let sidecar = project.path().join("sources.hash");
    std::fs::write(&sidecar, "anything").unwrap();

    let build = project.path().join("build");
    let out_a = run_zccache_meson_configure(cache.path(), &source, &build);
    assert!(out_a.status.success());
    std::fs::remove_dir_all(&build).unwrap();

    let out_b =
        run_zccache_meson_configure_with_extra_inputs(cache.path(), &source, &build, &[&sidecar]);
    assert!(out_b.status.success());
    let stderr_b = String::from_utf8_lossy(&out_b.stderr);
    assert!(
        stderr_b.contains("[zccache-meson] miss"),
        "adding --input-file to a previously-cached entry must produce a fresh miss; got: {stderr_b}",
    );
}

#[test]
fn input_file_order_does_not_affect_key() {
    if !meson_available() {
        eprintln!("SKIP: meson not on PATH");
        return;
    }
    // Two `--input-file` flags in opposite orders must produce the same
    // cache key (the implementation sorts internally). This pins that
    // contract so a caller using `BTreeMap`/`HashMap` iteration doesn't
    // accidentally split cache entries by argv order.
    let cache = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let source = project.path().join("src");
    std::fs::create_dir_all(&source).unwrap();
    write_tiny_meson_project(&source);

    let a = project.path().join("a.hash");
    let b = project.path().join("b.hash");
    std::fs::write(&a, "alpha").unwrap();
    std::fs::write(&b, "beta").unwrap();

    let build = project.path().join("build");
    let out_a =
        run_zccache_meson_configure_with_extra_inputs(cache.path(), &source, &build, &[&a, &b]);
    assert!(out_a.status.success());
    std::fs::remove_dir_all(&build).unwrap();

    let out_b =
        run_zccache_meson_configure_with_extra_inputs(cache.path(), &source, &build, &[&b, &a]);
    assert!(out_b.status.success());
    let stderr_b = String::from_utf8_lossy(&out_b.stderr);
    assert!(
        stderr_b.contains("[zccache-meson] hit"),
        "reordering --input-file flags must NOT change the key; got: {stderr_b}",
    );
}

// ============================================================================
// `--no-walk` — issue #659.
// ============================================================================
// Callers who know their meson.build set exactly and want to avoid the
// implicit recursive walk of `--source-dir` (e.g. monorepos whose
// scratch dirs aren't on the default skip list) can pass `--no-walk`
// plus per-file `--input-file` flags. The wrapper then skips the source
// walk entirely and keys only on the supplied inputs + (source, build,
// env, args, meson-version) tuple.

fn run_zccache_meson_configure_no_walk(
    cache_dir: &Path,
    source_dir: &Path,
    build_dir: &Path,
    extra_input_files: &[&Path],
) -> std::process::Output {
    let bin = zccache_bin();
    let mut cmd = Command::new(bin.as_path());
    cmd.env("ZCCACHE_CACHE_DIR", cache_dir);
    cmd.env_remove("ZCCACHE_SESSION_ID");
    cmd.arg("meson").arg("configure");
    cmd.arg("--source-dir").arg(source_dir);
    cmd.arg("--build-dir").arg(build_dir);
    cmd.arg("--no-walk");
    for f in extra_input_files {
        cmd.arg("--input-file").arg(f);
    }
    cmd.output()
        .expect("spawn zccache meson configure --no-walk")
}

#[test]
fn no_walk_requires_at_least_one_input_file() {
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
    // No --input-file supplied — wrapper must refuse rather than emit a
    // surprisingly-broad cache hit later.
    let out = run_zccache_meson_configure_no_walk(cache.path(), &source, &build, &[]);
    assert!(
        !out.status.success(),
        "--no-walk without --input-file must fail"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("--no-walk requires at least one --input-file"),
        "stderr must explain the constraint; got: {stderr}",
    );
}

#[test]
fn no_walk_hits_on_unchanged_input_files() {
    if !meson_available() {
        eprintln!("SKIP: meson not on PATH");
        return;
    }
    let cache = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let source = project.path().join("src");
    std::fs::create_dir_all(&source).unwrap();
    write_tiny_meson_project(&source);

    let inputs = source.join("meson.build");
    let build = project.path().join("build");

    let out_a = run_zccache_meson_configure_no_walk(cache.path(), &source, &build, &[&inputs]);
    assert!(
        out_a.status.success(),
        "cold --no-walk run must succeed: stdout={} stderr={}",
        String::from_utf8_lossy(&out_a.stdout),
        String::from_utf8_lossy(&out_a.stderr),
    );
    let stderr_a = String::from_utf8_lossy(&out_a.stderr);
    assert!(stderr_a.contains("[zccache-meson] miss"));

    std::fs::remove_dir_all(&build).unwrap();

    let out_b = run_zccache_meson_configure_no_walk(cache.path(), &source, &build, &[&inputs]);
    assert!(out_b.status.success());
    let stderr_b = String::from_utf8_lossy(&out_b.stderr);
    assert!(
        stderr_b.contains("[zccache-meson] hit"),
        "second --no-walk run with unchanged --input-file must hit; got: {stderr_b}",
    );
}

#[test]
fn no_walk_is_keyed_distinctly_from_walked() {
    if !meson_available() {
        eprintln!("SKIP: meson not on PATH");
        return;
    }
    // A run with --no-walk and a run without --no-walk produce different
    // cache keys even when the explicitly-named input is the same file
    // the implicit walk would have found. This is by design: the walked
    // case hashes every meson.build under source-dir; the no-walk case
    // hashes only what's listed. Cross-pollution between them would be a
    // foot-gun.
    let cache = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let source = project.path().join("src");
    std::fs::create_dir_all(&source).unwrap();
    write_tiny_meson_project(&source);

    let inputs = source.join("meson.build");
    let build = project.path().join("build");

    // Walked-mode populates the cache.
    let out_a = run_zccache_meson_configure(cache.path(), &source, &build);
    assert!(out_a.status.success());
    std::fs::remove_dir_all(&build).unwrap();

    // No-walk-mode with the same input — must NOT hit the walked entry.
    let out_b = run_zccache_meson_configure_no_walk(cache.path(), &source, &build, &[&inputs]);
    assert!(out_b.status.success());
    let stderr_b = String::from_utf8_lossy(&out_b.stderr);
    assert!(
        stderr_b.contains("[zccache-meson] miss"),
        "--no-walk run must NOT reuse a walked-mode cache entry; got: {stderr_b}",
    );
}
