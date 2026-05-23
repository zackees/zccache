//! Integration test: simulated ninja-style full-project rebuild.
//!
//! This test mirrors what meson+ninja does in a real build:
//! 1. Generate a multi-file C++ project with shared headers
//! 2. Cold build: invoke `zccache clang++ -c unit.cpp -o unit.o` for each file (ephemeral mode)
//! 3. Warm rebuild: delete all .o files, re-invoke — all should be cache hits
//! 4. Verify: output bytes are identical, daemon stats confirm hit/miss counts
//! 5. Persistent artifacts: stop daemon, restart, rebuild — still hits
//!
//! Each invocation goes through the real CLI binary in drop-in wrapper mode,
//! exercising the full `CompileEphemeral` single-roundtrip IPC path.
//!
//! Run all:    soldr cargo test -p zccache-daemon --test ninja_rebuild_test -- --nocapture
//! Run stress: soldr cargo test -p zccache-daemon --test ninja_rebuild_test -- --ignored --nocapture

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Once};
use tokio::sync::Notify;
use tokio::task::JoinHandle;
use zccache_monocrate::core::NormalizedPath;
use zccache_monocrate::daemon::DaemonServer;
use zccache_monocrate::protocol::{Request, Response};
use zccache_monocrate::test_support::{MesonProject, TestProject};

// ─── Helpers ─────────────────────────────────────────────────────────────────

/// Build the debug CLI binary once across all tests (avoids Cargo lock contention).
static BUILD_DEBUG_CLI: Once = Once::new();

fn find_cli_binary() -> NormalizedPath {
    BUILD_DEBUG_CLI.call_once(|| {
        let status = std::process::Command::new("cargo")
            .args(["build", "-p", "zccache-cli"])
            .status()
            .expect("failed to run cargo build");
        assert!(status.success(), "cargo build -p zccache-cli failed");
    });

    let bin_dir = std::path::Path::new(env!("CARGO_BIN_EXE_zccache-daemon"))
        .parent()
        .unwrap();
    if cfg!(windows) {
        bin_dir.join("zccache.exe").into()
    } else {
        bin_dir.join("zccache").into()
    }
}

/// Build the release CLI binary once across all tests.
/// Release is critical for meson tests because meson probes the compiler ~12
/// times during setup — each invocation goes through the zccache wrapper, so
/// debug overhead (~1.5s/call) dominates.
static BUILD_RELEASE_CLI: Once = Once::new();

fn build_and_find_release_cli() -> NormalizedPath {
    BUILD_RELEASE_CLI.call_once(|| {
        let status = std::process::Command::new("cargo")
            .args(["build", "--release", "-p", "zccache-cli"])
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

fn normalize_units<I, P, Q>(units: I) -> Vec<(NormalizedPath, NormalizedPath)>
where
    I: IntoIterator<Item = (P, Q)>,
    P: Into<NormalizedPath>,
    Q: Into<NormalizedPath>,
{
    units
        .into_iter()
        .map(|(src, obj)| (src.into(), obj.into()))
        .collect()
}

async fn start_daemon(endpoint: &str) -> (JoinHandle<()>, Arc<Notify>) {
    let mut server = DaemonServer::bind(endpoint).unwrap();
    let shutdown = server.shutdown_handle();
    let handle = tokio::spawn(async move {
        server.run(0).await.unwrap();
    });
    (handle, shutdown)
}

async fn get_status(endpoint: &str) -> zccache_monocrate::protocol::DaemonStatus {
    let mut client = zccache_monocrate::ipc::connect(endpoint).await.unwrap();
    client.send(&Request::Status).await.unwrap();
    match client.recv().await.unwrap() {
        Some(Response::Status(s)) => s,
        other => panic!("expected Status, got: {other:?}"),
    }
}

async fn clear_cache(endpoint: &str) {
    let mut client = zccache_monocrate::ipc::connect(endpoint).await.unwrap();
    client.send(&Request::Clear).await.unwrap();
    match client.recv().await.unwrap() {
        Some(Response::Cleared { .. }) => {}
        other => panic!("expected Cleared, got: {other:?}"),
    }
}

// ─── Build execution ─────────────────────────────────────────────────────────

/// Compile all units via the CLI binary in ephemeral mode (what ninja does).
/// Returns a map of source name → object file bytes.
fn build_all_cli(
    cli: &Path,
    clang: &Path,
    endpoint: &str,
    units: &[(NormalizedPath, NormalizedPath)],
    root: &Path,
) -> HashMap<String, Vec<u8>> {
    let clang_str = clang.to_string_lossy().into_owned();
    let cwd = root.to_string_lossy().into_owned();
    let flags = TestProject::compiler_flags();

    for (src, obj) in units {
        let src_str = src.to_string_lossy();
        let obj_str = obj.to_string_lossy();
        let mut args = vec![clang_str.as_str()];
        args.extend_from_slice(&flags);
        args.push(&src_str);
        args.push("-o");
        args.push(&obj_str);

        let output = std::process::Command::new(cli)
            .args(&args)
            .env("ZCCACHE_ENDPOINT", endpoint)
            .env_remove("ZCCACHE_SESSION_ID")
            .current_dir(&cwd)
            .output()
            .unwrap();

        assert!(
            output.status.success(),
            "compile failed for {}: {}",
            src.display(),
            String::from_utf8_lossy(&output.stderr),
        );
    }

    // Read all object files
    let mut objects = HashMap::new();
    for (src, obj) in units {
        let name = src.file_name().unwrap().to_string_lossy().into_owned();
        assert!(obj.exists(), "missing object file: {}", obj.display());
        objects.insert(name, std::fs::read(obj).unwrap());
    }
    objects
}

/// Compile all units via IPC directly (session mode).
/// Returns a vec of (source_name, exit_code, cached).
async fn build_all_ipc(
    endpoint: &str,
    clang: &Path,
    units: &[(NormalizedPath, NormalizedPath)],
    root: &Path,
) -> Vec<(String, i32, bool)> {
    let mut client = zccache_monocrate::ipc::connect(endpoint).await.unwrap();
    let cwd = root.to_string_lossy().into_owned();
    let compiler = clang.to_string_lossy().into_owned();
    let flags = TestProject::compiler_flags();

    // Start session
    client
        .send(&Request::SessionStart {
            client_pid: std::process::id(),
            working_dir: cwd.clone().into(),
            log_file: None,
            track_stats: true,
            journal_path: None,
            profile: false,
        })
        .await
        .unwrap();

    let session_id = match client.recv().await.unwrap() {
        Some(Response::SessionStarted { session_id, .. }) => session_id,
        other => panic!("expected SessionStarted, got: {other:?}"),
    };

    let mut results = Vec::new();
    for (src, obj) in units {
        let name = src.file_name().unwrap().to_string_lossy().into_owned();
        let mut args: Vec<String> = flags.iter().map(|s| s.to_string()).collect();
        args.push(src.to_string_lossy().into_owned());
        args.push("-o".into());
        args.push(obj.to_string_lossy().into_owned());

        client
            .send(&Request::Compile {
                session_id: session_id.clone(),
                args,
                cwd: cwd.clone().into(),
                compiler: compiler.clone().into(),
                env: None,
                stdin: Vec::new(),
            })
            .await
            .unwrap();

        match client.recv().await.unwrap() {
            Some(Response::CompileResult {
                exit_code, cached, ..
            }) => {
                results.push((name, exit_code, cached));
            }
            Some(Response::Error { message }) => panic!("compile error: {message}"),
            other => panic!("expected CompileResult, got: {other:?}"),
        }
    }

    // End session
    client
        .send(&Request::SessionEnd { session_id })
        .await
        .unwrap();
    let _: Option<Response> = client.recv().await.unwrap_or(None);

    results
}

// ═══════════════════════════════════════════════════════════════════════════════
// INTEGRATION TESTS (30 files, light bodies, run by default)
// ═══════════════════════════════════════════════════════════════════════════════

/// Cold build → warm rebuild via CLI ephemeral mode.
///
/// Simulates exactly what ninja does:
///   1. First build: `zccache clang++ -c unit.cpp -o unit.o` for each file → all misses
///   2. `ninja -t clean` (delete all .o files)
///   3. Rebuild: same commands → all cache hits
///   4. Verify output bytes are identical
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore] // integration: spawns clang 30+ times, run with --full
async fn ninja_cold_then_warm_rebuild_cli() {
    let clang = match zccache_monocrate::test_support::find_clang() {
        Some(p) => p,
        None => {
            eprintln!("skipping: clang not found");
            return;
        }
    };

    let cli = find_cli_binary();

    let project = TestProject::integration();
    let file_count = project.source_count;

    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let units = normalize_units(project.generate(root));

    let endpoint = zccache_monocrate::ipc::unique_test_endpoint();
    let (server_handle, shutdown) = start_daemon(&endpoint).await;

    // Clear any persistent artifacts from prior runs
    clear_cache(&endpoint).await;

    // ── Cold build ──────────────────────────────────────────────────
    let cold_objects = build_all_cli(&cli, &clang, &endpoint, &units, root);
    assert_eq!(cold_objects.len(), file_count);
    for (name, data) in &cold_objects {
        assert!(!data.is_empty(), "{name}: object file is empty");
    }

    let status_after_cold = get_status(&endpoint).await;
    eprintln!(
        "After cold: {} compilations, {} hits, {} misses, {} non-cacheable",
        status_after_cold.total_compilations,
        status_after_cold.cache_hits,
        status_after_cold.cache_misses,
        status_after_cold.non_cacheable,
    );
    assert_eq!(
        status_after_cold.cache_misses, file_count as u64,
        "cold build should have {file_count} misses"
    );
    assert_eq!(
        status_after_cold.cache_hits, 0,
        "cold build should have 0 hits"
    );

    // ── Warm rebuild (ninja -t clean + rebuild) ─────────────────────
    TestProject::clean_objects(root);
    for (_, obj) in &units {
        assert!(!obj.exists(), "object should be deleted: {}", obj.display());
    }

    let warm_objects = build_all_cli(&cli, &clang, &endpoint, &units, root);
    assert_eq!(warm_objects.len(), file_count);

    let status_after_warm = get_status(&endpoint).await;
    eprintln!(
        "After warm: {} compilations, {} hits, {} misses, {} non-cacheable",
        status_after_warm.total_compilations,
        status_after_warm.cache_hits,
        status_after_warm.cache_misses,
        status_after_warm.non_cacheable,
    );
    let warm_hits = status_after_warm.cache_hits - status_after_cold.cache_hits;
    assert_eq!(
        warm_hits, file_count as u64,
        "warm rebuild should have {file_count} hits, got {warm_hits}"
    );

    // ── Verify output bytes are identical ───────────────────────────
    for (name, cold_data) in &cold_objects {
        let warm_data = &warm_objects[name];
        assert_eq!(
            cold_data, warm_data,
            "{name}: cached object differs from cold-compiled object"
        );
    }

    shutdown.notify_one();
    server_handle.await.unwrap();
}

/// Cold build → warm rebuild via IPC session mode (no CLI binary needed).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore] // integration: spawns clang 30+ times, run with --full
async fn ninja_cold_then_warm_rebuild_ipc() {
    let clang = match zccache_monocrate::test_support::find_clang() {
        Some(p) => p,
        None => {
            eprintln!("skipping: clang not found");
            return;
        }
    };

    let project = TestProject::integration();

    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let units = normalize_units(project.generate(root));

    let endpoint = zccache_monocrate::ipc::unique_test_endpoint();
    let (server_handle, shutdown) = start_daemon(&endpoint).await;
    clear_cache(&endpoint).await;

    // ── Cold build ──────────────────────────────────────────────────
    let cold_results = build_all_ipc(&endpoint, &clang, &units, root).await;
    for (name, exit_code, cached) in &cold_results {
        assert_eq!(*exit_code, 0, "{name}: compile failed");
        assert!(!cached, "{name}: cold compile should be a miss");
    }

    let cold_objects: HashMap<String, Vec<u8>> = units
        .iter()
        .map(|(src, obj)| {
            let name = src.file_name().unwrap().to_string_lossy().into_owned();
            (name, std::fs::read(obj).unwrap())
        })
        .collect();

    let status = get_status(&endpoint).await;
    eprintln!(
        "After cold: {} hits, {} misses",
        status.cache_hits, status.cache_misses,
    );

    // ── Warm rebuild ────────────────────────────────────────────────
    TestProject::clean_objects(root);

    let warm_results = build_all_ipc(&endpoint, &clang, &units, root).await;
    for (name, exit_code, cached) in &warm_results {
        assert_eq!(*exit_code, 0, "{name}: compile failed");
        assert!(cached, "{name}: warm compile should be a hit");
    }

    // ── Verify output bytes match ───────────────────────────────────
    for (src, obj) in &units {
        let name = src.file_name().unwrap().to_string_lossy().into_owned();
        let warm_data = std::fs::read(obj).unwrap();
        assert_eq!(
            cold_objects[&name], warm_data,
            "{name}: cached object differs from original"
        );
    }

    shutdown.notify_one();
    server_handle.await.unwrap();
}

/// Persistent artifact test: cold build → verify .meta files on disk → restart → verify loaded.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore] // integration: spawns clang 30+ times, run with --full
async fn ninja_persistent_artifacts_survive_restart() {
    let clang = match zccache_monocrate::test_support::find_clang() {
        Some(p) => p,
        None => {
            eprintln!("skipping: clang not found");
            return;
        }
    };

    let project = TestProject::integration();

    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let units = normalize_units(project.generate(root));

    let endpoint = zccache_monocrate::ipc::unique_test_endpoint();
    let (server_handle, shutdown) = start_daemon(&endpoint).await;

    // ── Cold build (don't clear — other tests may be running) ───────
    let cold_results = build_all_ipc(&endpoint, &clang, &units, root).await;
    for (name, exit_code, cached) in &cold_results {
        assert_eq!(*exit_code, 0, "{name}: compile failed");
        assert!(!cached, "{name}: cold compile should be a miss");
    }

    // Check that .meta files were written to the persistent artifact dir
    let artifact_dir = zccache_monocrate::core::config::default_cache_dir().join("artifacts");
    let meta_count_before = std::fs::read_dir(&artifact_dir)
        .map(|entries| {
            entries
                .flatten()
                .filter(|e| e.path().extension().and_then(|ext| ext.to_str()) == Some("meta"))
                .count()
        })
        .unwrap_or(0);
    eprintln!("Before restart: {meta_count_before} .meta files on disk");
    assert!(
        meta_count_before > 0,
        "should have .meta sidecar files on disk"
    );

    // ── Stop daemon ─────────────────────────────────────────────────
    shutdown.notify_one();
    server_handle.await.unwrap();

    // ── Restart daemon on same endpoint ─────────────────────────────
    let (server_handle2, shutdown2) = start_daemon(&endpoint).await;

    let status2 = get_status(&endpoint).await;
    eprintln!(
        "After restart: {} artifacts restored",
        status2.artifact_count
    );
    assert!(
        status2.artifact_count > 0,
        "daemon should restore artifacts from .meta files on restart"
    );

    // ── Rebuild after restart ───────────────────────────────────────
    TestProject::clean_objects(root);
    let warm_results = build_all_ipc(&endpoint, &clang, &units, root).await;

    for (name, exit_code, _cached) in &warm_results {
        assert_eq!(*exit_code, 0, "{name}: compile failed after restart");
    }

    for (_, obj) in &units {
        let data = std::fs::read(obj).unwrap();
        assert!(
            !data.is_empty(),
            "object should be non-empty: {}",
            obj.display()
        );
    }

    shutdown2.notify_one();
    server_handle2.await.unwrap();
}

/// Header modification invalidates cache for all files that include it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore] // integration: spawns clang 30+ times, run with --full
async fn ninja_header_change_invalidates_dependents() {
    let clang = match zccache_monocrate::test_support::find_clang() {
        Some(p) => p,
        None => {
            eprintln!("skipping: clang not found");
            return;
        }
    };

    let project = TestProject::integration();

    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let units = normalize_units(project.generate(root));

    let endpoint = zccache_monocrate::ipc::unique_test_endpoint();
    let (server_handle, shutdown) = start_daemon(&endpoint).await;
    clear_cache(&endpoint).await;

    // ── Cold build ──────────────────────────────────────────────────
    let cold_results = build_all_ipc(&endpoint, &clang, &units, root).await;
    for (name, exit_code, _) in &cold_results {
        assert_eq!(*exit_code, 0, "{name}: compile failed");
    }

    // ── Modify a shared header ──────────────────────────────────────
    let header = root.join("include").join("shared_0.h");
    let mut content = std::fs::read_to_string(&header).unwrap();
    content.push_str("\n// Modified to invalidate cache\n");
    std::thread::sleep(std::time::Duration::from_millis(100));
    std::fs::write(&header, &content).unwrap();

    // ── Rebuild (should NOT serve stale cached objects) ─────────────
    TestProject::clean_objects(root);
    let rebuild_results = build_all_ipc(&endpoint, &clang, &units, root).await;

    for (name, exit_code, _) in &rebuild_results {
        assert_eq!(*exit_code, 0, "{name}: compile failed after header change");
    }

    for (_, obj) in &units {
        let data = std::fs::read(obj).unwrap();
        assert!(!data.is_empty(), "object file should not be empty");
    }

    shutdown.notify_one();
    server_handle.await.unwrap();
}

/// Concurrent compilation: multiple files compiled "simultaneously" via IPC.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore] // integration: spawns clang 30+ times concurrently, run with --full
async fn ninja_concurrent_cold_build() {
    let clang = match zccache_monocrate::test_support::find_clang() {
        Some(p) => p,
        None => {
            eprintln!("skipping: clang not found");
            return;
        }
    };

    let project = TestProject::integration();
    let file_count = project.source_count;

    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let units = normalize_units(project.generate(root));

    let endpoint = zccache_monocrate::ipc::unique_test_endpoint();
    let (server_handle, shutdown) = start_daemon(&endpoint).await;
    clear_cache(&endpoint).await;

    let flags = TestProject::compiler_flags();

    // Start a session
    let mut client = zccache_monocrate::ipc::connect(&endpoint).await.unwrap();
    client
        .send(&Request::SessionStart {
            client_pid: std::process::id(),
            working_dir: root.to_string_lossy().into_owned().into(),
            log_file: None,
            track_stats: false,
            journal_path: None,
            profile: false,
        })
        .await
        .unwrap();
    let session_id = match client.recv().await.unwrap() {
        Some(Response::SessionStarted { session_id, .. }) => session_id,
        other => panic!("expected SessionStarted, got: {other:?}"),
    };
    drop(client);

    // Spawn concurrent compile tasks
    let compiler = clang.to_string_lossy().into_owned();
    let mut handles = Vec::new();
    for (src, obj) in units.clone() {
        let ep = endpoint.clone();
        let cwd = root.to_string_lossy().into_owned();
        let fl: Vec<String> = flags.iter().map(|s| s.to_string()).collect();
        let comp = compiler.clone();
        let sid = session_id.clone();
        handles.push(tokio::spawn(async move {
            let mut conn = zccache_monocrate::ipc::connect(&ep).await.unwrap();
            let mut args = fl;
            args.push(src.to_string_lossy().into_owned());
            args.push("-o".into());
            args.push(obj.to_string_lossy().into_owned());

            conn.send(&Request::Compile {
                session_id: sid,
                args,
                cwd: cwd.into(),
                compiler: comp.into(),
                env: None,
                stdin: Vec::new(),
            })
            .await
            .unwrap();

            let name = src.file_name().unwrap().to_string_lossy().into_owned();
            match conn.recv().await.unwrap() {
                Some(Response::CompileResult {
                    exit_code, cached, ..
                }) => (name, exit_code, cached),
                Some(Response::Error { message }) => panic!("{name}: {message}"),
                other => panic!("{name}: unexpected response: {other:?}"),
            }
        }));
    }

    let mut results = Vec::new();
    for handle in handles {
        results.push(handle.await.unwrap());
    }

    for (name, exit_code, _cached) in &results {
        assert_eq!(*exit_code, 0, "{name}: concurrent compile failed");
    }

    for (_, obj) in &units {
        let data = std::fs::read(obj).unwrap();
        assert!(
            !data.is_empty(),
            "object should be non-empty: {}",
            obj.display()
        );
    }

    let status = get_status(&endpoint).await;
    eprintln!(
        "Concurrent cold: {} total, {} hits, {} misses, {} non-cacheable",
        status.total_compilations, status.cache_hits, status.cache_misses, status.non_cacheable,
    );
    assert!(
        status.total_compilations >= file_count as u64,
        "should have at least {file_count} compilations"
    );

    shutdown.notify_one();
    server_handle.await.unwrap();
}

/// `zccache clear` resets the cache, forcing cold misses on next build.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore] // integration: spawns clang 30+ times, run with --full
async fn ninja_clear_forces_cold_rebuild() {
    let clang = match zccache_monocrate::test_support::find_clang() {
        Some(p) => p,
        None => {
            eprintln!("skipping: clang not found");
            return;
        }
    };

    let project = TestProject::integration();

    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let units = normalize_units(project.generate(root));

    let endpoint = zccache_monocrate::ipc::unique_test_endpoint();
    let (server_handle, shutdown) = start_daemon(&endpoint).await;
    clear_cache(&endpoint).await;

    // ── Cold build ──────────────────────────────────────────────────
    let cold_results = build_all_ipc(&endpoint, &clang, &units, root).await;
    for (name, exit_code, cached) in &cold_results {
        assert_eq!(*exit_code, 0, "{name}: compile failed");
        assert!(!cached, "{name}: cold compile should be a miss");
    }

    // ── Warm rebuild (should hit) ───────────────────────────────────
    TestProject::clean_objects(root);
    let warm_results = build_all_ipc(&endpoint, &clang, &units, root).await;
    for (name, exit_code, cached) in &warm_results {
        assert_eq!(*exit_code, 0, "{name}: compile failed");
        assert!(cached, "{name}: warm compile should be a hit");
    }

    // ── Clear cache ─────────────────────────────────────────────────
    clear_cache(&endpoint).await;

    let status = get_status(&endpoint).await;
    assert_eq!(
        status.artifact_count, 0,
        "cache should be empty after clear"
    );

    // ── Rebuild after clear (should miss again) ─────────────────────
    TestProject::clean_objects(root);
    let post_clear_results = build_all_ipc(&endpoint, &clang, &units, root).await;
    for (name, exit_code, cached) in &post_clear_results {
        assert_eq!(*exit_code, 0, "{name}: compile failed after clear");
        assert!(!cached, "{name}: should miss after cache clear");
    }

    shutdown.notify_one();
    server_handle.await.unwrap();
}

// ═══════════════════════════════════════════════════════════════════════════════
// STRESS / BENCHMARK TESTS (large projects, run with --ignored)
// ═══════════════════════════════════════════════════════════════════════════════

/// Stress test: 250 files with heavy bodies, cold + warm + warm.
///
/// Run:  soldr cargo test -p zccache-daemon --test ninja_rebuild_test -- stress_large_project --ignored --nocapture
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore]
async fn stress_large_project_cold_warm() {
    let clang = match zccache_monocrate::test_support::find_clang() {
        Some(p) => p,
        None => {
            eprintln!("skipping: clang not found");
            return;
        }
    };

    let project = TestProject::stress();
    let file_count = project.source_count;
    eprintln!(
        "Stress test: {} source files, {} shared headers, {} private headers",
        project.source_count, project.header_count, project.private_header_count,
    );

    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let units = normalize_units(project.generate(root));
    eprintln!("Generated {} compilation units", units.len());

    let endpoint = zccache_monocrate::ipc::unique_test_endpoint();
    let (server_handle, shutdown) = start_daemon(&endpoint).await;
    clear_cache(&endpoint).await;

    // ── Cold build ──────────────────────────────────────────────────
    let t0 = std::time::Instant::now();
    let cold_results = build_all_ipc(&endpoint, &clang, &units, root).await;
    let cold_ms = t0.elapsed().as_millis();
    let cold_failures: Vec<_> = cold_results
        .iter()
        .filter(|(_, code, _)| *code != 0)
        .collect();
    assert!(
        cold_failures.is_empty(),
        "cold compile failures: {cold_failures:?}"
    );
    let cold_misses = cold_results.iter().filter(|(_, _, c)| !c).count();
    eprintln!("Cold build: {cold_ms}ms ({file_count} files, {cold_misses} misses)");

    let status = get_status(&endpoint).await;
    eprintln!(
        "  Artifacts: {}, Metadata: {}, Dep graph contexts: {}",
        status.artifact_count, status.metadata_entries, status.dep_graph_contexts,
    );

    // ── Warm rebuild #1 ─────────────────────────────────────────────
    TestProject::clean_objects(root);
    let t1 = std::time::Instant::now();
    let warm1_results = build_all_ipc(&endpoint, &clang, &units, root).await;
    let warm1_ms = t1.elapsed().as_millis();
    let warm1_hits = warm1_results.iter().filter(|(_, _, c)| *c).count();
    let warm1_misses = warm1_results.iter().filter(|(_, _, c)| !c).count();
    eprintln!("Warm #1:    {warm1_ms}ms ({warm1_hits} hits, {warm1_misses} misses)");

    assert_eq!(
        warm1_hits, file_count,
        "warm rebuild should hit all {file_count} files"
    );

    // ── Warm rebuild #2 (should be even faster — fast-hit path) ─────
    TestProject::clean_objects(root);
    let t2 = std::time::Instant::now();
    let warm2_results = build_all_ipc(&endpoint, &clang, &units, root).await;
    let warm2_ms = t2.elapsed().as_millis();
    let warm2_hits = warm2_results.iter().filter(|(_, _, c)| *c).count();
    eprintln!("Warm #2:    {warm2_ms}ms ({warm2_hits} hits, ultra-fast path)");

    assert_eq!(
        warm2_hits, file_count,
        "second warm rebuild should hit all {file_count} files"
    );

    // Print speedup
    if cold_ms > 0 {
        eprintln!(
            "\nSpeedup: {:.1}x (cold {cold_ms}ms → warm {warm2_ms}ms)",
            cold_ms as f64 / warm2_ms.max(1) as f64,
        );
    }

    shutdown.notify_one();
    server_handle.await.unwrap();
}

/// Benchmark: 100 files, medium bodies, cold + 5 warm iterations.
/// Prints per-iteration timing for trend analysis.
///
/// Run:  soldr cargo test -p zccache-daemon --test ninja_rebuild_test -- bench_medium --ignored --nocapture
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore]
async fn bench_medium_project_warm_iterations() {
    let clang = match zccache_monocrate::test_support::find_clang() {
        Some(p) => p,
        None => {
            eprintln!("skipping: clang not found");
            return;
        }
    };

    let project = TestProject::benchmark();
    let file_count = project.source_count;
    eprintln!("Benchmark: {file_count} files, medium bodies, 5 warm iterations");

    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let units = normalize_units(project.generate(root));

    let endpoint = zccache_monocrate::ipc::unique_test_endpoint();
    let (server_handle, shutdown) = start_daemon(&endpoint).await;
    clear_cache(&endpoint).await;

    // ── Cold build ──────────────────────────────────────────────────
    let t0 = std::time::Instant::now();
    let cold_results = build_all_ipc(&endpoint, &clang, &units, root).await;
    let cold_ms = t0.elapsed().as_millis();
    for (name, exit_code, _) in &cold_results {
        assert_eq!(*exit_code, 0, "{name}: compile failed");
    }
    eprintln!("Cold:    {cold_ms:>6}ms");

    // ── Warm iterations ─────────────────────────────────────────────
    let mut warm_times = Vec::new();
    for iter in 1..=5 {
        TestProject::clean_objects(root);
        let t = std::time::Instant::now();
        let results = build_all_ipc(&endpoint, &clang, &units, root).await;
        let ms = t.elapsed().as_millis();
        let hits = results.iter().filter(|(_, _, c)| *c).count();
        assert_eq!(hits, file_count, "iteration {iter}: expected all hits");
        eprintln!("Warm #{iter}: {ms:>6}ms ({hits} hits)");
        warm_times.push(ms);
    }

    let avg_warm = warm_times.iter().sum::<u128>() / warm_times.len() as u128;
    let min_warm = *warm_times.iter().min().unwrap();
    let max_warm = *warm_times.iter().max().unwrap();
    eprintln!("\nWarm avg: {avg_warm}ms, min: {min_warm}ms, max: {max_warm}ms");
    eprintln!(
        "Speedup: {:.1}x (cold {cold_ms}ms → warm avg {avg_warm}ms)",
        cold_ms as f64 / avg_warm.max(1) as f64,
    );

    shutdown.notify_one();
    server_handle.await.unwrap();
}

// ═══════════════════════════════════════════════════════════════════════════════
// MESON + NINJA INTEGRATION TESTS (run with --ignored, requires meson + ninja)
// ═══════════════════════════════════════════════════════════════════════════════

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

/// Full meson+ninja cold → warm rebuild through zccache.
///
/// This is the end-to-end test that exercises the real build system pipeline:
///   1. Generate a self-contained meson C++ project
///   2. `meson setup` with zccache as the compiler wrapper
///   3. `ninja` cold build (all cache misses)
///   4. `ninja -t clean` + `ninja` warm rebuild (all cache hits)
///   5. Verify warm is significantly faster than cold
///
/// Run:  soldr cargo test -p zccache-daemon --test ninja_rebuild_test -- meson_ninja_cold --ignored --nocapture
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn meson_ninja_cold_then_warm_rebuild() {
    let clang = match zccache_monocrate::test_support::find_clang() {
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

    let endpoint = zccache_monocrate::ipc::unique_test_endpoint();
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
/// Run:  soldr cargo test -p zccache-daemon --test ninja_rebuild_test -- meson_ninja_bench --ignored --nocapture
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn meson_ninja_bench_warm_iterations() {
    let clang = match zccache_monocrate::test_support::find_clang() {
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

    let endpoint = zccache_monocrate::ipc::unique_test_endpoint();
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
