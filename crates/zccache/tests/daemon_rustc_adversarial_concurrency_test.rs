//! Adversarial tests for rustc cache correctness — concurrency & cross-session/extern invalidation.
//!
//! Targets multi-session / multi-client scenarios and extern-crate cache keying:
//! - Thundering herd: concurrent identical compiles must agree
//! - Concurrent compilations with different externs
//! - Failed compile with missing extern (not served from failure)
//! - Extern crate content change invalidates downstream cache
//!
//! Companion files:
//! - `daemon_rustc_adversarial_mutations_test.rs` — source/flag mutations
//! - `daemon_rustc_adversarial_corner_cases_test.rs` — failed compiles, persistence, env vars, -Z flags
//!
//! Run all:    soldr cargo test -p zccache --test daemon_rustc_adversarial_concurrency_test -- --nocapture --ignored --test-threads=1
//! Run single: soldr cargo test -p zccache --test daemon_rustc_adversarial_concurrency_test -- <test_name> --nocapture --ignored

use zccache::core::NormalizedPath;
use zccache::daemon::DaemonServer;
use zccache::protocol::{Request, Response};

#[cfg(unix)]
type ClientConn = zccache::ipc::IpcConnection;
#[cfg(windows)]
type ClientConn = zccache::ipc::IpcClientConnection;

// ─── Helpers ────────────────────────────────────────────────────────────────

async fn start_daemon() -> (
    String,
    tokio::task::JoinHandle<()>,
    std::sync::Arc<tokio::sync::Notify>,
) {
    let endpoint = zccache::ipc::unique_test_endpoint();
    let mut server = DaemonServer::bind(&endpoint).unwrap();
    let shutdown = server.shutdown_handle();
    let handle = tokio::spawn(async move {
        server.run(0).await.unwrap();
    });
    (endpoint, handle, shutdown)
}

async fn do_start_session(client: &mut ClientConn, cwd: &str) -> String {
    client
        .send(&Request::SessionStart {
            client_pid: std::process::id(),
            working_dir: cwd.to_string().into(),
            log_file: None,
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

async fn compile(
    client: &mut ClientConn,
    session_id: &str,
    args: &[&str],
    cwd: &str,
    compiler: &str,
    env: Option<Vec<(String, String)>>,
) -> (i32, bool) {
    client
        .send(&Request::Compile {
            session_id: session_id.to_string(),
            args: args.iter().map(|s| s.to_string()).collect(),
            cwd: cwd.to_string().into(),
            compiler: compiler.to_string().into(),
            env,
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

/// Convenience harness: daemon + session + temp dir + rustc path.
/// Used by the error-handling test; concurrency tests drive the daemon directly.
struct TestHarness {
    rustc: NormalizedPath,
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
        let rustc = zccache::test_support::find_rustc()?;
        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.path().to_string_lossy().into_owned();
        let (endpoint, server_handle, shutdown) = start_daemon().await;
        let mut client = zccache::ipc::connect(&endpoint).await.unwrap();
        let session_id = do_start_session(&mut client, &cwd).await;
        Some(Self {
            rustc,
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
        self.tmp.path().join(name).into()
    }

    fn write_file(&self, name: &str, content: &str) -> NormalizedPath {
        let p = self.path(name);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&p, content).unwrap();
        p
    }

    fn rc(&self) -> String {
        self.rustc.to_string_lossy().into_owned()
    }

    /// Compile a .rs file to .rlib with default flags. Returns (exit_code, cached).
    async fn compile_lib(&mut self, src: &str, out: &str) -> (i32, bool) {
        let crate_name = src.trim_end_matches(".rs");
        let cwd = self.cwd();
        let rc = self.rc();
        compile(
            &mut self.client,
            &self.session_id,
            &[
                "--edition",
                "2021",
                "--crate-type",
                "lib",
                "--crate-name",
                crate_name,
                "--emit=link",
                src,
                "-o",
                out,
            ],
            &cwd,
            &rc,
            None,
        )
        .await
    }

    /// Compile with custom args. Returns (exit_code, cached).
    async fn compile_args(
        &mut self,
        args: &[&str],
        env: Option<Vec<(String, String)>>,
    ) -> (i32, bool) {
        let cwd = self.cwd();
        let rc = self.rc();
        compile(&mut self.client, &self.session_id, args, &cwd, &rc, env).await
    }

    async fn shutdown(self) {
        self.shutdown.notify_one();
        self.server_handle.await.unwrap();
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// CONCURRENCY
// ═══════════════════════════════════════════════════════════════════════════

/// Thundering herd: 4 concurrent sessions compile the same .rs file.
/// All must succeed and produce identical .rlib bytes.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore]
async fn rustc_thundering_herd_same_file() {
    let rustc = match zccache::test_support::find_rustc() {
        Some(p) => p,
        None => return,
    };

    let tmp = tempfile::tempdir().unwrap();
    let cwd = tmp.path().to_string_lossy().into_owned();
    std::fs::write(tmp.path().join("herd.rs"), "pub fn f() -> i32 { 42 }\n").unwrap();

    let (endpoint, server_handle, shutdown) = start_daemon().await;

    let n = 4;
    let mut handles = Vec::new();
    for i in 0..n {
        let ep = endpoint.clone();
        let rc = rustc.to_string_lossy().to_string();
        let cwd = cwd.clone();
        let out_dir = tmp.path().join(format!("out_{i}"));
        std::fs::create_dir_all(&out_dir).unwrap();

        handles.push(tokio::spawn(async move {
            let mut cl = zccache::ipc::connect(&ep).await.unwrap();
            let sid = do_start_session(&mut cl, &cwd).await;
            let out = format!("out_{i}/libherd.rlib");
            let (ec, cached) = compile(
                &mut cl,
                &sid,
                &[
                    "--edition",
                    "2021",
                    "--crate-type",
                    "lib",
                    "--crate-name",
                    "herd",
                    "--emit=link",
                    "herd.rs",
                    "-o",
                    &out,
                ],
                &cwd,
                &rc,
                None,
            )
            .await;
            let obj = std::fs::read(std::path::Path::new(&cwd).join(&out)).unwrap_or_default();
            (ec, cached, obj)
        }));
    }

    let mut results = Vec::new();
    for h in handles {
        results.push(h.await.unwrap());
    }

    // All must succeed
    for (i, (ec, _, _)) in results.iter().enumerate() {
        assert_eq!(*ec, 0, "session {i} must compile successfully");
    }

    // All must produce identical .rlib bytes
    let reference = &results[0].2;
    assert!(!reference.is_empty(), "reference .rlib must not be empty");
    for (i, (_, _, obj)) in results.iter().enumerate().skip(1) {
        assert_eq!(reference, obj, "session {i} .rlib must match session 0");
    }

    let miss_count = results.iter().filter(|(_, c, _)| !c).count();
    let hit_count = results.iter().filter(|(_, c, _)| *c).count();
    eprintln!("rustc thundering herd: {miss_count} misses, {hit_count} hits out of {n}");
    assert!(miss_count >= 1, "at least one must be a cache miss");

    shutdown.notify_one();
    server_handle.await.unwrap();
}

/// Concurrent compilations with different externs must produce different artifacts.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn rustc_concurrent_different_externs() {
    let rustc = match zccache::test_support::find_rustc() {
        Some(p) => p,
        None => return,
    };

    let tmp = tempfile::tempdir().unwrap();
    let cwd = tmp.path().to_string_lossy().into_owned();
    let rc = rustc.to_string_lossy().to_string();

    // Create two versions of crate A
    std::fs::write(tmp.path().join("a_v1.rs"), "pub fn val() -> i32 { 1 }\n").unwrap();
    std::fs::write(tmp.path().join("a_v2.rs"), "pub fn val() -> i32 { 2 }\n").unwrap();
    // Crate B depends on A
    std::fs::write(
        tmp.path().join("b.rs"),
        "extern crate a; pub fn double() -> i32 { a::val() * 2 }\n",
    )
    .unwrap();

    let (endpoint, server_handle, shutdown) = start_daemon().await;
    let mut cl = zccache::ipc::connect(&endpoint).await.unwrap();
    let sid = do_start_session(&mut cl, &cwd).await;

    // Compile A v1 and A v2
    let (ec, _) = compile(
        &mut cl,
        &sid,
        &[
            "--edition",
            "2021",
            "--crate-type",
            "lib",
            "--crate-name",
            "a",
            "--emit=link",
            "a_v1.rs",
            "-o",
            "liba_v1.rlib",
        ],
        &cwd,
        &rc,
        None,
    )
    .await;
    assert_eq!(ec, 0);
    let (ec, _) = compile(
        &mut cl,
        &sid,
        &[
            "--edition",
            "2021",
            "--crate-type",
            "lib",
            "--crate-name",
            "a",
            "--emit=link",
            "a_v2.rs",
            "-o",
            "liba_v2.rlib",
        ],
        &cwd,
        &rc,
        None,
    )
    .await;
    assert_eq!(ec, 0);

    // Compile B with extern a=v1
    let a_v1 = tmp
        .path()
        .join("liba_v1.rlib")
        .to_string_lossy()
        .to_string();
    let a_v2 = tmp
        .path()
        .join("liba_v2.rlib")
        .to_string_lossy()
        .to_string();
    let ext_v1 = format!("a={a_v1}");
    let ext_v2 = format!("a={a_v2}");

    let (ec, cached) = compile(
        &mut cl,
        &sid,
        &[
            "--edition",
            "2021",
            "--crate-type",
            "lib",
            "--crate-name",
            "b",
            "--emit=link",
            "--extern",
            &ext_v1,
            "b.rs",
            "-o",
            "libb.rlib",
        ],
        &cwd,
        &rc,
        None,
    )
    .await;
    assert_eq!(ec, 0);
    assert!(!cached, "B with extern v1 should be miss");
    let b_v1 = std::fs::read(tmp.path().join("libb.rlib")).unwrap();

    // Compile B with extern a=v2 — MUST be miss (different extern content)
    let (ec, cached) = compile(
        &mut cl,
        &sid,
        &[
            "--edition",
            "2021",
            "--crate-type",
            "lib",
            "--crate-name",
            "b",
            "--emit=link",
            "--extern",
            &ext_v2,
            "b.rs",
            "-o",
            "libb.rlib",
        ],
        &cwd,
        &rc,
        None,
    )
    .await;
    assert_eq!(ec, 0);
    assert!(
        !cached,
        "B with extern v2 must be miss (different extern content)"
    );
    let b_v2 = std::fs::read(tmp.path().join("libb.rlib")).unwrap();
    assert_ne!(b_v1, b_v2, "different extern → different .rlib");

    // Back to v1 → should hit
    std::fs::remove_file(tmp.path().join("libb.rlib")).unwrap();
    let (ec, cached) = compile(
        &mut cl,
        &sid,
        &[
            "--edition",
            "2021",
            "--crate-type",
            "lib",
            "--crate-name",
            "b",
            "--emit=link",
            "--extern",
            &ext_v1,
            "b.rs",
            "-o",
            "libb.rlib",
        ],
        &cwd,
        &rc,
        None,
    )
    .await;
    assert_eq!(ec, 0);
    assert!(cached, "B with extern v1 should hit cache");
    assert_eq!(b_v1, std::fs::read(tmp.path().join("libb.rlib")).unwrap());

    shutdown.notify_one();
    server_handle.await.unwrap();
}

// ═══════════════════════════════════════════════════════════════════════════
// ERROR HANDLING
// ═══════════════════════════════════════════════════════════════════════════

/// Compile with missing extern file → fails → NOT cached.
/// Create the extern, compile again → succeeds, miss (not served from failed attempt).
#[tokio::test]
#[ignore]
async fn rustc_missing_extern_not_cached() {
    let mut h = match TestHarness::new().await {
        Some(h) => h,
        None => return,
    };

    h.write_file(
        "needs_dep.rs",
        "extern crate dep; pub fn f() -> i32 { dep::val() }\n",
    );

    // Compile with missing extern → should fail
    let fake_dep = h.path("libdep.rlib").to_string_lossy().to_string();
    let ext_arg = format!("dep={fake_dep}");
    let (ec, cached) = h
        .compile_args(
            &[
                "--edition",
                "2021",
                "--crate-type",
                "lib",
                "--crate-name",
                "needs_dep",
                "--emit=link",
                "--extern",
                &ext_arg,
                "needs_dep.rs",
                "-o",
                "libneeds_dep.rlib",
            ],
            None,
        )
        .await;
    assert_ne!(ec, 0, "missing extern should fail");
    assert!(!cached, "failed compile must not be cached");

    // Create the extern
    h.write_file("dep.rs", "pub fn val() -> i32 { 42 }\n");
    let (ec, _) = h.compile_lib("dep.rs", "libdep.rlib").await;
    assert_eq!(ec, 0);

    // Now compile needs_dep again → should succeed, miss (NOT from failed attempt)
    let (ec, cached) = h
        .compile_args(
            &[
                "--edition",
                "2021",
                "--crate-type",
                "lib",
                "--crate-name",
                "needs_dep",
                "--emit=link",
                "--extern",
                &ext_arg,
                "needs_dep.rs",
                "-o",
                "libneeds_dep.rlib",
            ],
            None,
        )
        .await;
    assert_eq!(ec, 0, "should succeed now that extern exists");
    assert!(
        !cached,
        "should be cache miss (not served from failed attempt)"
    );

    // Third compile → should be cache HIT
    std::fs::remove_file(h.path("libneeds_dep.rlib")).unwrap();
    let (ec, cached) = h
        .compile_args(
            &[
                "--edition",
                "2021",
                "--crate-type",
                "lib",
                "--crate-name",
                "needs_dep",
                "--emit=link",
                "--extern",
                &ext_arg,
                "needs_dep.rs",
                "-o",
                "libneeds_dep.rlib",
            ],
            None,
        )
        .await;
    assert_eq!(ec, 0);
    assert!(cached, "third compile should hit cache");

    h.shutdown().await;
}

// ═══════════════════════════════════════════════════════════════════════════
// CROSS-SESSION + EXTERN INVALIDATION
// ═══════════════════════════════════════════════════════════════════════════

/// Cache persists across sessions, but extern crate content change invalidates.
#[tokio::test]
#[ignore]
async fn rustc_cache_persists_but_extern_change_invalidates() {
    let rustc = match zccache::test_support::find_rustc() {
        Some(p) => p,
        None => return,
    };
    let tmp = tempfile::tempdir().unwrap();
    let cwd = tmp.path().to_string_lossy().into_owned();
    let rc = rustc.to_string_lossy().into_owned();

    // Create crate A and B
    std::fs::write(tmp.path().join("a.rs"), "pub fn val() -> i32 { 1 }\n").unwrap();
    std::fs::write(
        tmp.path().join("b.rs"),
        "extern crate a; pub fn f() -> i32 { a::val() }\n",
    )
    .unwrap();

    let (endpoint, server_handle, shutdown) = start_daemon().await;

    // Session A: compile A, compile B with extern A → miss
    let mut cl_a = zccache::ipc::connect(&endpoint).await.unwrap();
    let sid_a = do_start_session(&mut cl_a, &cwd).await;

    let (ec, _) = compile(
        &mut cl_a,
        &sid_a,
        &[
            "--edition",
            "2021",
            "--crate-type",
            "lib",
            "--crate-name",
            "a",
            "--emit=link",
            "a.rs",
            "-o",
            "liba.rlib",
        ],
        &cwd,
        &rc,
        None,
    )
    .await;
    assert_eq!(ec, 0);

    let ext_a = format!("a={}", tmp.path().join("liba.rlib").to_string_lossy());
    let (ec, cached) = compile(
        &mut cl_a,
        &sid_a,
        &[
            "--edition",
            "2021",
            "--crate-type",
            "lib",
            "--crate-name",
            "b",
            "--emit=link",
            "--extern",
            &ext_a,
            "b.rs",
            "-o",
            "libb.rlib",
        ],
        &cwd,
        &rc,
        None,
    )
    .await;
    assert_eq!(ec, 0);
    assert!(!cached, "session A: B should be miss");

    // End session A
    cl_a.send(&Request::SessionEnd { session_id: sid_a })
        .await
        .unwrap();
    let _ = cl_a.recv::<Response>().await;

    // Change A's source and recompile A (changes liba.rlib content)
    std::thread::sleep(std::time::Duration::from_millis(1100));
    std::fs::write(tmp.path().join("a.rs"), "pub fn val() -> i32 { 99 }\n").unwrap();

    let mut cl_mid = zccache::ipc::connect(&endpoint).await.unwrap();
    let sid_mid = do_start_session(&mut cl_mid, &cwd).await;
    let (ec, _) = compile(
        &mut cl_mid,
        &sid_mid,
        &[
            "--edition",
            "2021",
            "--crate-type",
            "lib",
            "--crate-name",
            "a",
            "--emit=link",
            "a.rs",
            "-o",
            "liba.rlib",
        ],
        &cwd,
        &rc,
        None,
    )
    .await;
    assert_eq!(ec, 0);
    cl_mid
        .send(&Request::SessionEnd {
            session_id: sid_mid,
        })
        .await
        .unwrap();
    let _ = cl_mid.recv::<Response>().await;

    // Session B: compile B with extern A → must be MISS (extern content changed)
    let mut cl_b = zccache::ipc::connect(&endpoint).await.unwrap();
    let sid_b = do_start_session(&mut cl_b, &cwd).await;

    std::fs::remove_file(tmp.path().join("libb.rlib")).unwrap();
    let (ec, cached) = compile(
        &mut cl_b,
        &sid_b,
        &[
            "--edition",
            "2021",
            "--crate-type",
            "lib",
            "--crate-name",
            "b",
            "--emit=link",
            "--extern",
            &ext_a,
            "b.rs",
            "-o",
            "libb.rlib",
        ],
        &cwd,
        &rc,
        None,
    )
    .await;
    assert_eq!(ec, 0);
    assert!(
        !cached,
        "session B: B must be miss (extern A changed between sessions)"
    );

    shutdown.notify_one();
    server_handle.await.unwrap();
}
