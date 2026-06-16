//! Adversarial tests for rustc cache correctness — source mutations & flag differentiation.
//!
//! Targets Rust-specific edge cases where a *mutation* of either the source file
//! or the compiler flags must (or must not) re-key the cache:
//! - Source mutations (touch, delete-recreate, add unrelated file)
//! - Flag differentiation (--cfg, --edition, -C opt-level)
//!
//! Companion files:
//! - `daemon_rustc_adversarial_corner_cases_test.rs` — failed compiles, persistence, env vars, -Z flags
//! - `daemon_rustc_adversarial_concurrency_test.rs` — thundering herd, externs, error handling
//!
//! Run all:    soldr cargo test -p zccache --test daemon_rustc_adversarial_mutations_test -- --nocapture --ignored --test-threads=1
//! Run single: soldr cargo test -p zccache --test daemon_rustc_adversarial_mutations_test -- <test_name> --nocapture --ignored

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

    /// Compile and read the output file bytes.
    async fn compile_and_read(&mut self, src: &str, out: &str) -> (i32, bool, Vec<u8>) {
        let (ec, cached) = self.compile_lib(src, out).await;
        let data = if self.path(out).exists() {
            std::fs::read(self.path(out)).unwrap()
        } else {
            vec![]
        };
        (ec, cached, data)
    }

    async fn shutdown(self) {
        self.shutdown.notify_one();
        self.server_handle.await.unwrap();
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// SOURCE FILE MUTATIONS
// ═══════════════════════════════════════════════════════════════════════════

/// Touch source (change mtime, same content) → cache should still HIT.
#[tokio::test]
#[ignore]
async fn rustc_mutation_touch_source_no_invalidation() {
    let mut h = match TestHarness::new().await {
        Some(h) => h,
        None => return,
    };

    h.write_file("touch.rs", "pub fn f() -> i32 { 42 }\n");
    let (_, cached, obj_v1) = h.compile_and_read("touch.rs", "libtouch.rlib").await;
    assert!(!cached);

    // Touch: rewrite identical content (changes mtime)
    std::thread::sleep(std::time::Duration::from_millis(1100));
    h.write_file("touch.rs", "pub fn f() -> i32 { 42 }\n");

    std::fs::remove_file(h.path("libtouch.rlib")).unwrap();
    let (_, cached, obj_v2) = h.compile_and_read("touch.rs", "libtouch.rlib").await;
    assert!(cached, "touch with same content should still hit cache");
    assert_eq!(obj_v1, obj_v2, "same content → same .rlib");

    h.shutdown().await;
}

/// Delete source, recreate with SAME content → should still hit.
#[tokio::test]
#[ignore]
async fn rustc_mutation_delete_recreate_same_content() {
    let mut h = match TestHarness::new().await {
        Some(h) => h,
        None => return,
    };

    let content = "pub fn f() -> i32 { 42 }\n";
    h.write_file("same.rs", content);
    let (_, cached, obj_v1) = h.compile_and_read("same.rs", "libsame.rlib").await;
    assert!(!cached);

    std::fs::remove_file(h.path("same.rs")).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(1100));
    h.write_file("same.rs", content);

    std::fs::remove_file(h.path("libsame.rlib")).unwrap();
    let (_, cached, obj_v2) = h.compile_and_read("same.rs", "libsame.rlib").await;
    assert!(cached, "delete+recreate with same content should hit");
    assert_eq!(obj_v1, obj_v2);

    h.shutdown().await;
}

/// Adding an unrelated file doesn't affect existing caches.
#[tokio::test]
#[ignore]
async fn rustc_mutation_add_new_file_no_interference() {
    let mut h = match TestHarness::new().await {
        Some(h) => h,
        None => return,
    };

    h.write_file("existing.rs", "pub fn f() -> i32 { 1 }\n");
    let (_, cached, obj_existing) = h.compile_and_read("existing.rs", "libexisting.rlib").await;
    assert!(!cached);

    // Add a brand new unrelated file and compile it
    h.write_file("brand_new.rs", "pub fn g() -> i32 { 2 }\n");
    let (_, cached, _) = h
        .compile_and_read("brand_new.rs", "libbrand_new.rlib")
        .await;
    assert!(!cached, "brand new file should miss");

    // Existing file should still hit
    std::fs::remove_file(h.path("libexisting.rlib")).unwrap();
    let (_, cached, obj_again) = h.compile_and_read("existing.rs", "libexisting.rlib").await;
    assert!(
        cached,
        "existing file should still hit after adding new file"
    );
    assert_eq!(obj_existing, obj_again);

    h.shutdown().await;
}

// ═══════════════════════════════════════════════════════════════════════════
// FLAG DIFFERENTIATION
// ═══════════════════════════════════════════════════════════════════════════

/// --cfg flag change → different cache entries. Removing → original entry.
#[tokio::test]
#[ignore]
async fn rustc_mutation_cfg_flag_toggle() {
    let mut h = match TestHarness::new().await {
        Some(h) => h,
        None => return,
    };

    h.write_file(
        "cfg.rs",
        r#"
#[cfg(feature = "x")]
pub fn f() -> i32 { 1 }
#[cfg(not(feature = "x"))]
pub fn f() -> i32 { 0 }
"#,
    );

    // Without --cfg
    let (ec, cached) = h
        .compile_args(
            &[
                "--edition",
                "2021",
                "--crate-type",
                "lib",
                "--crate-name",
                "cfg",
                "--emit=link",
                "cfg.rs",
                "-o",
                "libcfg.rlib",
            ],
            None,
        )
        .await;
    assert_eq!(ec, 0);
    assert!(!cached);
    let obj_no_cfg = std::fs::read(h.path("libcfg.rlib")).unwrap();

    // With --cfg feature="x"
    let (ec, cached) = h
        .compile_args(
            &[
                "--edition",
                "2021",
                "--crate-type",
                "lib",
                "--crate-name",
                "cfg",
                "--emit=link",
                "--cfg",
                "feature=\"x\"",
                "cfg.rs",
                "-o",
                "libcfg.rlib",
            ],
            None,
        )
        .await;
    assert_eq!(ec, 0);
    assert!(!cached, "--cfg change must create different cache entry");
    let obj_with_cfg = std::fs::read(h.path("libcfg.rlib")).unwrap();
    assert_ne!(obj_no_cfg, obj_with_cfg, "--cfg → different .rlib");

    // Back to without --cfg → should hit original cache
    std::fs::remove_file(h.path("libcfg.rlib")).unwrap();
    let (_, cached) = h
        .compile_args(
            &[
                "--edition",
                "2021",
                "--crate-type",
                "lib",
                "--crate-name",
                "cfg",
                "--emit=link",
                "cfg.rs",
                "-o",
                "libcfg.rlib",
            ],
            None,
        )
        .await;
    assert!(cached, "recompile without --cfg should hit original cache");
    let obj_no_cfg2 = std::fs::read(h.path("libcfg.rlib")).unwrap();
    assert_eq!(obj_no_cfg, obj_no_cfg2);

    h.shutdown().await;
}

/// Different --edition → different cache entries.
#[tokio::test]
#[ignore]
async fn rustc_mutation_edition_change() {
    let mut h = match TestHarness::new().await {
        Some(h) => h,
        None => return,
    };

    h.write_file("ed.rs", "pub fn f() -> i32 { 42 }\n");

    let (ec, cached) = h
        .compile_args(
            &[
                "--edition",
                "2021",
                "--crate-type",
                "lib",
                "--crate-name",
                "ed",
                "--emit=link",
                "ed.rs",
                "-o",
                "libed.rlib",
            ],
            None,
        )
        .await;
    assert_eq!(ec, 0);
    assert!(!cached);

    // Different edition
    let (ec, cached) = h
        .compile_args(
            &[
                "--edition",
                "2024",
                "--crate-type",
                "lib",
                "--crate-name",
                "ed",
                "--emit=link",
                "ed.rs",
                "-o",
                "libed.rlib",
            ],
            None,
        )
        .await;
    assert_eq!(ec, 0);
    assert!(!cached, "different --edition must be different cache entry");

    // Back to 2021
    std::fs::remove_file(h.path("libed.rlib")).unwrap();
    let (_, cached) = h
        .compile_args(
            &[
                "--edition",
                "2021",
                "--crate-type",
                "lib",
                "--crate-name",
                "ed",
                "--emit=link",
                "ed.rs",
                "-o",
                "libed.rlib",
            ],
            None,
        )
        .await;
    assert!(cached, "edition 2021 should hit original cache");

    h.shutdown().await;
}

/// Different -C opt-level → different cache entries, different output.
#[tokio::test]
#[ignore]
async fn rustc_mutation_opt_level_change() {
    let mut h = match TestHarness::new().await {
        Some(h) => h,
        None => return,
    };

    h.write_file("opt.rs", "pub fn f(n: i32) -> i32 { n * n + n * 3 + 1 }\n");

    let (ec, cached) = h
        .compile_args(
            &[
                "--edition",
                "2021",
                "--crate-type",
                "lib",
                "--crate-name",
                "opt",
                "--emit=link",
                "-C",
                "opt-level=0",
                "opt.rs",
                "-o",
                "libopt.rlib",
            ],
            None,
        )
        .await;
    assert_eq!(ec, 0);
    assert!(!cached);
    let obj_debug = std::fs::read(h.path("libopt.rlib")).unwrap();

    let (ec, cached) = h
        .compile_args(
            &[
                "--edition",
                "2021",
                "--crate-type",
                "lib",
                "--crate-name",
                "opt",
                "--emit=link",
                "-C",
                "opt-level=3",
                "opt.rs",
                "-o",
                "libopt.rlib",
            ],
            None,
        )
        .await;
    assert_eq!(ec, 0);
    assert!(!cached, "different opt-level must be different cache entry");
    let obj_release = std::fs::read(h.path("libopt.rlib")).unwrap();
    assert_ne!(obj_debug, obj_release, "opt-level=0 vs 3 → different .rlib");

    // Back to opt-level=0
    std::fs::remove_file(h.path("libopt.rlib")).unwrap();
    let (_, cached) = h
        .compile_args(
            &[
                "--edition",
                "2021",
                "--crate-type",
                "lib",
                "--crate-name",
                "opt",
                "--emit=link",
                "-C",
                "opt-level=0",
                "opt.rs",
                "-o",
                "libopt.rlib",
            ],
            None,
        )
        .await;
    assert!(cached, "opt-level=0 recompile should hit original cache");
    assert_eq!(obj_debug, std::fs::read(h.path("libopt.rlib")).unwrap());

    h.shutdown().await;
}
