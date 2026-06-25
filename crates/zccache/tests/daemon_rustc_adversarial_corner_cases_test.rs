//! Adversarial tests for rustc cache correctness — corner cases & Rust-specific attack surfaces.
//!
//! Targets edge cases around what gets cached (or doesn't) and what must be
//! folded into the cache key:
//! - Failed compile not cached
//! - Cache persistence across sessions
//! - CARGO_* env vars in cache key
//! - --remap-path-prefix in cache key
//! - -Z flag with value in cache key
//!
//! Companion files:
//! - `daemon_rustc_adversarial_mutations_test.rs` — source/flag mutations
//! - `daemon_rustc_adversarial_concurrency_test.rs` — thundering herd, externs, error handling
//!
//! Run all:    soldr cargo test -p zccache --test daemon_rustc_adversarial_corner_cases_test -- --nocapture --ignored --test-threads=1
//! Run single: soldr cargo test -p zccache --test daemon_rustc_adversarial_corner_cases_test -- <test_name> --nocapture --ignored

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

fn full_env_with_overrides(overrides: Vec<(String, String)>) -> Vec<(String, String)> {
    let mut env: std::collections::BTreeMap<String, String> = std::env::vars().collect();
    env.remove("ZCCACHE_WORKTREE_ROOT");
    env.remove("ZCCACHE_PATH_REMAP");
    for (key, value) in overrides {
        env.insert(key, value);
    }
    env.into_iter().collect()
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
        let env = env.map(full_env_with_overrides);
        compile(&mut self.client, &self.session_id, args, &cwd, &rc, env).await
    }

    async fn shutdown(self) {
        self.shutdown.notify_one();
        self.server_handle.await.unwrap();
    }
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
    let rustc = match zccache::test_support::find_rustc() {
        Some(p) => p,
        None => return,
    };
    let tmp = tempfile::tempdir().unwrap();
    let cwd = tmp.path().to_string_lossy().into_owned();
    let rc = rustc.to_string_lossy().into_owned();
    std::fs::write(tmp.path().join("sess.rs"), "pub fn f() -> i32 { 42 }\n").unwrap();

    let (endpoint, server_handle, shutdown) = start_daemon().await;

    // Session A: compile → miss
    let mut client_a = zccache::ipc::connect(&endpoint).await.unwrap();
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
    let mut client_b = zccache::ipc::connect(&endpoint).await.unwrap();
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
