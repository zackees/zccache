//! Adversarial corner-case tests for cache correctness.
//!
//! These tests target subtle scenarios NOT covered by other test suites:
//! - Failed compile not cached (header appears after failure)
//! - Cache persistence across session boundaries
//! - Thundering herd (concurrent same-file compilation from multiple sessions)
//!
//! Run all:    soldr cargo test -p zccache-daemon --test adversarial_corner_cases -- --nocapture
//! Run single: soldr cargo test -p zccache-daemon --test adversarial_corner_cases -- <test_name> --nocapture

use std::path::Path;
use std::sync::Arc;
use tokio::sync::Notify;
use tokio::task::JoinHandle;
use zccache::core::NormalizedPath;
use zccache::daemon::DaemonServer;
use zccache::protocol::{Request, Response};

// ─── Platform types ──────────────────────────────────────────────────────────

#[cfg(unix)]
type ClientConn = zccache::ipc::IpcConnection;
#[cfg(windows)]
type ClientConn = zccache::ipc::IpcClientConnection;

// ─── Helpers ─────────────────────────────────────────────────────────────────

async fn start_daemon() -> (String, JoinHandle<()>, Arc<Notify>) {
    let endpoint = zccache::ipc::unique_test_endpoint();
    let mut server = DaemonServer::bind(&endpoint).unwrap();
    let shutdown = server.shutdown_handle();
    let handle = tokio::spawn(async move {
        server.run(0).await.unwrap();
    });
    (endpoint, handle, shutdown)
}

async fn start_session(
    client: &mut ClientConn,
    _clang: &Path,
    cwd: &str,
    log_file: &str,
) -> String {
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

async fn compile(
    client: &mut ClientConn,
    session_id: &str,
    compiler: &str,
    args: &[&str],
    cwd: &str,
) -> (i32, bool) {
    client
        .send(&Request::Compile {
            session_id: session_id.to_string(),
            args: args.iter().map(|s| s.to_string()).collect(),
            cwd: cwd.to_string().into(),
            compiler: compiler.to_string().into(),
            env: None,
            stdin: Vec::new(),
        })
        .await
        .unwrap();

    match client.recv().await.unwrap() {
        Some(Response::CompileResult {
            exit_code, cached, ..
        }) => (exit_code, cached),
        Some(Response::Error { message }) => panic!("compile error: {message}"),
        other => panic!("expected CompileResult, got: {other:?}"),
    }
}

async fn compile_and_read(
    client: &mut ClientConn,
    session_id: &str,
    compiler: &str,
    args: &[&str],
    cwd: &str,
    obj_path: &Path,
) -> (i32, bool, Vec<u8>) {
    let (exit_code, cached) = compile(client, session_id, compiler, args, cwd).await;
    let obj_data = if obj_path.exists() {
        std::fs::read(obj_path).unwrap()
    } else {
        vec![]
    };
    (exit_code, cached, obj_data)
}

struct TestHarness {
    clang: NormalizedPath,
    tmp: tempfile::TempDir,
    #[expect(dead_code)]
    endpoint: String,
    server_handle: JoinHandle<()>,
    shutdown: Arc<Notify>,
    client: ClientConn,
    session_id: String,
}

impl TestHarness {
    async fn new() -> Option<Self> {
        let clang = zccache::test_support::find_clang()?;
        let tmp = tempfile::tempdir().unwrap();
        let log = tmp.path().join("log.txt");
        let cwd = tmp.path().to_string_lossy().into_owned();

        let (endpoint, server_handle, shutdown) = start_daemon().await;
        let mut client = zccache::ipc::connect(&endpoint).await.unwrap();
        let session_id = start_session(&mut client, &clang, &cwd, &log.to_string_lossy()).await;

        Some(Self {
            clang,
            tmp,
            endpoint,
            server_handle,
            shutdown,
            client,
            session_id,
        })
    }

    fn cwd(&self) -> String {
        self.tmp.path().to_string_lossy().into_owned()
    }

    fn path(&self, name: &str) -> NormalizedPath {
        NormalizedPath::new(self.tmp.path().join(name))
    }

    fn write_file(&self, name: &str, content: &str) -> NormalizedPath {
        let p = self.path(name);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&p, content).unwrap();
        p
    }

    async fn compile_file_read(&mut self, src: &str, obj: &str) -> (i32, bool, Vec<u8>) {
        let obj_path = self.path(obj);
        let cwd = self.cwd();
        let compiler = self.clang.to_string_lossy().into_owned();
        compile_and_read(
            &mut self.client,
            &self.session_id,
            &compiler,
            &["-c", src, "-o", obj],
            &cwd,
            &obj_path,
        )
        .await
    }

    async fn shutdown(self) {
        self.shutdown.notify_one();
        self.server_handle.await.unwrap();
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// FAILED COMPILE NOT CACHED
//
// If a compile fails (e.g., missing header), the failure must NOT be cached.
// When the header is later created, the next compile must succeed.
// ═══════════════════════════════════════════════════════════════════════════

/// Missing header → compile fails. Create header → compile succeeds.
/// The failure must not be served from cache.
#[tokio::test]
#[ignore] // integration: spawns clang, run with --full
async fn corner_failed_compile_not_cached_then_header_appears() {
    let mut h = match TestHarness::new().await {
        Some(h) => h,
        None => return,
    };

    // Source references a header that doesn't exist yet
    h.write_file(
        "needs_hdr.cpp",
        "#include \"late.h\"\nint f() { return LATE_VAL; }\n",
    );

    // First compile: fails (missing header)
    let (exit_code, cached, _) = h.compile_file_read("needs_hdr.cpp", "needs_hdr.o").await;
    assert_ne!(exit_code, 0, "compile with missing header should fail");
    assert!(!cached, "failed compile must not be cached");

    // Now create the missing header
    h.write_file("late.h", "#define LATE_VAL 42\n");

    // Second compile: must succeed (not serve cached failure)
    let (exit_code, cached, _) = h.compile_file_read("needs_hdr.cpp", "needs_hdr.o").await;
    assert_eq!(
        exit_code, 0,
        "compile should succeed after header is created"
    );
    assert!(
        !cached,
        "must be a fresh compile, not a cached result from the failed attempt"
    );

    // Third compile: should hit (nothing changed)
    std::fs::remove_file(h.path("needs_hdr.o")).unwrap();
    let (exit_code, cached, _) = h.compile_file_read("needs_hdr.cpp", "needs_hdr.o").await;
    assert_eq!(exit_code, 0);
    assert!(cached, "third compile with no changes should hit cache");

    h.shutdown().await;
}

/// Compile error from syntax error → fix source → must succeed.
/// Verifies compile errors are never cached regardless of cause.
#[tokio::test]
#[ignore] // integration: spawns clang, run with --full
async fn corner_syntax_error_not_cached() {
    let mut h = match TestHarness::new().await {
        Some(h) => h,
        None => return,
    };

    // Broken source
    h.write_file("broken.cpp", "int f() { return }\n");
    let (exit_code, cached, _) = h.compile_file_read("broken.cpp", "broken.o").await;
    assert_ne!(exit_code, 0, "syntax error should fail");
    assert!(!cached, "failed compile must not be cached");

    // Fix source
    h.write_file("broken.cpp", "int f() { return 0; }\n");
    let (exit_code, cached, _) = h.compile_file_read("broken.cpp", "broken.o").await;
    assert_eq!(exit_code, 0, "fixed source should compile");
    assert!(!cached, "must be fresh compile, not cached failure");

    h.shutdown().await;
}

// ═══════════════════════════════════════════════════════════════════════════
// SESSION BOUNDARY PERSISTENCE
//
// Cache must survive session end/restart. Artifacts are stored on disk and
// should be available to new sessions.
// ═══════════════════════════════════════════════════════════════════════════

/// Compile in session A, end session A, start new session B on same daemon,
/// recompile → should hit cache.
#[tokio::test]
#[ignore] // integration: spawns clang, run with --full
async fn corner_cache_survives_session_restart() {
    let clang = match zccache::test_support::find_clang() {
        Some(c) => c,
        None => return,
    };

    let tmp = tempfile::tempdir().unwrap();
    let cwd = tmp.path().to_string_lossy().into_owned();
    let log1 = tmp.path().join("log1.txt");
    let log2 = tmp.path().join("log2.txt");

    let (endpoint, server_handle, shutdown) = start_daemon().await;

    // Session A: compile
    let mut client1 = zccache::ipc::connect(&endpoint).await.unwrap();
    let sid1 = start_session(&mut client1, &clang, &cwd, &log1.to_string_lossy()).await;

    let src = tmp.path().join("persist.cpp");
    std::fs::write(&src, "int f() { return 7; }\n").unwrap();

    let obj = tmp.path().join("persist.o");
    let (exit, cached, obj_v1) = compile_and_read(
        &mut client1,
        &sid1,
        &clang.to_string_lossy(),
        &["-c", "persist.cpp", "-o", "persist.o"],
        &cwd,
        &obj,
    )
    .await;
    assert_eq!(exit, 0);
    assert!(!cached, "session A first compile must miss");

    // End session A
    client1
        .send(&Request::SessionEnd { session_id: sid1 })
        .await
        .unwrap();
    let _: Option<Response> = client1.recv().await.unwrap();

    // Session B: new client, new session, same daemon
    let mut client2 = zccache::ipc::connect(&endpoint).await.unwrap();
    let sid2 = start_session(&mut client2, &clang, &cwd, &log2.to_string_lossy()).await;

    // Recompile same file in session B — should hit
    std::fs::remove_file(&obj).unwrap();
    let (exit, cached, obj_v2) = compile_and_read(
        &mut client2,
        &sid2,
        &clang.to_string_lossy(),
        &["-c", "persist.cpp", "-o", "persist.o"],
        &cwd,
        &obj,
    )
    .await;
    assert_eq!(exit, 0);
    assert!(
        cached,
        "session B should hit cache from session A's artifact"
    );
    assert_eq!(obj_v1, obj_v2, "same content → same .o across sessions");

    shutdown.notify_one();
    server_handle.await.unwrap();
}

// ═══════════════════════════════════════════════════════════════════════════
// THUNDERING HERD
//
// Multiple sessions compile the exact same file at the same time.
// All must succeed and produce identical, non-corrupted artifacts.
// ═══════════════════════════════════════════════════════════════════════════

/// 4 sessions simultaneously compile the same source file.
/// At most 1 should be a miss; the rest should either hit or also miss
/// (depending on timing), but ALL must produce identical, valid .o files.
#[tokio::test]
#[ignore] // integration: spawns clang 4x concurrently, run with --full
async fn corner_thundering_herd_same_file() {
    let clang = match zccache::test_support::find_clang() {
        Some(c) => c,
        None => return,
    };

    let tmp = tempfile::tempdir().unwrap();
    let cwd = tmp.path().to_string_lossy().into_owned();

    let src = tmp.path().join("herd.cpp");
    std::fs::write(&src, "int f() { return 42; }\n").unwrap();

    let (endpoint, server_handle, shutdown) = start_daemon().await;

    let n_sessions = 4;
    let mut handles = Vec::new();

    for i in 0..n_sessions {
        let ep = endpoint.clone();
        let clang = clang.clone();
        let cwd = cwd.clone();
        let obj_dir = tmp.path().join(format!("out_{i}"));
        std::fs::create_dir_all(&obj_dir).unwrap();
        let log = tmp.path().join(format!("log_{i}.txt"));

        handles.push(tokio::spawn(async move {
            let mut client = zccache::ipc::connect(&ep).await.unwrap();
            let sid = start_session(&mut client, &clang, &cwd, &log.to_string_lossy()).await;

            let obj_name = format!("out_{i}/herd.o");
            let obj_path = NormalizedPath::new(Path::new(&cwd).join(&obj_name));
            let (exit, cached, obj) = compile_and_read(
                &mut client,
                &sid,
                &clang.to_string_lossy(),
                &["-c", "herd.cpp", "-o", &obj_name],
                &cwd,
                &obj_path,
            )
            .await;

            (exit, cached, obj)
        }));
    }

    let mut results = Vec::new();
    for handle in handles {
        results.push(handle.await.unwrap());
    }

    // All must succeed
    for (i, (exit, _, _)) in results.iter().enumerate() {
        assert_eq!(*exit, 0, "session {i} must compile successfully");
    }

    // All must produce identical .o bytes (whether cached or not),
    // ignoring the COFF TimeDateStamp at bytes 4..8 which the compiler
    // sets to the current time and may differ across compilations.
    let strip_coff_timestamp = |data: &[u8]| -> Vec<u8> {
        let mut v = data.to_vec();
        if v.len() >= 8 {
            v[4..8].fill(0);
        }
        v
    };
    let reference_obj = strip_coff_timestamp(&results[0].2);
    assert!(!reference_obj.is_empty(), "reference .o must not be empty");
    for (i, (_, _, obj)) in results.iter().enumerate().skip(1) {
        assert_eq!(
            reference_obj,
            strip_coff_timestamp(obj),
            "session {i} must produce identical .o to session 0 (ignoring COFF timestamp)"
        );
    }

    // At least one should have been a cache miss (the first to compile)
    let miss_count = results.iter().filter(|(_, cached, _)| !cached).count();
    let hit_count = results.iter().filter(|(_, cached, _)| *cached).count();
    eprintln!("thundering herd: {miss_count} misses, {hit_count} hits out of {n_sessions}");
    assert!(miss_count >= 1, "at least one session must be a cache miss");

    shutdown.notify_one();
    server_handle.await.unwrap();
}

/// Thundering herd after cache is warm: all sessions should hit.
#[tokio::test]
#[ignore] // integration: spawns clang 4x concurrently, run with --full
async fn corner_thundering_herd_all_warm() {
    let clang = match zccache::test_support::find_clang() {
        Some(c) => c,
        None => return,
    };

    let tmp = tempfile::tempdir().unwrap();
    let cwd = tmp.path().to_string_lossy().into_owned();
    let src = tmp.path().join("warm.cpp");
    std::fs::write(&src, "int f() { return 7; }\n").unwrap();

    let (endpoint, server_handle, shutdown) = start_daemon().await;

    // Warm the cache with a single compile
    {
        let log = tmp.path().join("warm_log.txt");
        let mut client = zccache::ipc::connect(&endpoint).await.unwrap();
        let sid = start_session(&mut client, &clang, &cwd, &log.to_string_lossy()).await;
        let obj_path = tmp.path().join("warm.o");
        let (exit, cached, _) = compile_and_read(
            &mut client,
            &sid,
            &clang.to_string_lossy(),
            &["-c", "warm.cpp", "-o", "warm.o"],
            &cwd,
            &obj_path,
        )
        .await;
        assert_eq!(exit, 0);
        assert!(!cached, "warming compile must miss");
    }

    // Now 4 sessions all compile at once — all should hit
    let n_sessions = 4;
    let mut handles = Vec::new();

    for i in 0..n_sessions {
        let ep = endpoint.clone();
        let clang = clang.clone();
        let cwd = cwd.clone();
        let obj_dir = tmp.path().join(format!("warm_out_{i}"));
        std::fs::create_dir_all(&obj_dir).unwrap();
        let log = tmp.path().join(format!("warm_log_{i}.txt"));

        handles.push(tokio::spawn(async move {
            let mut client = zccache::ipc::connect(&ep).await.unwrap();
            let sid = start_session(&mut client, &clang, &cwd, &log.to_string_lossy()).await;

            let obj_name = format!("warm_out_{i}/warm.o");
            let obj_path = NormalizedPath::new(Path::new(&cwd).join(&obj_name));
            let (exit, cached, obj) = compile_and_read(
                &mut client,
                &sid,
                &clang.to_string_lossy(),
                &["-c", "warm.cpp", "-o", &obj_name],
                &cwd,
                &obj_path,
            )
            .await;

            (exit, cached, obj)
        }));
    }

    let mut results = Vec::new();
    for handle in handles {
        results.push(handle.await.unwrap());
    }

    // All must succeed and hit cache
    for (i, (exit, cached, _)) in results.iter().enumerate() {
        assert_eq!(*exit, 0, "session {i} must succeed");
        assert!(*cached, "session {i} must hit cache (cache was warmed)");
    }

    shutdown.notify_one();
    server_handle.await.unwrap();
}
