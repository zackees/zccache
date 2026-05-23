//! Adversarial mutation tests for cache correctness under stable file states.
//!
//! These tests verify that the cache behaves correctly when files don't change
//! between compilations. File-change invalidation is handled by the watcher
//! subsystem and tested separately. Here we focus on: content-hash stability
//! (touch/delete-recreate with same content), independent file isolation,
//! include-path differentiation, and preprocessor-flag differentiation.
//!
//! Run all:    soldr cargo test -p zccache-daemon --test adversarial_mutations -- --nocapture
//! Run single: soldr cargo test -p zccache-daemon --test adversarial_mutations -- <test_name> --nocapture

use std::path::Path;
use zccache_monocrate::core::NormalizedPath;
use zccache_monocrate::daemon::DaemonServer;
use zccache_monocrate::protocol::{Request, Response};

// ─── Platform types ──────────────────────────────────────────────────────────

#[cfg(unix)]
type ClientConn = zccache_monocrate::ipc::IpcConnection;
#[cfg(windows)]
type ClientConn = zccache_monocrate::ipc::IpcClientConnection;

// ─── Helpers ─────────────────────────────────────────────────────────────────

async fn start_daemon() -> (
    String,
    tokio::task::JoinHandle<()>,
    std::sync::Arc<tokio::sync::Notify>,
) {
    let endpoint = zccache_monocrate::ipc::unique_test_endpoint();
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
    args: &[&str],
    cwd: &str,
    compiler: &str,
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

/// Compile and return (exit_code, cached, object_file_bytes).
async fn compile_and_read(
    client: &mut ClientConn,
    session_id: &str,
    args: &[&str],
    cwd: &str,
    obj_path: &Path,
    compiler: &str,
) -> (i32, bool, Vec<u8>) {
    let (exit_code, cached) = compile(client, session_id, args, cwd, compiler).await;
    let obj_data = if obj_path.exists() {
        std::fs::read(obj_path).unwrap()
    } else {
        vec![]
    };
    (exit_code, cached, obj_data)
}

/// Convenience: set up daemon + session + temp dir.
struct TestHarness {
    clang: NormalizedPath,
    tmp: tempfile::TempDir,
    #[expect(dead_code)]
    endpoint: String,
    server_handle: tokio::task::JoinHandle<()>,
    shutdown: std::sync::Arc<tokio::sync::Notify>,
    client: ClientConn,
    session_id: String,
}

impl TestHarness {
    async fn new() -> Option<Self> {
        let clang = zccache_monocrate::test_support::find_clang()?;
        let tmp = tempfile::tempdir().unwrap();
        let log = tmp.path().join("log.txt");
        let cwd = tmp.path().to_string_lossy().into_owned();

        let (endpoint, server_handle, shutdown) = start_daemon().await;
        let mut client = zccache_monocrate::ipc::connect(&endpoint).await.unwrap();
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
        std::fs::write(&p, content).unwrap();
        p
    }

    fn compiler_str(&self) -> String {
        self.clang.to_string_lossy().into_owned()
    }

    async fn compile_file_read(&mut self, src: &str, obj: &str) -> (i32, bool, Vec<u8>) {
        let obj_path = self.path(obj);
        let cwd = self.cwd();
        let compiler = self.compiler_str();
        compile_and_read(
            &mut self.client,
            &self.session_id,
            &["-c", src, "-o", obj],
            &cwd,
            &obj_path,
            &compiler,
        )
        .await
    }

    async fn shutdown(self) {
        self.shutdown.notify_one();
        self.server_handle.await.unwrap();
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// SOURCE FILE MUTATIONS
// ═══════════════════════════════════════════════════════════════════════════

/// Touch source (change mtime, same content) → cache should still HIT
/// because content hash is unchanged after rehash.
#[tokio::test]
#[ignore] // integration: spawns clang + 1100ms sleep, run with --full
async fn mutation_touch_source_no_invalidation() {
    let mut h = match TestHarness::new().await {
        Some(h) => h,
        None => return,
    };

    h.write_file("touch.cpp", "int f() { return 42; }\n");

    let (_, cached, obj_v1) = h.compile_file_read("touch.cpp", "touch.o").await;
    assert!(!cached);

    // Touch: rewrite identical content (changes mtime)
    std::thread::sleep(std::time::Duration::from_millis(1100)); // ensure mtime differs
    h.write_file("touch.cpp", "int f() { return 42; }\n");

    std::fs::remove_file(h.path("touch.o")).unwrap();
    let (_, cached, obj_v2) = h.compile_file_read("touch.cpp", "touch.o").await;
    assert!(
        cached,
        "touch with same content should still hit cache (content hash unchanged)"
    );
    assert_eq!(obj_v1, obj_v2, "same content → same .o");

    h.shutdown().await;
}

// ═══════════════════════════════════════════════════════════════════════════
// FILE LIFECYCLE MUTATIONS
// ═══════════════════════════════════════════════════════════════════════════

/// Delete source, recreate with SAME content → should still hit
/// (content hash is the same).
#[tokio::test]
#[ignore] // integration: spawns clang + 1100ms sleep, run with --full
async fn mutation_delete_recreate_same_content() {
    let mut h = match TestHarness::new().await {
        Some(h) => h,
        None => return,
    };

    let content = "int f() { return 42; }\n";
    h.write_file("same.cpp", content);
    let (_, cached, obj_v1) = h.compile_file_read("same.cpp", "same.o").await;
    assert!(!cached);

    // Delete and recreate with same content
    std::fs::remove_file(h.path("same.cpp")).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(1100)); // ensure different mtime
    h.write_file("same.cpp", content);

    std::fs::remove_file(h.path("same.o")).unwrap();
    let (_, cached, obj_v2) = h.compile_file_read("same.cpp", "same.o").await;
    assert!(
        cached,
        "delete+recreate with same content should hit (same content hash)"
    );
    assert_eq!(obj_v1, obj_v2);

    h.shutdown().await;
}

/// Add a brand new source file to the project → should not affect existing caches.
#[tokio::test]
#[ignore] // integration: spawns clang, run with --full
async fn mutation_add_new_file_no_interference() {
    let mut h = match TestHarness::new().await {
        Some(h) => h,
        None => return,
    };

    h.write_file("existing.cpp", "int f() { return 1; }\n");
    let (_, cached, obj_existing) = h.compile_file_read("existing.cpp", "existing.o").await;
    assert!(!cached);

    // Add a brand new file
    h.write_file("brand_new.cpp", "int g() { return 2; }\n");
    let (_, cached, _) = h.compile_file_read("brand_new.cpp", "brand_new.o").await;
    assert!(!cached, "brand new file should miss");

    // Existing file should still hit
    std::fs::remove_file(h.path("existing.o")).unwrap();
    let (_, cached, obj_again) = h.compile_file_read("existing.cpp", "existing.o").await;
    assert!(
        cached,
        "existing file should still hit after adding new file"
    );
    assert_eq!(obj_existing, obj_again);

    h.shutdown().await;
}

// ═══════════════════════════════════════════════════════════════════════════
// INCLUDE PATH AND FLAG MUTATIONS
// ═══════════════════════════════════════════════════════════════════════════

/// Same source, different -I include paths → different cache entries.
#[tokio::test]
#[ignore] // integration: spawns clang, run with --full
async fn mutation_include_path_creates_different_cache_entry() {
    let mut h = match TestHarness::new().await {
        Some(h) => h,
        None => return,
    };

    // Two directories with same-named header but different content
    let dir_a = h.path("inc_a");
    let dir_b = h.path("inc_b");
    std::fs::create_dir_all(&dir_a).unwrap();
    std::fs::create_dir_all(&dir_b).unwrap();

    std::fs::write(dir_a.join("config.h"), "#define VAL 1\n").unwrap();
    std::fs::write(dir_b.join("config.h"), "#define VAL 2\n").unwrap();

    h.write_file(
        "inc_test.cpp",
        "#include \"config.h\"\nint f() { return VAL; }\n",
    );

    let inc_a_str = format!("-I{}", dir_a.to_string_lossy());
    let inc_b_str = format!("-I{}", dir_b.to_string_lossy());
    let compiler = h.compiler_str();

    // Compile with -I inc_a
    let (exit_code, cached, obj_a) = {
        let obj_path = h.path("inc_test.o");
        let cwd = h.cwd();
        let (ec, c) = compile(
            &mut h.client,
            &h.session_id,
            &["-c", "inc_test.cpp", "-o", "inc_test.o", &inc_a_str],
            &cwd,
            &compiler,
        )
        .await;
        let obj = std::fs::read(&obj_path).unwrap();
        (ec, c, obj)
    };
    assert_eq!(exit_code, 0);
    assert!(!cached);

    // Compile with -I inc_b — different include path = different cache key
    let _ = std::fs::remove_file(h.path("inc_test.o"));
    let (exit_code, cached, obj_b) = {
        let obj_path = h.path("inc_test.o");
        let cwd = h.cwd();
        let (ec, c) = compile(
            &mut h.client,
            &h.session_id,
            &["-c", "inc_test.cpp", "-o", "inc_test.o", &inc_b_str],
            &cwd,
            &compiler,
        )
        .await;
        let obj = std::fs::read(&obj_path).unwrap();
        (ec, c, obj)
    };
    assert_eq!(exit_code, 0);
    assert!(
        !cached,
        "different -I path must create different cache entry"
    );
    assert_ne!(
        obj_a, obj_b,
        "different include dirs with different headers → different .o"
    );

    // Recompile with -I inc_a — should hit original cache
    let _ = std::fs::remove_file(h.path("inc_test.o"));
    let (_, cached, obj_a2) = {
        let obj_path = h.path("inc_test.o");
        let cwd = h.cwd();
        let (ec, c) = compile(
            &mut h.client,
            &h.session_id,
            &["-c", "inc_test.cpp", "-o", "inc_test.o", &inc_a_str],
            &cwd,
            &compiler,
        )
        .await;
        let obj = std::fs::read(&obj_path).unwrap();
        (ec, c, obj)
    };
    assert!(cached, "-I inc_a recompile should hit cache");
    assert_eq!(obj_a, obj_a2);

    h.shutdown().await;
}

/// Add -D flag → different cache entry. Remove -D → original entry.
#[tokio::test]
#[ignore] // integration: spawns clang, run with --full
async fn mutation_define_flag_toggle() {
    let mut h = match TestHarness::new().await {
        Some(h) => h,
        None => return,
    };

    h.write_file(
        "deftog.cpp",
        r#"
#ifdef FEATURE
int f() { return 1; }
#else
int f() { return 0; }
#endif
"#,
    );

    // Without -D
    let (_, cached, obj_no_d) = h.compile_file_read("deftog.cpp", "deftog.o").await;
    assert!(!cached);

    // With -DFEATURE
    let compiler = h.compiler_str();
    let (_, cached) = {
        let cwd = h.cwd();
        compile(
            &mut h.client,
            &h.session_id,
            &["-c", "deftog.cpp", "-o", "deftog.o", "-DFEATURE"],
            &cwd,
            &compiler,
        )
        .await
    };
    assert!(!cached, "-DFEATURE is a different cache key");
    let obj_with_d = std::fs::read(h.path("deftog.o")).unwrap();
    assert_ne!(obj_no_d, obj_with_d, "-DFEATURE → different .o");

    // Back to without -D — should hit original cache
    let _ = std::fs::remove_file(h.path("deftog.o"));
    let (_, cached, obj_no_d2) = h.compile_file_read("deftog.cpp", "deftog.o").await;
    assert!(cached, "recompile without -D should hit original cache");
    assert_eq!(obj_no_d, obj_no_d2);

    h.shutdown().await;
}
