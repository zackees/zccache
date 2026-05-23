//! Adversarial tests for rustc cache correctness.
//!
//! Mirrors the C++ adversarial test suites (`adversarial_mutations.rs`,
//! `adversarial_corner_cases.rs`) but targets Rust-specific edge cases:
//! - Source mutations (touch, delete-recreate, add unrelated file)
//! - Flag differentiation (--cfg, --edition, -C opt-level)
//! - Failed compile not cached
//! - Cache persistence across sessions
//! - CARGO_* env vars in cache key
//! - --remap-path-prefix in cache key
//!
//! Run all:    soldr cargo test -p zccache-daemon --test rustc_adversarial_test -- --nocapture --ignored --test-threads=1
//! Run single: soldr cargo test -p zccache-daemon --test rustc_adversarial_test -- <test_name> --nocapture --ignored

use zccache_monocrate::core::NormalizedPath;
use zccache_monocrate::daemon::DaemonServer;
use zccache_monocrate::protocol::{Request, Response};

#[cfg(unix)]
type ClientConn = zccache_monocrate::ipc::IpcConnection;
#[cfg(windows)]
type ClientConn = zccache_monocrate::ipc::IpcClientConnection;

// ─── Helpers ────────────────────────────────────────────────────────────────

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

async fn do_start_session(client: &mut ClientConn, cwd: &str) -> String {
    client
        .send(&Request::SessionStart {
            client_pid: std::process::id(),
            working_dir: cwd.to_string().into(),
            log_file: None,
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
        let rustc = zccache_monocrate::test_support::find_rustc()?;
        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.path().to_string_lossy().into_owned();
        let (endpoint, server_handle, shutdown) = start_daemon().await;
        let mut client = zccache_monocrate::ipc::connect(&endpoint).await.unwrap();
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

// ═══════════════════════════════════════════════════════════════════════════
// CORNER CASES
// ═══════════════════════════════════════════════════════════════════════════

/// Syntax error → compile fails → NOT cached.
/// Fix error → succeeds → cached on third compile.
#[tokio::test]
#[ignore]
async fn rustc_failed_compile_not_cached() {
    let mut h = match TestHarness::new().await {
        Some(h) => h,
        None => return,
    };

    // Syntax error
    h.write_file("broken.rs", "pub fn f() -> i32 { NOPE }\n");
    let (ec, cached) = h.compile_lib("broken.rs", "libbroken.rlib").await;
    assert_ne!(ec, 0, "syntax error should fail");
    assert!(!cached, "failed compile must not be cached");

    // Fix the error
    h.write_file("broken.rs", "pub fn f() -> i32 { 42 }\n");
    let (ec, cached) = h.compile_lib("broken.rs", "libbroken.rlib").await;
    assert_eq!(ec, 0);
    assert!(!cached, "first successful compile should be miss");

    // Third compile → hit
    std::fs::remove_file(h.path("libbroken.rlib")).unwrap();
    let (ec, cached) = h.compile_lib("broken.rs", "libbroken.rlib").await;
    assert_eq!(ec, 0);
    assert!(cached, "third compile should hit cache");

    h.shutdown().await;
}

/// Cache survives session boundaries.
#[tokio::test]
#[ignore]
async fn rustc_cache_persists_across_sessions() {
    let rustc = match zccache_monocrate::test_support::find_rustc() {
        Some(p) => p,
        None => return,
    };
    let tmp = tempfile::tempdir().unwrap();
    let cwd = tmp.path().to_string_lossy().into_owned();
    let rc = rustc.to_string_lossy().into_owned();
    std::fs::write(tmp.path().join("sess.rs"), "pub fn f() -> i32 { 42 }\n").unwrap();

    let (endpoint, server_handle, shutdown) = start_daemon().await;

    // Session A: compile → miss
    let mut client_a = zccache_monocrate::ipc::connect(&endpoint).await.unwrap();
    let sid_a = do_start_session(&mut client_a, &cwd).await;
    let (ec, cached) = compile(
        &mut client_a,
        &sid_a,
        &[
            "--edition",
            "2021",
            "--crate-type",
            "lib",
            "--crate-name",
            "sess",
            "--emit=link",
            "sess.rs",
            "-o",
            "libsess.rlib",
        ],
        &cwd,
        &rc,
        None,
    )
    .await;
    assert_eq!(ec, 0);
    assert!(!cached, "session A first compile should be miss");

    // End session A
    client_a
        .send(&Request::SessionEnd {
            session_id: sid_a.clone(),
        })
        .await
        .unwrap();
    let _ = client_a.recv::<Response>().await;

    // Session B: same file → hit
    let mut client_b = zccache_monocrate::ipc::connect(&endpoint).await.unwrap();
    let sid_b = do_start_session(&mut client_b, &cwd).await;
    std::fs::remove_file(tmp.path().join("libsess.rlib")).unwrap();
    let (ec, cached) = compile(
        &mut client_b,
        &sid_b,
        &[
            "--edition",
            "2021",
            "--crate-type",
            "lib",
            "--crate-name",
            "sess",
            "--emit=link",
            "sess.rs",
            "-o",
            "libsess.rlib",
        ],
        &cwd,
        &rc,
        None,
    )
    .await;
    assert_eq!(ec, 0);
    assert!(cached, "session B should hit cache from session A");

    shutdown.notify_one();
    server_handle.await.unwrap();
}

// ═══════════════════════════════════════════════════════════════════════════
// RUST-SPECIFIC ATTACK SURFACES
// ═══════════════════════════════════════════════════════════════════════════

/// CARGO_* env vars must affect the cache key.
/// Source uses env!("CARGO_PKG_VERSION"), so different env → different binary.
#[tokio::test]
#[ignore]
async fn rustc_env_vars_affect_cache_key() {
    let mut h = match TestHarness::new().await {
        Some(h) => h,
        None => return,
    };

    // Source that embeds CARGO_PKG_VERSION at compile time
    h.write_file(
        "envtest.rs",
        r#"pub fn version() -> &'static str { env!("CARGO_PKG_VERSION") }"#,
    );

    let args_base = &[
        "--edition",
        "2021",
        "--crate-type",
        "lib",
        "--crate-name",
        "envtest",
        "--emit=link",
        "envtest.rs",
        "-o",
        "libenvtest.rlib",
    ];

    // Compile with CARGO_PKG_VERSION=1.0.0
    let env_v1: Vec<(String, String)> = vec![("CARGO_PKG_VERSION".into(), "1.0.0".into())];
    let (ec, cached) = h.compile_args(args_base, Some(env_v1.clone())).await;
    assert_eq!(ec, 0);
    assert!(!cached, "first compile should be miss");
    let obj_v1 = std::fs::read(h.path("libenvtest.rlib")).unwrap();

    // Compile with CARGO_PKG_VERSION=2.0.0 — MUST be a miss (different env)
    let env_v2: Vec<(String, String)> = vec![("CARGO_PKG_VERSION".into(), "2.0.0".into())];
    let (ec, cached) = h.compile_args(args_base, Some(env_v2)).await;
    assert_eq!(ec, 0);
    assert!(
        !cached,
        "different CARGO_PKG_VERSION MUST produce cache miss (false hit = correctness bug)"
    );
    let obj_v2 = std::fs::read(h.path("libenvtest.rlib")).unwrap();
    assert_ne!(
        obj_v1, obj_v2,
        "different CARGO_PKG_VERSION → different .rlib content"
    );

    // Back to v1 → should hit original
    std::fs::remove_file(h.path("libenvtest.rlib")).unwrap();
    let (ec, cached) = h.compile_args(args_base, Some(env_v1)).await;
    assert_eq!(ec, 0);
    assert!(cached, "CARGO_PKG_VERSION=1.0.0 should hit original cache");
    assert_eq!(obj_v1, std::fs::read(h.path("libenvtest.rlib")).unwrap());

    h.shutdown().await;
}

/// --remap-path-prefix changes embedded paths in the binary.
#[tokio::test]
#[ignore]
async fn rustc_remap_path_prefix_affects_cache_key() {
    let mut h = match TestHarness::new().await {
        Some(h) => h,
        None => return,
    };

    h.write_file("remap.rs", "pub fn f() -> i32 { 42 }\n");

    // Without remap
    let (ec, cached) = h
        .compile_args(
            &[
                "--edition",
                "2021",
                "--crate-type",
                "lib",
                "--crate-name",
                "remap",
                "--emit=link",
                "remap.rs",
                "-o",
                "libremap.rlib",
            ],
            None,
        )
        .await;
    assert_eq!(ec, 0);
    assert!(!cached);
    let obj_no_remap = std::fs::read(h.path("libremap.rlib")).unwrap();

    // With remap
    let (ec, cached) = h
        .compile_args(
            &[
                "--edition",
                "2021",
                "--crate-type",
                "lib",
                "--crate-name",
                "remap",
                "--emit=link",
                "--remap-path-prefix",
                "/home/user=/src",
                "remap.rs",
                "-o",
                "libremap.rlib",
            ],
            None,
        )
        .await;
    assert_eq!(ec, 0);
    assert!(
        !cached,
        "--remap-path-prefix MUST produce cache miss (changes binary content)"
    );

    // Back to without remap → should hit original
    std::fs::remove_file(h.path("libremap.rlib")).unwrap();
    let (_, cached) = h
        .compile_args(
            &[
                "--edition",
                "2021",
                "--crate-type",
                "lib",
                "--crate-name",
                "remap",
                "--emit=link",
                "remap.rs",
                "-o",
                "libremap.rlib",
            ],
            None,
        )
        .await;
    assert!(cached, "no-remap recompile should hit original cache");
    assert_eq!(
        obj_no_remap,
        std::fs::read(h.path("libremap.rlib")).unwrap()
    );

    h.shutdown().await;
}

/// -Z flag with value must be in cache key (not silently dropped).
#[tokio::test]
#[ignore]
async fn rustc_z_flag_with_value_in_cache_key() {
    let mut h = match TestHarness::new().await {
        Some(h) => h,
        None => return,
    };

    h.write_file("zflag.rs", "pub fn f() -> i32 { 42 }\n");

    // Without -Z flag
    let (ec, cached) = h
        .compile_args(
            &[
                "--edition",
                "2021",
                "--crate-type",
                "lib",
                "--crate-name",
                "zflag",
                "--emit=link",
                "zflag.rs",
                "-o",
                "libzflag.rlib",
            ],
            None,
        )
        .await;
    assert_eq!(ec, 0);
    assert!(!cached);

    // With -Z flag and value — cache entry must differ from above.
    // -Z flags require nightly rustc, so we don't assert on exit_code.
    // We only verify the cache key is different (not a false hit from above).
    let (_, cached) = h
        .compile_args(
            &[
                "--edition",
                "2021",
                "--crate-type",
                "lib",
                "--crate-name",
                "zflag",
                "--emit=link",
                "-Z",
                "macro-backtrace",
                "zflag.rs",
                "-o",
                "libzflag.rlib",
            ],
            None,
        )
        .await;
    assert!(
        !cached,
        "-Z flag with value must create different cache entry"
    );

    // Back to without -Z → should hit original
    std::fs::remove_file(h.path("libzflag.rlib")).unwrap();
    let (_, cached) = h
        .compile_args(
            &[
                "--edition",
                "2021",
                "--crate-type",
                "lib",
                "--crate-name",
                "zflag",
                "--emit=link",
                "zflag.rs",
                "-o",
                "libzflag.rlib",
            ],
            None,
        )
        .await;
    assert!(cached, "without -Z should hit original cache");

    h.shutdown().await;
}

// ═══════════════════════════════════════════════════════════════════════════
// CONCURRENCY
// ═══════════════════════════════════════════════════════════════════════════

/// Thundering herd: 4 concurrent sessions compile the same .rs file.
/// All must succeed and produce identical .rlib bytes.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore]
async fn rustc_thundering_herd_same_file() {
    let rustc = match zccache_monocrate::test_support::find_rustc() {
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
            let mut cl = zccache_monocrate::ipc::connect(&ep).await.unwrap();
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
    let rustc = match zccache_monocrate::test_support::find_rustc() {
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
    let mut cl = zccache_monocrate::ipc::connect(&endpoint).await.unwrap();
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
    let rustc = match zccache_monocrate::test_support::find_rustc() {
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
    let mut cl_a = zccache_monocrate::ipc::connect(&endpoint).await.unwrap();
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

    let mut cl_mid = zccache_monocrate::ipc::connect(&endpoint).await.unwrap();
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
    let mut cl_b = zccache_monocrate::ipc::connect(&endpoint).await.unwrap();
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

/// Multiple CARGO_* env vars all affect cache key independently.
#[tokio::test]
#[ignore]
async fn rustc_multiple_cargo_env_vars() {
    let mut h = match TestHarness::new().await {
        Some(h) => h,
        None => return,
    };

    h.write_file(
        "multienv.rs",
        r#"pub fn info() -> (&'static str, &'static str) {
    (env!("CARGO_PKG_VERSION"), env!("CARGO_PKG_NAME"))
}"#,
    );

    let args_base = &[
        "--edition",
        "2021",
        "--crate-type",
        "lib",
        "--crate-name",
        "multienv",
        "--emit=link",
        "multienv.rs",
        "-o",
        "libmultienv.rlib",
    ];

    let env_a = vec![
        ("CARGO_PKG_VERSION".into(), "1.0.0".into()),
        ("CARGO_PKG_NAME".into(), "myapp".into()),
    ];
    let env_b = vec![
        ("CARGO_PKG_VERSION".into(), "1.0.0".into()),
        ("CARGO_PKG_NAME".into(), "other".into()), // only name changed
    ];
    let env_c = vec![
        ("CARGO_PKG_VERSION".into(), "2.0.0".into()), // only version changed
        ("CARGO_PKG_NAME".into(), "myapp".into()),
    ];

    // Env A
    let (ec, cached) = h.compile_args(args_base, Some(env_a.clone())).await;
    assert_eq!(ec, 0);
    assert!(!cached);

    // Env B (name changed) → must miss
    let (ec, cached) = h.compile_args(args_base, Some(env_b)).await;
    assert_eq!(ec, 0);
    assert!(!cached, "different CARGO_PKG_NAME must miss");

    // Env C (version changed) → must miss
    let (ec, cached) = h.compile_args(args_base, Some(env_c)).await;
    assert_eq!(ec, 0);
    assert!(!cached, "different CARGO_PKG_VERSION must miss");

    // Env A again → must hit
    std::fs::remove_file(h.path("libmultienv.rlib")).unwrap();
    let (ec, cached) = h.compile_args(args_base, Some(env_a)).await;
    assert_eq!(ec, 0);
    assert!(cached, "original env should hit");

    h.shutdown().await;
}
