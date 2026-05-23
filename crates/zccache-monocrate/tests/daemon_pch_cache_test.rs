//! Integration tests for precompiled header (PCH) caching.
//!
//! Verifies:
//! - Compilations using `-include-pch` are cacheable
//! - PCH content is part of the cache key (different PCH = different cache entry)
//! - PCH generation (`-x c-header`) passes through as non-cacheable
//! - Sub-header changes (headers included BY the PCH source header) correctly
//!   invalidate both PCH generation and consuming compilations
//! - Build-directory separation (PCH binary in a different dir than source)
//!   doesn't cause stale cache hits

use zccache_monocrate::daemon::DaemonServer;
use zccache_monocrate::protocol::{Request, Response};

/// Platform-correct client connection type.
#[cfg(unix)]
type ClientConn = zccache_monocrate::ipc::IpcConnection;
#[cfg(windows)]
type ClientConn = zccache_monocrate::ipc::IpcClientConnection;

async fn start_daemon() -> (
    String,
    tokio::task::JoinHandle<()>,
    std::sync::Arc<tokio::sync::Notify>,
) {
    let endpoint = zccache_monocrate::ipc::unique_test_endpoint();
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
    let clang = match zccache_monocrate::test_support::find_clang() {
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
    let mut client = zccache_monocrate::ipc::connect(&endpoint).await.unwrap();
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
    let clang = match zccache_monocrate::test_support::find_clang() {
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
    let mut client = zccache_monocrate::ipc::connect(&endpoint).await.unwrap();
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

/// Adversarial test: modifying a SUB-HEADER included by the PCH source header
/// (not the PCH source header itself) must invalidate both PCH generation and
/// all consuming compilations.
///
/// This is the scenario reported by users: a header that is *part of* the PCH
/// changes, and the cache must detect this. The PCH source header (`pch.h`)
/// content does NOT change — only `sub.h` (which `pch.h` includes) changes.
///
/// Covers:
/// 1. PCH gen with sub-headers → cache miss (first run)
/// 2. Compile with PCH → cache miss → cache hit
/// 3. Modify sub-header → PCH regen → cache miss (sub.h hash changed)
/// 4. Compile with new PCH → cache miss (artifact key changed via sub.h)
/// 5. Compile again → cache hit (stable)
#[tokio::test]
#[ignore] // integration: spawns clang + watcher sleeps, run with --full
async fn pch_sub_header_change_invalidates_cache() {
    let clang = match zccache_monocrate::test_support::find_clang() {
        Some(p) => p,
        None => {
            eprintln!("skipping test: clang not found");
            return;
        }
    };

    let tmp = tempfile::tempdir().unwrap();
    let cwd = tmp.path().to_string_lossy().into_owned();
    let log_dir = tmp.path().join("logs");
    std::fs::create_dir_all(&log_dir).unwrap();
    let log = log_dir.join("session.log");
    let compiler = clang.to_string_lossy().into_owned();

    // ── Set up files: pch.h includes sub.h ─────────────────────
    let sub_header = tmp.path().join("sub.h");
    let pch_header = tmp.path().join("pch.h");
    let pch_file = tmp.path().join("pch.h.pch");
    let source = tmp.path().join("main.cpp");
    let obj = tmp.path().join("main.o");

    // sub.h defines a value; pch.h includes it but adds nothing else.
    std::fs::write(
        &sub_header,
        "#ifndef SUB_H\n#define SUB_H\n#define SUB_VALUE 42\n#endif\n",
    )
    .unwrap();
    // pch.h is a thin wrapper — its own content NEVER changes.
    std::fs::write(
        &pch_header,
        "#ifndef PCH_H\n#define PCH_H\n#include \"sub.h\"\n#endif\n",
    )
    .unwrap();
    std::fs::write(&source, "int main() { return SUB_VALUE; }\n").unwrap();

    let (endpoint, server_handle, shutdown) = start_daemon().await;
    let mut client = zccache_monocrate::ipc::connect(&endpoint).await.unwrap();
    let sid = start_session(&mut client, &cwd, &log.to_string_lossy()).await;

    let pch_gen_args = || {
        vec![
            "-x".to_string(),
            "c++-header".to_string(),
            "-c".to_string(),
            pch_header.to_string_lossy().into_owned(),
            "-o".to_string(),
            pch_file.to_string_lossy().into_owned(),
        ]
    };
    let compile_args = || {
        vec![
            "-c".to_string(),
            source.to_string_lossy().into_owned(),
            "-o".to_string(),
            obj.to_string_lossy().into_owned(),
            "-include-pch".to_string(),
            pch_file.to_string_lossy().into_owned(),
        ]
    };

    // ── Step 1: Generate PCH (cold cache) ──────────────────────
    let (exit_code, _, stderr) =
        compile_raw(&mut client, &sid, &compiler, pch_gen_args(), &cwd).await;
    let stderr_str = String::from_utf8_lossy(&stderr);
    assert_eq!(exit_code, 0, "PCH generation failed: {stderr_str}");

    // ── Step 2: Compile with PCH → cache miss ──────────────────
    let (exit_code, cached, stderr) =
        compile_raw(&mut client, &sid, &compiler, compile_args(), &cwd).await;
    let stderr_str = String::from_utf8_lossy(&stderr);
    assert_eq!(exit_code, 0, "compile with PCH failed: {stderr_str}");
    assert!(!cached, "first compile should be a cache miss");
    let first_obj = std::fs::read(&obj).unwrap();

    // ── Step 3: Compile again → cache hit ──────────────────────
    std::fs::remove_file(&obj).unwrap();
    let (exit_code, cached, _) =
        compile_raw(&mut client, &sid, &compiler, compile_args(), &cwd).await;
    assert_eq!(exit_code, 0);
    assert!(cached, "second compile should be a cache hit");

    // ── Step 4: Modify SUB-HEADER ONLY (pch.h content unchanged) ────
    std::thread::sleep(std::time::Duration::from_millis(100));
    std::fs::write(
        &sub_header,
        "#ifndef SUB_H\n#define SUB_H\n#define SUB_VALUE 99\n\
         typedef struct { int x; int y; } SubExtra;\n\
         #endif\n",
    )
    .unwrap();
    // Verify pch.h is unchanged
    let pch_content = std::fs::read_to_string(&pch_header).unwrap();
    assert!(
        pch_content.contains("#include \"sub.h\""),
        "pch.h should be unchanged"
    );

    // Wait for watcher to process the sub.h change.
    // Windows watcher latency can exceed 500ms under load.
    std::thread::sleep(std::time::Duration::from_millis(2000));

    // ── Step 5: Regenerate PCH → must be cache miss ────────────
    let (exit_code, cached, _) =
        compile_raw(&mut client, &sid, &compiler, pch_gen_args(), &cwd).await;
    assert_eq!(exit_code, 0, "PCH regen should succeed");
    assert!(
        !cached,
        "PCH regen after sub-header change MUST be a cache miss"
    );

    // ── Step 6: Compile with new PCH → must be cache miss ──────
    std::fs::remove_file(&obj).unwrap();
    let (exit_code, cached, _) =
        compile_raw(&mut client, &sid, &compiler, compile_args(), &cwd).await;
    assert_eq!(exit_code, 0, "compile with updated PCH should succeed");
    assert!(
        !cached,
        "compile after sub-header change MUST be a cache miss (not a stale hit)"
    );
    let second_obj = std::fs::read(&obj).unwrap();
    assert_ne!(
        first_obj, second_obj,
        "different SUB_VALUE should produce different .o"
    );

    // ── Step 7: Compile again → cache hit (stable) ─────────────
    std::fs::remove_file(&obj).unwrap();
    let (exit_code, cached, _) =
        compile_raw(&mut client, &sid, &compiler, compile_args(), &cwd).await;
    assert_eq!(exit_code, 0);
    assert!(
        cached,
        "re-compile with unchanged PCH should be a cache hit"
    );

    // ── Step 8: PCH regen with no changes → cache hit ──────────
    let pch_before = std::fs::read(&pch_file).unwrap();
    let (exit_code, cached, _) =
        compile_raw(&mut client, &sid, &compiler, pch_gen_args(), &cwd).await;
    assert_eq!(exit_code, 0);
    assert!(
        cached,
        "PCH regen with no further changes should be a cache hit"
    );
    let pch_after = std::fs::read(&pch_file).unwrap();
    assert_eq!(
        pch_before, pch_after,
        "cached PCH should be identical to previous"
    );

    let log_text = std::fs::read_to_string(&log).unwrap();
    eprintln!("=== sub-header test log ===\n{log_text}");

    shutdown.notify_one();
    server_handle.await.unwrap();
}

/// Adversarial test: PCH binary in a separate BUILD directory (not sibling to
/// source header). This tests the `pch_source_header()` resolution when the
/// PCH lives at e.g. `.build/pch.h.pch` while the source is at `src/pch.h`.
///
/// If `pch_source_header()` can't find the source header, it falls back to
/// hashing the PCH binary directly. Since PCH binaries embed timestamps
/// (non-deterministic), this can cause spurious cache misses or, worse,
/// stale hits if the binary bytes happen to be the same.
///
/// The critical assertion: after modifying a sub-header, the compilation
/// using the PCH MUST get a cache miss even when pch_source_header fails.
#[tokio::test]
#[ignore] // integration: spawns clang + watcher sleeps, run with --full
async fn pch_build_dir_separation_sub_header_change() {
    let clang = match zccache_monocrate::test_support::find_clang() {
        Some(p) => p,
        None => {
            eprintln!("skipping test: clang not found");
            return;
        }
    };

    let tmp = tempfile::tempdir().unwrap();
    let log_dir = tmp.path().join("logs");
    std::fs::create_dir_all(&log_dir).unwrap();
    let log = log_dir.join("session.log");
    let compiler = clang.to_string_lossy().into_owned();

    // Source in src/, PCH output in build/ — different directories.
    let src_dir = tmp.path().join("src");
    let build_dir = tmp.path().join("build");
    std::fs::create_dir_all(&src_dir).unwrap();
    std::fs::create_dir_all(&build_dir).unwrap();

    let cwd = tmp.path().to_string_lossy().into_owned();

    let sub_header = src_dir.join("sub.h");
    let pch_header = src_dir.join("pch.h");
    // PCH binary is in the BUILD dir, not next to source
    let pch_file = build_dir.join("pch.h.pch");
    let source = src_dir.join("main.cpp");
    let obj = build_dir.join("main.o");

    std::fs::write(
        &sub_header,
        "#ifndef SUB_H\n#define SUB_H\n#define SUB_VALUE 42\n#endif\n",
    )
    .unwrap();
    std::fs::write(
        &pch_header,
        "#ifndef PCH_H\n#define PCH_H\n#include \"sub.h\"\n#endif\n",
    )
    .unwrap();
    std::fs::write(&source, "int main() { return SUB_VALUE; }\n").unwrap();

    let (endpoint, server_handle, shutdown) = start_daemon().await;
    let mut client = zccache_monocrate::ipc::connect(&endpoint).await.unwrap();
    let sid = start_session(&mut client, &cwd, &log.to_string_lossy()).await;

    let pch_gen_args = || {
        vec![
            "-x".to_string(),
            "c++-header".to_string(),
            "-c".to_string(),
            pch_header.to_string_lossy().into_owned(),
            "-o".to_string(),
            pch_file.to_string_lossy().into_owned(),
            // Include path so pch.h can find sub.h
            "-I".to_string(),
            src_dir.to_string_lossy().into_owned(),
        ]
    };
    let compile_args = || {
        vec![
            "-c".to_string(),
            source.to_string_lossy().into_owned(),
            "-o".to_string(),
            obj.to_string_lossy().into_owned(),
            "-include-pch".to_string(),
            pch_file.to_string_lossy().into_owned(),
            "-I".to_string(),
            src_dir.to_string_lossy().into_owned(),
        ]
    };

    // ── Step 1: Generate PCH (in build dir) ────────────────────
    let (exit_code, _, stderr) =
        compile_raw(&mut client, &sid, &compiler, pch_gen_args(), &cwd).await;
    let stderr_str = String::from_utf8_lossy(&stderr);
    assert_eq!(exit_code, 0, "PCH gen failed: {stderr_str}");

    // ── Step 2: Compile with PCH → miss ────────────────────────
    let (exit_code, cached, stderr) =
        compile_raw(&mut client, &sid, &compiler, compile_args(), &cwd).await;
    let stderr_str = String::from_utf8_lossy(&stderr);
    assert_eq!(exit_code, 0, "compile failed: {stderr_str}");
    assert!(!cached, "first compile should miss");
    let first_obj = std::fs::read(&obj).unwrap();

    // ── Step 3: Compile again → hit ────────────────────────────
    std::fs::remove_file(&obj).unwrap();
    let (exit_code, cached, _) =
        compile_raw(&mut client, &sid, &compiler, compile_args(), &cwd).await;
    assert_eq!(exit_code, 0);
    assert!(cached, "second compile should hit");

    // ── Step 4: Modify sub-header (pch.h unchanged) ────────────
    std::thread::sleep(std::time::Duration::from_millis(100));
    std::fs::write(
        &sub_header,
        "#ifndef SUB_H\n#define SUB_H\n#define SUB_VALUE 99\n\
         inline int sub_extra() { return 7; }\n\
         #endif\n",
    )
    .unwrap();
    // Wait for watcher to process the sub.h change.
    // On Windows, ReadDirectoryChangesW may fail to deliver events for
    // subdirectories of temp dirs — this is the root cause of the bug
    // this test is designed to catch.
    std::thread::sleep(std::time::Duration::from_millis(2000));

    // ── Step 5: Regen PCH → must miss (sub.h changed) ──────────
    let (exit_code, cached, _) =
        compile_raw(&mut client, &sid, &compiler, pch_gen_args(), &cwd).await;
    assert_eq!(exit_code, 0);
    assert!(
        !cached,
        "PCH regen after sub.h change must miss (build-dir separation)"
    );

    // ── Step 6: Compile → must miss (stale hit = BUG) ──────────
    std::fs::remove_file(&obj).unwrap();
    let (exit_code, cached, _) =
        compile_raw(&mut client, &sid, &compiler, compile_args(), &cwd).await;
    assert_eq!(exit_code, 0);
    assert!(
        !cached,
        "compile after sub.h change MUST miss even with build-dir-separated PCH"
    );
    let second_obj = std::fs::read(&obj).unwrap();
    assert_ne!(
        first_obj, second_obj,
        "different SUB_VALUE must produce different .o"
    );

    // ── Step 7: Compile again → hit ────────────────────────────
    std::fs::remove_file(&obj).unwrap();
    let (exit_code, cached, _) =
        compile_raw(&mut client, &sid, &compiler, compile_args(), &cwd).await;
    assert_eq!(exit_code, 0);
    assert!(cached, "re-compile with no changes should hit");

    let log_text = std::fs::read_to_string(&log).unwrap();
    eprintln!("=== build-dir separation log ===\n{log_text}");

    shutdown.notify_one();
    server_handle.await.unwrap();
}

/// Adversarial test: chained PCH scenario (like FastLED).
///
/// base.h includes sub.h → base.h.pch
/// test_pch.h uses -include-pch base.h.pch → test_pch.h.pch
/// main.cpp uses -include-pch test_pch.h.pch
///
/// When sub.h changes:
/// - base.h.pch must be regenerated (cache miss)
/// - test_pch.h.pch must be regenerated (cache miss — base PCH changed)
/// - main.cpp must be recompiled (cache miss — test PCH changed)
#[tokio::test]
#[ignore] // integration: spawns clang + watcher sleeps, run with --full
async fn pch_chained_sub_header_change() {
    let clang = match zccache_monocrate::test_support::find_clang() {
        Some(p) => p,
        None => {
            eprintln!("skipping test: clang not found");
            return;
        }
    };

    let tmp = tempfile::tempdir().unwrap();
    let cwd = tmp.path().to_string_lossy().into_owned();
    let log_dir = tmp.path().join("logs");
    std::fs::create_dir_all(&log_dir).unwrap();
    let log = log_dir.join("session.log");
    let compiler = clang.to_string_lossy().into_owned();

    let sub_h = tmp.path().join("sub.h");
    let base_h = tmp.path().join("base.h");
    let base_pch = tmp.path().join("base.h.pch");
    let test_h = tmp.path().join("test_pch.h");
    let test_pch = tmp.path().join("test_pch.h.pch");
    let source = tmp.path().join("main.cpp");
    let obj = tmp.path().join("main.o");

    std::fs::write(
        &sub_h,
        "#ifndef SUB_H\n#define SUB_H\n#define SUB_VALUE 42\n#endif\n",
    )
    .unwrap();
    std::fs::write(
        &base_h,
        "#ifndef BASE_H\n#define BASE_H\n#include \"sub.h\"\n#endif\n",
    )
    .unwrap();
    std::fs::write(
        &test_h,
        "#ifndef TEST_PCH_H\n#define TEST_PCH_H\n#define TEST_EXTRA 1\n#endif\n",
    )
    .unwrap();
    std::fs::write(&source, "int main() { return SUB_VALUE + TEST_EXTRA; }\n").unwrap();

    let (endpoint, server_handle, shutdown) = start_daemon().await;
    let mut client = zccache_monocrate::ipc::connect(&endpoint).await.unwrap();
    let sid = start_session(&mut client, &cwd, &log.to_string_lossy()).await;

    // ── Build the PCH chain ─────────────────────────────────────
    // 1. Generate base PCH
    let (exit_code, _, stderr) = compile_raw(
        &mut client,
        &sid,
        &compiler,
        vec![
            "-x".into(),
            "c++-header".into(),
            "-c".into(),
            base_h.to_string_lossy().into_owned(),
            "-o".into(),
            base_pch.to_string_lossy().into_owned(),
        ],
        &cwd,
    )
    .await;
    let stderr_str = String::from_utf8_lossy(&stderr);
    assert_eq!(exit_code, 0, "base PCH gen failed: {stderr_str}");

    // 2. Generate test PCH (chained, includes base PCH)
    let (exit_code, _, stderr) = compile_raw(
        &mut client,
        &sid,
        &compiler,
        vec![
            "-x".into(),
            "c++-header".into(),
            "-c".into(),
            test_h.to_string_lossy().into_owned(),
            "-o".into(),
            test_pch.to_string_lossy().into_owned(),
            "-include-pch".into(),
            base_pch.to_string_lossy().into_owned(),
        ],
        &cwd,
    )
    .await;
    let stderr_str = String::from_utf8_lossy(&stderr);
    assert_eq!(exit_code, 0, "test PCH gen failed: {stderr_str}");

    // 3. Compile with chained PCH → miss
    let (exit_code, cached, stderr) = compile_raw(
        &mut client,
        &sid,
        &compiler,
        vec![
            "-c".into(),
            source.to_string_lossy().into_owned(),
            "-o".into(),
            obj.to_string_lossy().into_owned(),
            "-include-pch".into(),
            test_pch.to_string_lossy().into_owned(),
        ],
        &cwd,
    )
    .await;
    let stderr_str = String::from_utf8_lossy(&stderr);
    assert_eq!(
        exit_code, 0,
        "compile with chained PCH failed: {stderr_str}"
    );
    assert!(!cached, "first compile should miss");
    let first_obj = std::fs::read(&obj).unwrap();

    // 4. Compile again → hit
    std::fs::remove_file(&obj).unwrap();
    let (exit_code, cached, _) = compile_raw(
        &mut client,
        &sid,
        &compiler,
        vec![
            "-c".into(),
            source.to_string_lossy().into_owned(),
            "-o".into(),
            obj.to_string_lossy().into_owned(),
            "-include-pch".into(),
            test_pch.to_string_lossy().into_owned(),
        ],
        &cwd,
    )
    .await;
    assert_eq!(exit_code, 0);
    assert!(cached, "second compile should hit");

    // ── Modify sub.h (base.h and test_pch.h unchanged) ─────────
    std::thread::sleep(std::time::Duration::from_millis(100));
    std::fs::write(
        &sub_h,
        "#ifndef SUB_H\n#define SUB_H\n#define SUB_VALUE 99\n#endif\n",
    )
    .unwrap();
    // Windows watcher latency can exceed 500ms under load.
    std::thread::sleep(std::time::Duration::from_millis(2000));

    // ── Rebuild the entire chain ────────────────────────────────
    // 5. Regen base PCH → must miss
    let (exit_code, cached, _) = compile_raw(
        &mut client,
        &sid,
        &compiler,
        vec![
            "-x".into(),
            "c++-header".into(),
            "-c".into(),
            base_h.to_string_lossy().into_owned(),
            "-o".into(),
            base_pch.to_string_lossy().into_owned(),
        ],
        &cwd,
    )
    .await;
    assert_eq!(exit_code, 0);
    assert!(!cached, "base PCH regen must miss after sub.h change");

    // 6. Regen test PCH → must miss (base PCH changed)
    let (exit_code, cached, _) = compile_raw(
        &mut client,
        &sid,
        &compiler,
        vec![
            "-x".into(),
            "c++-header".into(),
            "-c".into(),
            test_h.to_string_lossy().into_owned(),
            "-o".into(),
            test_pch.to_string_lossy().into_owned(),
            "-include-pch".into(),
            base_pch.to_string_lossy().into_owned(),
        ],
        &cwd,
    )
    .await;
    assert_eq!(exit_code, 0);
    assert!(
        !cached,
        "chained test PCH regen must miss after sub.h change"
    );

    // 7. Compile with new chained PCH → must miss
    std::fs::remove_file(&obj).unwrap();
    let (exit_code, cached, _) = compile_raw(
        &mut client,
        &sid,
        &compiler,
        vec![
            "-c".into(),
            source.to_string_lossy().into_owned(),
            "-o".into(),
            obj.to_string_lossy().into_owned(),
            "-include-pch".into(),
            test_pch.to_string_lossy().into_owned(),
        ],
        &cwd,
    )
    .await;
    assert_eq!(exit_code, 0);
    assert!(
        !cached,
        "compile with changed chained PCH MUST miss (sub.h changed transitively)"
    );
    let second_obj = std::fs::read(&obj).unwrap();
    assert_ne!(
        first_obj, second_obj,
        "different SUB_VALUE must produce different .o"
    );

    // 8. Compile again → hit (stable)
    std::fs::remove_file(&obj).unwrap();
    let (exit_code, cached, _) = compile_raw(
        &mut client,
        &sid,
        &compiler,
        vec![
            "-c".into(),
            source.to_string_lossy().into_owned(),
            "-o".into(),
            obj.to_string_lossy().into_owned(),
            "-include-pch".into(),
            test_pch.to_string_lossy().into_owned(),
        ],
        &cwd,
    )
    .await;
    assert_eq!(exit_code, 0);
    assert!(cached, "re-compile after stabilization should hit");

    let log_text = std::fs::read_to_string(&log).unwrap();
    eprintln!("=== chained PCH log ===\n{log_text}");

    shutdown.notify_one();
    server_handle.await.unwrap();
}

/// Regression test: modifying a sub-header included by a PCH must NOT create
/// a spurious `.pch` file next to the modified header in the source tree.
///
/// Reproduces the bug reported in BUG_PCH_SPURIOUS_GENERATION.md:
/// - Source tree has `src/sub.h` (dependency) and `src/pch.h` (PCH target)
/// - PCH output goes to a separate `build/` directory via explicit `-o`
/// - After modifying `sub.h` and rebuilding, `src/sub.h.pch` must NOT exist
///
/// Root cause: `default_output()` used to preserve directory components for
/// header files (e.g., `src/sub.h` → `src/sub.h.pch`), which the daemon
/// could resolve into the source tree during cache restoration.
#[tokio::test]
#[ignore] // integration: spawns clang + watcher sleeps, run with --full
async fn pch_rebuild_no_spurious_output_in_source_tree() {
    let clang = match zccache_monocrate::test_support::find_clang() {
        Some(p) => p,
        None => {
            eprintln!("skipping test: clang not found");
            return;
        }
    };

    let tmp = tempfile::tempdir().unwrap();
    let log_dir = tmp.path().join("logs");
    std::fs::create_dir_all(&log_dir).unwrap();
    let log = log_dir.join("session.log");
    let compiler = clang.to_string_lossy().into_owned();

    // Source in src/, PCH output in build/ — separate directories.
    let src_dir = tmp.path().join("src");
    let build_dir = tmp.path().join("build");
    std::fs::create_dir_all(&src_dir).unwrap();
    std::fs::create_dir_all(&build_dir).unwrap();

    let cwd = tmp.path().to_string_lossy().into_owned();

    // sub.h — the header we'll modify
    let sub_h = src_dir.join("sub.h");
    std::fs::write(&sub_h, "#pragma once\ninline int sub() { return 1; }\n").unwrap();

    // pch.h — the PCH target that includes sub.h
    let pch_h = src_dir.join("pch.h");
    std::fs::write(&pch_h, "#pragma once\n#include \"sub.h\"\n").unwrap();

    // main.cpp — source file
    let main_cpp = src_dir.join("main.cpp");
    std::fs::write(
        &main_cpp,
        "#include \"sub.h\"\nint main() { return sub(); }\n",
    )
    .unwrap();

    let pch_output = build_dir.join("pch.h.pch");
    let main_obj = build_dir.join("main.o");

    let (endpoint, server_handle, shutdown) = start_daemon().await;
    let mut client = zccache_monocrate::ipc::connect(&endpoint).await.unwrap();
    let sid = start_session(&mut client, &cwd, &log.to_string_lossy()).await;

    let isrc = format!("-I{}", src_dir.to_string_lossy());

    // ── Step 1: Generate PCH (explicit -o into build dir) ────────
    let (exit_code, _, stderr) = compile_raw(
        &mut client,
        &sid,
        &compiler,
        vec![
            "-x".into(),
            "c++-header".into(),
            pch_h.to_string_lossy().into_owned(),
            "-o".into(),
            pch_output.to_string_lossy().into_owned(),
            isrc.clone(),
        ],
        &cwd,
    )
    .await;
    let stderr_str = String::from_utf8_lossy(&stderr);
    assert_eq!(exit_code, 0, "PCH gen failed: {stderr_str}");
    assert!(pch_output.exists(), "PCH file should be created");

    // ── Step 2: Compile main.cpp using PCH ───────────────────────
    let (exit_code, _, stderr) = compile_raw(
        &mut client,
        &sid,
        &compiler,
        vec![
            "-c".into(),
            main_cpp.to_string_lossy().into_owned(),
            "-o".into(),
            main_obj.to_string_lossy().into_owned(),
            "-include-pch".into(),
            pch_output.to_string_lossy().into_owned(),
            isrc.clone(),
        ],
        &cwd,
    )
    .await;
    let stderr_str = String::from_utf8_lossy(&stderr);
    assert_eq!(exit_code, 0, "compile with PCH failed: {stderr_str}");

    // ── Step 3: Modify sub.h ─────────────────────────────────────
    std::thread::sleep(std::time::Duration::from_millis(100));
    std::fs::write(&sub_h, "#pragma once\ninline int sub() { return 2; }\n").unwrap();
    std::thread::sleep(std::time::Duration::from_millis(2000));

    // ── Step 4: Rebuild PCH ──────────────────────────────────────
    let (exit_code, _, _) = compile_raw(
        &mut client,
        &sid,
        &compiler,
        vec![
            "-x".into(),
            "c++-header".into(),
            pch_h.to_string_lossy().into_owned(),
            "-o".into(),
            pch_output.to_string_lossy().into_owned(),
            isrc.clone(),
        ],
        &cwd,
    )
    .await;
    assert_eq!(exit_code, 0, "PCH rebuild failed");

    // ── Step 5: Recompile main.cpp ───────────────────────────────
    let (exit_code, _, _) = compile_raw(
        &mut client,
        &sid,
        &compiler,
        vec![
            "-c".into(),
            main_cpp.to_string_lossy().into_owned(),
            "-o".into(),
            main_obj.to_string_lossy().into_owned(),
            "-include-pch".into(),
            pch_output.to_string_lossy().into_owned(),
            isrc.clone(),
        ],
        &cwd,
    )
    .await;
    assert_eq!(exit_code, 0, "recompile with PCH failed");

    // ── Step 6: ASSERTION — no .pch files in source tree ─────────
    for entry in std::fs::read_dir(&src_dir).unwrap() {
        let path = entry.unwrap().path();
        assert!(
            path.extension().and_then(|e| e.to_str()) != Some("pch"),
            "BUG: spurious PCH file found in source tree: {}",
            path.display()
        );
    }

    let log_text = std::fs::read_to_string(&log).unwrap();
    eprintln!("=== spurious PCH test log ===\n{log_text}");

    shutdown.notify_one();
    server_handle.await.unwrap();
}
