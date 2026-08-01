//! Integration test: CLI binary end-to-end tests.
//!
//! These tests spawn the actual `zccache` binary as a subprocess,
//! exercising the full CLI → IPC → daemon pipeline.
//!
//! IPC-based session flow tests live in `server.rs` unit tests.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::panic_in_result_fn,
    clippy::unwrap_in_result
)]

use std::sync::Once;

use zccache::core::NormalizedPath;
use zccache::daemon::DaemonServer;

/// Build the CLI binary once across all tests (avoids Cargo lock contention).
static BUILD_CLI_ONCE: Once = Once::new();

fn ensure_cli_built() {
    BUILD_CLI_ONCE.call_once(|| {
        let mut cmd = std::process::Command::new("cargo");
        let status = cmd
            .args(["build", "-p", "zccache-cli"])
            .env_remove("RUSTC_WRAPPER")
            .env_remove("RUSTC_WORKSPACE_WRAPPER")
            .status()
            .expect("failed to run cargo build");
        assert!(status.success(), "cargo build -p zccache-cli failed");
    });
}

fn cli_binary_path() -> NormalizedPath {
    ensure_cli_built();
    let bin_dir = std::path::Path::new(env!("CARGO_BIN_EXE_zccache-daemon"))
        .parent()
        .unwrap();
    let path = if cfg!(windows) {
        bin_dir.join("zccache.exe")
    } else {
        bin_dir.join("zccache")
    };
    NormalizedPath::new(path)
}

/// Parse `session_id` from the CLI's one-line JSON output:
/// `{"session_id":1,"started_at":1710000000}`
fn parse_session_id_from_json(json: &str) -> String {
    let key = "\"session_id\":";
    let start = json.find(key).expect("missing session_id in JSON") + key.len();
    let rest = &json[start..];
    let end = rest.find([',', '}']).unwrap_or(rest.len());
    rest[..end].trim().trim_matches('"').to_string()
}

/// Start a daemon rooted at an isolated cache directory under `cache_root`.
///
/// `DaemonServer::bind` resolves the *process-global* cache root, so every
/// test using it contends with whatever daemon is already live on the
/// developer's (or runner's) default root — failing with "another live daemon
/// already holds this cache root as its writer", or, if it wins the race,
/// writing its logs somewhere the test does not look. `lifecycle.rs` says so
/// directly: everything outside the env guard "should bind an isolated root
/// ... which needs no global state at all and therefore cannot participate in
/// the race".
///
/// The caller already owns a tempdir, so rooting the daemon in it costs
/// nothing and makes the test independent of ambient machine state.
async fn start_daemon(
    cache_root: &std::path::Path,
) -> (
    String,
    tokio::task::JoinHandle<()>,
    std::sync::Arc<tokio::sync::Notify>,
) {
    let endpoint = zccache::ipc::unique_test_endpoint();
    let cache_dir: zccache::core::NormalizedPath = cache_root.join("zccache-cache").into();
    let mut server = DaemonServer::bind_with_cache_dir(&endpoint, &cache_dir).unwrap();
    let shutdown = server.shutdown_handle();
    let handle = tokio::spawn(async move {
        server.run(0).await.unwrap();
    });
    (endpoint, handle, shutdown)
}

/// Test the actual CLI binary end-to-end using subprocess.
/// Runs `zccache session-start`, compiles (miss + hit), then `zccache session-end`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore] // integration-level: starts real daemon + spawns CLI binary
async fn cli_binary_session_round_trip() {
    let clang = match zccache::test_support::find_clang() {
        Some(p) => p,
        None => return,
    };
    let cli_binary = cli_binary_path();
    zccache::test_support::test_timeout(async move {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("cli_test.cpp");
        let obj = tmp.path().join("cli_test.o");
        let log = tmp.path().join("cli.log");
        let cwd = tmp.path().to_string_lossy().into_owned();

        std::fs::write(&src, "int main() { return 0; }\n").unwrap();

        let (endpoint, server_handle, shutdown) = start_daemon(tmp.path()).await;

        assert!(
            cli_binary.exists(),
            "zccache binary not found at {}",
            cli_binary.display()
        );

        // session-start via CLI binary
        let output = std::process::Command::new(&cli_binary)
            .args([
                "session-start",
                "--cwd",
                &cwd,
                "--log",
                &log.to_string_lossy(),
                "--endpoint",
                &endpoint,
            ])
            .output()
            .unwrap();

        assert!(
            output.status.success(),
            "session-start failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );

        let session_json = String::from_utf8_lossy(&output.stdout).trim().to_string();
        let session_id_str = parse_session_id_from_json(&session_json);

        let clang_str = clang.to_string_lossy().into_owned();
        let src_str = src.to_string_lossy().into_owned();
        let obj_str = obj.to_string_lossy().into_owned();

        // Compile via CLI binary (wrap mode, auto-detected)
        let output = std::process::Command::new(&cli_binary)
            .args([&clang_str, "-c", &src_str, "-o", &obj_str])
            .env("ZCCACHE_SESSION_ID", &session_id_str)
            .env("ZCCACHE_ENDPOINT", &endpoint)
            .current_dir(&cwd)
            .output()
            .unwrap();

        assert!(
            output.status.success(),
            "wrap compile failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(obj.exists(), ".o should exist after compile");

        // Compile again — should hit cache
        std::fs::remove_file(&obj).unwrap();
        let output = std::process::Command::new(&cli_binary)
            .args([&clang_str, "-c", &src_str, "-o", &obj_str])
            .env("ZCCACHE_SESSION_ID", &session_id_str)
            .env("ZCCACHE_ENDPOINT", &endpoint)
            .current_dir(&cwd)
            .output()
            .unwrap();

        assert!(
            output.status.success(),
            "cached compile failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(obj.exists(), ".o should exist after cached compile");

        // session-end via CLI binary
        let output = std::process::Command::new(&cli_binary)
            .args(["session-end", &session_id_str, "--endpoint", &endpoint])
            .output()
            .unwrap();

        assert!(
            output.status.success(),
            "session-end failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );

        // Verify log shows miss then hit
        let log_text = std::fs::read_to_string(&log).unwrap();
        assert!(log_text.contains("[MISS]"), "log should show miss");
        assert!(log_text.contains("[HIT]"), "log should show hit");

        shutdown.notify_one();
        server_handle.await.unwrap();
    })
    .await;
}

/// Test ephemeral (sessionless) mode: `zccache clang++ -c foo.cpp -o foo.o`
/// without ZCCACHE_SESSION_ID. The CLI should auto-create a session.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore] // integration-level: starts real daemon + spawns CLI binary
async fn cli_binary_ephemeral_session() {
    let clang = match zccache::test_support::find_clang() {
        Some(p) => p,
        None => return,
    };
    let cli_binary = cli_binary_path();
    zccache::test_support::test_timeout(async move {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("ephemeral.cpp");
        let obj = tmp.path().join("ephemeral.o");
        let cwd = tmp.path().to_string_lossy().into_owned();

        std::fs::write(&src, "int main() { return 0; }\n").unwrap();

        let (endpoint, server_handle, shutdown) = start_daemon(tmp.path()).await;

        let clang_str = clang.to_string_lossy().into_owned();
        let src_str = src.to_string_lossy().into_owned();
        let obj_str = obj.to_string_lossy().into_owned();

        // Compile WITHOUT ZCCACHE_SESSION_ID — ephemeral mode
        let output = std::process::Command::new(&cli_binary)
            .args([&clang_str, "-c", &src_str, "-o", &obj_str])
            .env("ZCCACHE_ENDPOINT", &endpoint)
            .env_remove("ZCCACHE_SESSION_ID")
            .current_dir(&cwd)
            .output()
            .unwrap();

        assert!(
            output.status.success(),
            "ephemeral compile failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(obj.exists(), ".o should exist after ephemeral compile");

        // Compile again — should hit cache (new ephemeral session, but same cache)
        std::fs::remove_file(&obj).unwrap();
        let output = std::process::Command::new(&cli_binary)
            .args([&clang_str, "-c", &src_str, "-o", &obj_str])
            .env("ZCCACHE_ENDPOINT", &endpoint)
            .env_remove("ZCCACHE_SESSION_ID")
            .current_dir(&cwd)
            .output()
            .unwrap();

        assert!(
            output.status.success(),
            "second ephemeral compile failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(obj.exists(), ".o should exist after second compile");

        shutdown.notify_one();
        server_handle.await.unwrap();
    })
    .await;
}

/// Repro for the g++/gcc bug: session started with clang++ (C++ compiler),
/// then wrapping clang (C compiler) to compile a .c file with `-std=c11`.
///
/// Without the compiler override fix, the daemon would invoke clang++ for the
/// C file, causing "not valid for C++" warnings or outright failures.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore] // integration-level: starts real daemon + spawns CLI binary
async fn cli_binary_compiler_override_cpp_session_c_file() {
    let clangpp = match zccache::test_support::find_clang() {
        Some(p) => p,
        None => return,
    };
    let cli_binary = cli_binary_path();
    zccache::test_support::test_timeout(async move {
        // Derive clang (C compiler) from clang++ path
        let clang =
            clangpp
                .parent()
                .unwrap()
                .join(if cfg!(windows) { "clang.exe" } else { "clang" });
        if !clang.exists() {
            eprintln!("SKIP: clang not found at {}", clang.display());
            return;
        }

        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("repro.c");
        let obj = tmp.path().join("repro.o");
        let cwd = tmp.path().to_string_lossy().into_owned();

        // C code using C11 designated initializers — invalid under C++ mode
        std::fs::write(
            &src,
            "struct Point { int x; int y; };\n\
             int main(void) {\n\
             \tstruct Point p = { .x = 1, .y = 2 };\n\
             \treturn p.x + p.y - 3;\n\
             }\n",
        )
        .unwrap();

        let (endpoint, server_handle, shutdown) = start_daemon(tmp.path()).await;

        // Start session (compiler-agnostic now)
        let output = std::process::Command::new(&cli_binary)
            .args(["session-start", "--cwd", &cwd, "--endpoint", &endpoint])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "session-start failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let session_json = String::from_utf8_lossy(&output.stdout).trim().to_string();
        let session_id_str = parse_session_id_from_json(&session_json);

        // Wrap clang (C compiler) to compile a .c file with -std=c11.
        // The bug: without the fix, the daemon would invoke clang++ instead of clang.
        let clang_str = clang.to_string_lossy().into_owned();
        let src_str = src.to_string_lossy().into_owned();
        let obj_str = obj.to_string_lossy().into_owned();

        let output = std::process::Command::new(&cli_binary)
            .args([&clang_str, "-std=c11", "-c", &src_str, "-o", &obj_str])
            .env("ZCCACHE_SESSION_ID", &session_id_str)
            .env("ZCCACHE_ENDPOINT", &endpoint)
            .current_dir(&cwd)
            .output()
            .unwrap();

        let stderr_text = String::from_utf8_lossy(&output.stderr);
        assert!(
            output.status.success(),
            "C file with -std=c11 should compile when wrapping clang on a clang++ session.\n\
             This fails if the daemon uses the session compiler (clang++) instead of \
             the wrapped compiler (clang).\nstderr: {stderr_text}"
        );
        assert!(
            !stderr_text.contains("not valid for C++"),
            "compiler override should use clang, not clang++. stderr: {stderr_text}"
        );
        assert!(obj.exists(), ".o should exist");

        // Session-end
        let output = std::process::Command::new(&cli_binary)
            .args(["session-end", &session_id_str, "--endpoint", &endpoint])
            .output()
            .unwrap();
        assert!(output.status.success());

        shutdown.notify_one();
        server_handle.await.unwrap();
    })
    .await;
}
