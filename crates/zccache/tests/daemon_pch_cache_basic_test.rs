//! Integration tests for precompiled header (PCH) caching — basic scenarios.
//!
//! Verifies:
//! - Compilations using `-include-pch` are cacheable
//! - PCH content is part of the cache key (different PCH = different cache entry)
//! - PCH generation (`-x c++-header`) is cacheable through the daemon
//!
//! Adversarial sub-header / chained / build-dir-separation / spurious-output
//! scenarios live in `daemon_pch_cache_invalidation_test.rs`.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::panic_in_result_fn, clippy::unwrap_in_result)]

use zccache::daemon::DaemonServer;
use zccache::protocol::{Request, Response};

/// Platform-correct client connection type.
#[cfg(unix)]
type ClientConn = zccache::ipc::IpcConnection;
#[cfg(windows)]
type ClientConn = zccache::ipc::IpcClientConnection;

async fn start_daemon() -> (
    String,
    tokio::task::JoinHandle<()>,
    std::sync::Arc<tokio::sync::Notify>,
) {
    let endpoint = zccache::ipc::unique_test_endpoint();
    let mut server = DaemonServer::bind(&endpoint).unwrap();
    let shutdown = server.shutdown_handle();
    let handle = tokio::spawn(async move { server.run(0).await.unwrap() });
    (endpoint, handle, shutdown)
}

async fn start_session(client: &mut ClientConn, cwd: &str, log_file: &str) -> String {
    client
        .send(&Request::SessionStart {
            client_pid: std::process::id(),
            working_dir: cwd.to_string().into(),
            log_file: Some(log_file.to_string().into()),
            track_stats: false,
            journal_path: None,
            profile: false,
            private_daemon: None,
        })
        .await
        .unwrap();
    match client.recv().await.unwrap() {
        Some(Response::SessionStarted { session_id, .. }) => session_id,
        other => panic!("expected SessionStarted, got: {other:?}"),
    }
}

async fn compile_raw(
    client: &mut ClientConn,
    session_id: &str,
    compiler: &str,
    args: Vec<String>,
    cwd: &str,
) -> (i32, bool, Vec<u8>) {
    client
        .send(&Request::Compile {
            session_id: session_id.to_string(),
            args,
            cwd: cwd.to_string().into(),
            compiler: compiler.to_string().into(),
            env: None,
            stdin: Vec::new(),
        })
        .await
        .unwrap();

    match client.recv().await.unwrap() {
        Some(Response::CompileResult {
            exit_code,
            cached,
            stderr,
            ..
        }) => (exit_code, cached, (*stderr).clone()),
        Some(Response::Error { message }) => panic!("compile error: {message}"),
        other => panic!("expected CompileResult, got: {other:?}"),
    }
}

/// Test: compilation with `-include-pch` is cacheable and cache key includes PCH content.
///
/// 1. Generate PCH through daemon → cache miss
/// 2. Compile source using PCH → cache miss
/// 3. Compile again → cache hit
/// 4. Modify header → regenerate PCH through daemon (content changes, daemon invalidates output)
/// 5. Compile with new PCH → cache miss (PCH content hash changed)
#[tokio::test]
#[ignore] // integration: spawns clang + watcher sleeps, run with --full
async fn pch_usage_is_cacheable() {
    let clang = match zccache::test_support::find_clang() {
        Some(p) => p,
        None => {
            eprintln!("skipping test: clang not found");
            return;
        }
    };

    let tmp = tempfile::tempdir().unwrap();
    let cwd = tmp.path().to_string_lossy().into_owned();
    // Put the log in a subdirectory so its writes don't keep resetting
    // the watcher settle buffer for the source directory.
    let log_dir = tmp.path().join("logs");
    std::fs::create_dir_all(&log_dir).unwrap();
    let log = log_dir.join("session.log");
    let compiler = clang.to_string_lossy().into_owned();

    // Create header and source files
    let header = tmp.path().join("pch.h");
    let pch_file = tmp.path().join("pch.h.pch");
    let source = tmp.path().join("main.cpp");
    let obj = tmp.path().join("main.o");

    std::fs::write(
        &header,
        "#ifndef PCH_H\n#define PCH_H\n#define PCH_VALUE 42\n#endif\n",
    )
    .unwrap();

    std::fs::write(&source, "int main() { return PCH_VALUE; }\n").unwrap();

    let (endpoint, server_handle, shutdown) = start_daemon().await;
    let mut client = zccache::ipc::connect(&endpoint).await.unwrap();
    let sid = start_session(&mut client, &cwd, &log.to_string_lossy()).await;

    // Generate PCH through the daemon (now cacheable with -x c++-header).
    // The daemon's apply_changes on the output invalidates pch.h.pch
    // so subsequent compilations see it immediately.
    let (exit_code, _, stderr) = compile_raw(
        &mut client,
        &sid,
        &compiler,
        vec![
            "-x".to_string(),
            "c++-header".to_string(),
            "-c".to_string(),
            header.to_string_lossy().into_owned(),
            "-o".to_string(),
            pch_file.to_string_lossy().into_owned(),
        ],
        &cwd,
    )
    .await;
    let stderr_str = String::from_utf8_lossy(&stderr);
    assert_eq!(exit_code, 0, "PCH generation failed: {stderr_str}");
    assert!(pch_file.exists(), "PCH file should be created");
    let original_pch_data = std::fs::read(&pch_file).unwrap();

    // Compile with PCH — should be a cache miss
    let (exit_code, cached, stderr) = compile_raw(
        &mut client,
        &sid,
        &compiler,
        vec![
            "-c".to_string(),
            source.to_string_lossy().into_owned(),
            "-o".to_string(),
            obj.to_string_lossy().into_owned(),
            "-include-pch".to_string(),
            pch_file.to_string_lossy().into_owned(),
        ],
        &cwd,
    )
    .await;

    let stderr_str = String::from_utf8_lossy(&stderr);
    assert_eq!(
        exit_code, 0,
        "compile with PCH should succeed. stderr: {stderr_str}"
    );
    assert!(!cached, "first compile with PCH should be a cache miss");
    assert!(obj.exists(), "object file should be produced");
    let first_obj = std::fs::read(&obj).unwrap();

    // Delete .o and compile again — should be a cache hit
    std::fs::remove_file(&obj).unwrap();
    let (exit_code, cached, _) = compile_raw(
        &mut client,
        &sid,
        &compiler,
        vec![
            "-c".to_string(),
            source.to_string_lossy().into_owned(),
            "-o".to_string(),
            obj.to_string_lossy().into_owned(),
            "-include-pch".to_string(),
            pch_file.to_string_lossy().into_owned(),
        ],
        &cwd,
    )
    .await;
    assert_eq!(exit_code, 0, "second compile should succeed");
    assert!(cached, "second compile with same PCH should be a CACHE HIT");
    let second_obj = std::fs::read(&obj).unwrap();
    assert_eq!(first_obj, second_obj, "cached .o should match original");

    // Verify log shows miss then hit
    let log_text = std::fs::read_to_string(&log).unwrap();
    eprintln!("=== session log ===\n{log_text}");
    assert!(log_text.contains("[MISS]"), "log should show MISS");
    assert!(log_text.contains("[HIT]"), "log should show HIT");

    // Now modify header → regenerate PCH through the daemon → compile again.
    // Sleep BEFORE write ensures mtime differs from previous state (same
    // pattern as ninja_rebuild_test line 486).
    std::thread::sleep(std::time::Duration::from_millis(100));
    // Use a dramatically different header to guarantee the PCH binary changes.
    std::fs::write(
        &header,
        "#ifndef PCH_H\n#define PCH_H\n\
         #define PCH_VALUE 99\n\
         typedef struct { int a; int b; int c; double d; } ExtraStruct;\n\
         inline int extra_fn(void) { return 123; }\n\
         #endif\n",
    )
    .unwrap();
    // Let watcher settle buffer process the header change event.
    // Needs enough time for the 50ms settle window plus OS watcher latency.
    std::thread::sleep(std::time::Duration::from_millis(500));

    // Regenerate PCH through the daemon — the daemon will apply_changes on
    // the output (pch.h.pch), so subsequent compilations see it immediately.
    let (exit_code, cached, _) = compile_raw(
        &mut client,
        &sid,
        &compiler,
        vec![
            "-x".to_string(),
            "c++-header".to_string(),
            "-c".to_string(),
            header.to_string_lossy().into_owned(),
            "-o".to_string(),
            pch_file.to_string_lossy().into_owned(),
        ],
        &cwd,
    )
    .await;
    assert_eq!(
        exit_code, 0,
        "PCH regeneration through daemon should succeed"
    );
    assert!(
        !cached,
        "PCH regen with changed header should be a cache miss"
    );
    let regenerated_pch_data = std::fs::read(&pch_file).unwrap();
    assert_ne!(
        original_pch_data, regenerated_pch_data,
        "PCH content should differ after header change"
    );

    std::fs::remove_file(&obj).unwrap();
    let (exit_code, cached, _) = compile_raw(
        &mut client,
        &sid,
        &compiler,
        vec![
            "-c".to_string(),
            source.to_string_lossy().into_owned(),
            "-o".to_string(),
            obj.to_string_lossy().into_owned(),
            "-include-pch".to_string(),
            pch_file.to_string_lossy().into_owned(),
        ],
        &cwd,
    )
    .await;
    assert_eq!(exit_code, 0, "compile with updated PCH should succeed");

    // PCH content changed → should be a cache miss (force_includes are hashed).
    assert!(
        !cached,
        "compile with changed PCH should be a cache MISS (PCH content hash changed)"
    );
    let third_obj = std::fs::read(&obj).unwrap();
    assert_ne!(
        first_obj, third_obj,
        "different PCH_VALUE should produce different .o"
    );

    shutdown.notify_one();
    server_handle.await.unwrap();
}

/// Test: PCH generation via daemon is cacheable — second generation is a cache hit.
#[tokio::test]
#[ignore] // integration: spawns clang, run with --full
async fn pch_generation_is_cacheable() {
    let clang = match zccache::test_support::find_clang() {
        Some(p) => p,
        None => {
            eprintln!("skipping test: clang not found");
            return;
        }
    };

    let tmp = tempfile::tempdir().unwrap();
    let cwd = tmp.path().to_string_lossy().into_owned();
    let log = tmp.path().join("session.log");
    let compiler = clang.to_string_lossy().into_owned();

    let header = tmp.path().join("gen.h");
    let pch_file = tmp.path().join("gen.h.pch");

    std::fs::write(&header, "#define GEN_VALUE 1\n").unwrap();

    let (endpoint, server_handle, shutdown) = start_daemon().await;
    let mut client = zccache::ipc::connect(&endpoint).await.unwrap();
    let sid = start_session(&mut client, &cwd, &log.to_string_lossy()).await;

    let pch_args = || {
        vec![
            "-x".to_string(),
            "c++-header".to_string(),
            "-c".to_string(),
            header.to_string_lossy().into_owned(),
            "-o".to_string(),
            pch_file.to_string_lossy().into_owned(),
        ]
    };

    // First PCH generation through the daemon — cache miss
    let (exit_code, cached, stderr) =
        compile_raw(&mut client, &sid, &compiler, pch_args(), &cwd).await;

    let stderr_str = String::from_utf8_lossy(&stderr);
    assert_eq!(
        exit_code, 0,
        "PCH generation through daemon should succeed. stderr: {stderr_str}"
    );
    assert!(!cached, "first PCH generation should be a cache miss");
    assert!(pch_file.exists(), "PCH file should be created");
    let first_pch = std::fs::read(&pch_file).unwrap();

    // Delete PCH and regenerate — should be a cache hit
    std::fs::remove_file(&pch_file).unwrap();
    let (exit_code, cached, _) = compile_raw(&mut client, &sid, &compiler, pch_args(), &cwd).await;
    assert_eq!(exit_code, 0, "second PCH generation should succeed");
    assert!(cached, "second PCH generation should be a CACHE HIT");
    let second_pch = std::fs::read(&pch_file).unwrap();
    assert_eq!(first_pch, second_pch, "cached PCH should match original");

    let log_text = std::fs::read_to_string(&log).unwrap();
    eprintln!("=== PCH generation log ===\n{log_text}");

    shutdown.notify_one();
    server_handle.await.unwrap();
}
