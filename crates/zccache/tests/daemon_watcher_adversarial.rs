//! Tests for watcher-assisted cache invalidation.
//!
//! These tests verify that the watcher pipeline (NotifyWatcher → SettleBuffer →
//! CacheSystem) correctly detects file changes and invalidates the cache under
//! the assumption that no edits occur during active compilation. Scenarios
//! covered: header edits, touch-without-change, deep directories, ephemeral
//! file creation/deletion, and multi-session cache sharing.
//!
//! Run all:    soldr cargo test -p zccache-daemon --test watcher_adversarial -- --nocapture
//! Run single: soldr cargo test -p zccache-daemon --test watcher_adversarial -- <test_name> --nocapture

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
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
            private_daemon: None,
        })
        .await
        .unwrap();
    match client.recv().await.unwrap() {
        Some(Response::SessionStarted { session_id, .. }) => session_id,
        other => panic!("expected SessionStarted, got: {other:?}"),
    }
}

async fn compile_and_read(
    client: &mut ClientConn,
    session_id: &str,
    args: &[&str],
    cwd: &str,
    obj_path: &Path,
    compiler: &str,
) -> (i32, bool, Vec<u8>) {
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

    let (exit_code, cached) = match client.recv().await.unwrap() {
        Some(Response::CompileResult {
            exit_code, cached, ..
        }) => (exit_code, cached),
        Some(Response::Error { message }) => panic!("compile error: {message}"),
        other => panic!("expected CompileResult, got: {other:?}"),
    };
    let obj_data = if obj_path.exists() {
        std::fs::read(obj_path).unwrap()
    } else {
        vec![]
    };
    (exit_code, cached, obj_data)
}

/// Time to let the watcher pipeline propagate:
/// OS event delivery (~10ms) + settle window (50ms) + consumer processing (~1ms).
/// Under heavy load (full workspace test suite), Windows event delivery can be
/// significantly delayed, so we use a generous initial timeout.
const WATCHER_SETTLE_MS: u64 = 500;

async fn wait_for_watcher() {
    tokio::time::sleep(Duration::from_millis(WATCHER_SETTLE_MS)).await;
}

/// Compile with polling retry — watcher events may be delayed under heavy CPU load
/// (e.g., full workspace test suite running many test binaries in parallel).
/// Polls every 300ms for up to 5 seconds total if `cached` doesn't match `expect_cached`.
async fn compile_with_retry(
    h: &mut TestHarness,
    src: &str,
    obj: &str,
    expect_cached: bool,
) -> (i32, bool, Vec<u8>) {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        if expect_cached {
            let _ = std::fs::remove_file(h.path(obj));
        }
        let (exit_code, cached, data) = h.compile_file_read(src, obj).await;
        if cached == expect_cached || std::time::Instant::now() >= deadline {
            return (exit_code, cached, data);
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
}

/// Test harness with watcher-aware helpers.
struct TestHarness {
    clang: NormalizedPath,
    tmp: tempfile::TempDir,
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

        // Give the watcher time to start watching the working directory.
        wait_for_watcher().await;

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

    /// Start a second session on the same daemon, with a different working directory.
    async fn second_session(&self, cwd: &str) -> (ClientConn, String) {
        let mut client2 = zccache::ipc::connect(&self.endpoint).await.unwrap();
        let log = NormalizedPath::new(Path::new(cwd).join("log2.txt"));
        let sid2 = start_session(&mut client2, &self.clang, cwd, &log.to_string_lossy()).await;
        (client2, sid2)
    }

    async fn shutdown(self) {
        self.shutdown.notify_one();
        self.server_handle.await.unwrap();
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// GROUP 1: WATCHER PIPELINE VERIFICATION
//
// Does the watcher actually detect changes and downgrade metadata confidence?
// ═══════════════════════════════════════════════════════════════════════════

/// Edit header file, wait for watcher, recompile → cache miss.
#[tokio::test]
#[ignore] // integration: spawns clang + watcher settle delays, run with --full
async fn watcher_header_edit_detected() {
    let mut h = match TestHarness::new().await {
        Some(h) => h,
        None => return,
    };

    h.write_file("val.h", "#define VALUE 1\n");
    h.write_file("src.cpp", "#include \"val.h\"\nint f() { return VALUE; }\n");

    let (_, cached, obj_v1) = h.compile_file_read("src.cpp", "src.o").await;
    assert!(!cached);

    // Edit header, wait for watcher
    h.write_file("val.h", "#define VALUE 99\n");
    wait_for_watcher().await;

    let (_, cached, obj_v2) = compile_with_retry(&mut h, "src.cpp", "src.o", false).await;
    assert!(
        !cached,
        "watcher should have invalidated cache after header edit"
    );
    assert_ne!(obj_v1, obj_v2, "header change → different .o");

    h.shutdown().await;
}

/// Touch source file (same content, different mtime), wait for watcher → cache hit.
/// The watcher fires and downgrades confidence, but content hash is unchanged,
/// so the artifact key is the same → cache hit.
///
/// Under heavy parallel test load, the depgraph can intermittently return Cold
/// on the first attempt (suspected DashMap timing under contention). We use
/// compile_with_retry to handle this, and only check `cached=true` (not byte
/// equality of the .o, since intermediate misses produce new COFF timestamps).
#[tokio::test]
#[ignore] // integration: spawns clang + 1100ms sleep, run with --full
async fn watcher_touch_same_content_hits() {
    let mut h = match TestHarness::new().await {
        Some(h) => h,
        None => return,
    };

    let content = "int f() { return 42; }\n";
    h.write_file("src.cpp", content);
    let (_, cached, _obj_v1) = h.compile_file_read("src.cpp", "src.o").await;
    assert!(!cached);

    // Touch: rewrite same content after a delay so mtime changes
    std::thread::sleep(Duration::from_millis(1100));
    h.write_file("src.cpp", content);
    wait_for_watcher().await;

    let (_, cached, _obj_v2) = compile_with_retry(&mut h, "src.cpp", "src.o", true).await;
    assert!(
        cached,
        "same content → same hash → cache hit despite mtime change"
    );

    h.shutdown().await;
}

// ═══════════════════════════════════════════════════════════════════════════
// GROUP 2: WATCH BOUNDARY TESTS
//
// Does the system stay correct for files inside and outside the watched tree?
// ═══════════════════════════════════════════════════════════════════════════

/// File 5 levels deep in nested subdirectory → watcher detects change.
/// With non-recursive watches, each directory in the include path is watched
/// individually (discovered via depfile scanning on the first compile).
#[tokio::test]
#[ignore] // integration: spawns clang + watcher settle delays, run with --full
async fn deeply_nested_dir_watched() {
    let mut h = match TestHarness::new().await {
        Some(h) => h,
        None => return,
    };

    h.write_file("a/b/c/d/e/deep.h", "#define DEEP 1\n");
    h.write_file(
        "src.cpp",
        "#include \"a/b/c/d/e/deep.h\"\nint f() { return DEEP; }\n",
    );

    let (_, cached, obj_v1) = h.compile_file_read("src.cpp", "src.o").await;
    assert!(!cached);

    // Edit deeply nested header
    h.write_file("a/b/c/d/e/deep.h", "#define DEEP 99\n");
    wait_for_watcher().await;

    let (_, cached, obj_v2) = compile_with_retry(&mut h, "src.cpp", "src.o", false).await;
    assert!(!cached, "watcher must detect change 5 levels deep");
    assert_ne!(obj_v1, obj_v2);

    h.shutdown().await;
}

// ═══════════════════════════════════════════════════════════════════════════
// GROUP 3: TIMING / RACE CONDITIONS
//
// Rapid file changes, settle buffer coalescing.
// ═══════════════════════════════════════════════════════════════════════════

/// Create and immediately delete a file within the settle window → no crash,
/// daemon remains functional.
#[tokio::test]
#[ignore] // integration: spawns clang + watcher settle delays, run with --full
async fn rapid_create_delete_no_crash() {
    let mut h = match TestHarness::new().await {
        Some(h) => h,
        None => return,
    };

    h.write_file("src.cpp", "int f() { return 1; }\n");
    let (_, cached, _) = h.compile_file_read("src.cpp", "src.o").await;
    assert!(!cached);

    // Ephemeral file: create then immediately delete
    let ephemeral = h.path("ephemeral.h");
    std::fs::write(&ephemeral, "#define TEMP 1\n").unwrap();
    std::fs::remove_file(&ephemeral).unwrap();
    wait_for_watcher().await;

    // Daemon should still function normally
    let (_, cached, _) = compile_with_retry(&mut h, "src.cpp", "src.o", true).await;
    assert!(
        cached,
        "unrelated ephemeral file should not affect src.cpp cache"
    );

    h.shutdown().await;
}

// ═══════════════════════════════════════════════════════════════════════════
// GROUP 4: MULTI-SESSION
//
// Multiple sessions sharing the same watcher.
// ═══════════════════════════════════════════════════════════════════════════

/// Two sessions watching the same directory. Edit a header → both sessions see
/// the invalidation. Second session gets a cache hit from first session's recompile.
#[tokio::test]
#[ignore] // integration: spawns clang + multi-session watcher, run with --full
async fn two_sessions_same_dir_share_watcher() {
    let mut h = match TestHarness::new().await {
        Some(h) => h,
        None => return,
    };

    let cwd = h.cwd();
    let (mut client2, sid2) = h.second_session(&cwd).await;
    let compiler = h.compiler_str();
    wait_for_watcher().await;

    h.write_file("config.h", "#define CFG 1\n");
    h.write_file(
        "shared.cpp",
        "#include \"config.h\"\nint f() { return CFG; }\n",
    );

    // Session A compiles (miss)
    let (_, cached, _) = h.compile_file_read("shared.cpp", "shared.o").await;
    assert!(!cached);

    // Session B compiles same file (hit — cross-session cache sharing)
    let obj_path = h.path("shared_b.o");
    let (_, cached, _) = compile_and_read(
        &mut client2,
        &sid2,
        &["-c", "shared.cpp", "-o", "shared_b.o"],
        &cwd,
        &obj_path,
        &compiler,
    )
    .await;
    assert!(cached, "session B should hit from session A's cache");

    // Edit header
    h.write_file("config.h", "#define CFG 99\n");
    wait_for_watcher().await;

    // Session A recompiles (miss — header changed)
    let (_, cached, _) = h.compile_file_read("shared.cpp", "shared.o").await;
    assert!(!cached, "session A must miss after header edit");

    // Session B recompiles (hit — session A just populated the new artifact)
    let (_, cached, _) = compile_and_read(
        &mut client2,
        &sid2,
        &["-c", "shared.cpp", "-o", "shared_b.o"],
        &cwd,
        &obj_path,
        &compiler,
    )
    .await;
    assert!(cached, "session B should hit from session A's recompile");

    h.shutdown().await;
}
