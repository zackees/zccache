//! Integration test: meson + ninja full-project rebuild through zccache.
//!
//! This file covers the end-to-end pipeline where `meson setup` + `ninja`
//! drive the real build system using the zccache CLI as the compiler wrapper.
//! The direct CLI/IPC ephemeral and stress/bench scenarios live in the sibling
//! `daemon_ninja_rebuild_direct_test.rs` file.
//!
//! Run:    soldr cargo test -p zccache-daemon --test daemon_ninja_rebuild_meson_test -- --ignored --nocapture

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::panic_in_result_fn,
    clippy::unwrap_in_result
)]

use std::sync::{Arc, Once};
use tokio::sync::Notify;
use tokio::task::JoinHandle;
use zccache::core::NormalizedPath;
use zccache::daemon::DaemonServer;
use zccache::protocol::{Request, Response};
use zccache::test_support::{MesonProject, TestProject};

// ─── Helpers ─────────────────────────────────────────────────────────────────

/// Build the release CLI binary once across all tests.
/// Release is critical for meson tests because meson probes the compiler ~12
/// times during setup — each invocation goes through the zccache wrapper, so
/// debug overhead (~1.5s/call) dominates.
static BUILD_RELEASE_CLI: Once = Once::new();

fn build_and_find_release_cli() -> NormalizedPath {
    BUILD_RELEASE_CLI.call_once(|| {
        let status = std::process::Command::new("cargo")
            .args(["build", "--release", "-p", "zccache", "--bin", "zccache"])
            .env_remove("RUSTC_WRAPPER")
            .env_remove("RUSTC_WORKSPACE_WRAPPER")
            .status()
            .expect("failed to run cargo build --release");
        assert!(status.success(), "cargo build --release failed");
    });

    // Release binary is in target/release/, not target/debug/
    let debug_dir = std::path::Path::new(env!("CARGO_BIN_EXE_zccache-daemon"))
        .parent()
        .unwrap();
    // debug_dir is target/debug/ — go up to target/ then into release/
    let release_dir = debug_dir.parent().unwrap().join("release");
    if cfg!(windows) {
        release_dir.join("zccache.exe").into()
    } else {
        release_dir.join("zccache").into()
    }
}

async fn start_daemon(endpoint: &str) -> (JoinHandle<()>, Arc<Notify>) {
    let mut server = DaemonServer::bind(endpoint).unwrap();
    let shutdown = server.shutdown_handle();
    let handle = tokio::spawn(async move {
        server.run(0).await.unwrap();
    });
    (handle, shutdown)
}

async fn get_status(endpoint: &str) -> zccache::protocol::DaemonStatus {
    let mut client = zccache::ipc::connect(endpoint).await.unwrap();
    client.send(&Request::Status).await.unwrap();
    match client.recv().await.unwrap() {
        Some(Response::Status(s)) => s,
        other => panic!("expected Status, got: {other:?}"),
    }
}

async fn clear_cache(endpoint: &str) {
    let mut client = zccache::ipc::connect(endpoint).await.unwrap();
    client.send(&Request::Clear).await.unwrap();
    match client.recv().await.unwrap() {
        Some(Response::Cleared { .. }) => {}
        other => panic!("expected Cleared, got: {other:?}"),
    }
}

// ─── Tool discovery ──────────────────────────────────────────────────────────

/// Simple PATH lookup (mirrors the CLI's which_on_path).
fn which_on_path(name: &str) -> Option<NormalizedPath> {
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate.into());
        }
        #[cfg(windows)]
        if std::path::Path::new(name).extension().is_none() {
            let with_exe = dir.join(format!("{name}.exe"));
            if with_exe.is_file() {
                return Some(with_exe.into());
            }
        }
    }
    None
}

/// Find meson: `MESON` env var, then PATH.
fn find_meson() -> Option<NormalizedPath> {
    if let Ok(p) = std::env::var("MESON") {
        let path = NormalizedPath::new(p);
        if path.is_file() {
            return Some(path);
        }
    }
    which_on_path("meson")
}

/// Find ninja: `NINJA` env var, then PATH.
fn find_ninja() -> Option<NormalizedPath> {
    if let Ok(p) = std::env::var("NINJA") {
        let path = NormalizedPath::new(p);
        if path.is_file() {
            return Some(path);
        }
    }
    which_on_path("ninja")
}

// ═══════════════════════════════════════════════════════════════════════════════
// MESON + NINJA INTEGRATION TESTS (run with --ignored, requires meson + ninja)
// ═══════════════════════════════════════════════════════════════════════════════

/// Full meson+ninja cold → warm rebuild through zccache.
///
/// This is the end-to-end test that exercises the real build system pipeline:
///   1. Generate a self-contained meson C++ project
///   2. `meson setup` with zccache as the compiler wrapper
///   3. `ninja` cold build (all cache misses)
///   4. `ninja -t clean` + `ninja` warm rebuild (all cache hits)
///   5. Verify warm is significantly faster than cold
///
/// Run:  soldr cargo test -p zccache-daemon --test daemon_ninja_rebuild_meson_test -- meson_ninja_cold --ignored --nocapture
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn meson_ninja_cold_then_warm_rebuild() {
    let clang = match zccache::test_support::find_clang() {
        Some(p) => p,
        None => {
            eprintln!("skipping: clang not found");
            return;
        }
    };
    let meson_bin = match find_meson() {
        Some(p) => p,
        None => {
            eprintln!("skipping: meson not found on PATH");
            return;
        }
    };
    let ninja_bin = match find_ninja() {
        Some(p) => p,
        None => {
            eprintln!("skipping: ninja not found on PATH");
            return;
        }
    };

    // Build release CLI binary — release is critical because meson probes the
    // compiler ~12 times during setup, each through the zccache wrapper.
    // Debug wrapper: ~1.5s/probe → 18s setup. Release: ~0.1s/probe → 1.2s setup.
    let cli = build_and_find_release_cli();

    let project = TestProject::integration();
    let file_count = project.source_count;

    // Generate project files and start daemon concurrently — they're independent.
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("project");
    std::fs::create_dir_all(&root).unwrap();
    let meson = project.generate_meson(&root);

    let build_dir = tmp.path().join("build");
    let native_file = tmp.path().join("native.ini");

    let endpoint = zccache::ipc::unique_test_endpoint();
    let (server_handle, shutdown) = start_daemon(&endpoint).await;
    clear_cache(&endpoint).await;

    MesonProject::write_native_file(&native_file, &clang, None, Some(&cli));

    eprintln!("Meson+Ninja test: {file_count} source files");
    eprintln!("  meson: {}", meson_bin.display());
    eprintln!("  ninja: {}", ninja_bin.display());
    eprintln!("  clang: {}", clang.display());
    eprintln!("  zccache: {} (release)", cli.display());
    eprintln!("  endpoint: {endpoint}");

    let env = [("ZCCACHE_ENDPOINT", endpoint.as_str())];

    // ── Cold build (meson setup + ninja) ────────────────────────────
    let cold = meson.build(&build_dir, &native_file, &meson_bin, &ninja_bin, &env);
    eprintln!(
        "Cold: setup {}ms + build {}ms = {}ms",
        cold.setup_ms, cold.build_ms, cold.total_ms,
    );

    let status = get_status(&endpoint).await;
    eprintln!(
        "  {} compilations, {} hits, {} misses, {} non-cacheable",
        status.total_compilations, status.cache_hits, status.cache_misses, status.non_cacheable,
    );

    // Meson probes add non-cacheable compilations; actual source files are misses.
    assert!(
        status.cache_misses >= file_count as u64,
        "cold build should have at least {file_count} misses, got {}",
        status.cache_misses,
    );

    // ── Warm rebuild (ninja clean + ninja) ──────────────────────────
    MesonProject::ninja_clean(&ninja_bin, &build_dir);
    let warm_ms = MesonProject::ninja_rebuild(&ninja_bin, &build_dir, &env);
    eprintln!("Warm: rebuild {warm_ms}ms");

    let status2 = get_status(&endpoint).await;
    let new_hits = status2.cache_hits - status.cache_hits;
    eprintln!(
        "  {} new hits, {} total compilations",
        new_hits, status2.total_compilations,
    );

    // All source files should be cache hits on warm rebuild.
    assert!(
        new_hits >= file_count as u64,
        "warm rebuild should have at least {file_count} new hits, got {new_hits}",
    );

    // Warm should be significantly faster than cold build.
    if cold.build_ms > 0 {
        let speedup = cold.build_ms as f64 / warm_ms.max(1) as f64;
        eprintln!(
            "Speedup: {speedup:.1}x (cold {}ms → warm {warm_ms}ms)",
            cold.build_ms
        );
        assert!(
            speedup >= 1.5,
            "warm rebuild should be at least 1.5x faster than cold (got {speedup:.1}x)",
        );
    }

    shutdown.notify_one();
    server_handle.await.unwrap();
}

/// Meson+ninja benchmark: larger project, cold + 3 warm iterations.
///
/// Run:  soldr cargo test -p zccache-daemon --test daemon_ninja_rebuild_meson_test -- meson_ninja_bench --ignored --nocapture
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn meson_ninja_bench_warm_iterations() {
    let clang = match zccache::test_support::find_clang() {
        Some(p) => p,
        None => {
            eprintln!("skipping: clang not found");
            return;
        }
    };
    let meson_bin = match find_meson() {
        Some(p) => p,
        None => {
            eprintln!("skipping: meson not found on PATH");
            return;
        }
    };
    let ninja_bin = match find_ninja() {
        Some(p) => p,
        None => {
            eprintln!("skipping: ninja not found on PATH");
            return;
        }
    };

    let cli = build_and_find_release_cli();

    let project = TestProject::benchmark();
    let file_count = project.source_count;

    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("project");
    std::fs::create_dir_all(&root).unwrap();
    let meson = project.generate_meson(&root);

    let build_dir = tmp.path().join("build");
    let native_file = tmp.path().join("native.ini");

    let endpoint = zccache::ipc::unique_test_endpoint();
    let (server_handle, shutdown) = start_daemon(&endpoint).await;
    clear_cache(&endpoint).await;

    MesonProject::write_native_file(&native_file, &clang, None, Some(&cli));

    let env = [("ZCCACHE_ENDPOINT", endpoint.as_str())];

    eprintln!(
        "Meson+Ninja benchmark: {file_count} files, medium bodies, 3 warm iterations (release cli)"
    );

    // ── Cold build ──────────────────────────────────────────────────
    let cold = meson.build(&build_dir, &native_file, &meson_bin, &ninja_bin, &env);
    eprintln!(
        "Cold:    setup {:>5}ms + build {:>5}ms = {:>5}ms",
        cold.setup_ms, cold.build_ms, cold.total_ms,
    );

    // ── Warm iterations ─────────────────────────────────────────────
    let mut warm_times = Vec::new();
    for iter in 1..=3 {
        MesonProject::ninja_clean(&ninja_bin, &build_dir);
        let ms = MesonProject::ninja_rebuild(&ninja_bin, &build_dir, &env);
        eprintln!("Warm #{iter}: {ms:>5}ms");
        warm_times.push(ms);
    }

    let avg = warm_times.iter().sum::<u128>() / warm_times.len() as u128;
    let min = *warm_times.iter().min().unwrap();
    let max = *warm_times.iter().max().unwrap();
    eprintln!("\nWarm avg: {avg}ms, min: {min}ms, max: {max}ms");
    eprintln!(
        "Speedup: {:.1}x (cold build {}ms → warm avg {avg}ms)",
        cold.build_ms as f64 / avg.max(1) as f64,
        cold.build_ms,
    );

    let status = get_status(&endpoint).await;
    eprintln!(
        "Final: {} artifacts, {} hits, {} misses",
        status.artifact_count, status.cache_hits, status.cache_misses,
    );

    shutdown.notify_one();
    server_handle.await.unwrap();
}
