//! Integration tests for DLL / shared library caching.
//!
//! Tests the full flow: compile .o files → `gcc -shared` → cache hit/miss.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::panic_in_result_fn,
    clippy::unwrap_in_result
)]

use zccache::daemon::DaemonServer;
use zccache::protocol::{Request, Response};

/// Helper: start a daemon server on a unique endpoint.
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

/// Compile a minimal C source to an object file using gcc.
fn compile_object(gcc: &std::path::Path, dir: &std::path::Path, name: &str, body: &str) {
    let src = dir.join(format!("{name}.c"));
    let obj = dir.join(format!("{name}.o"));
    std::fs::write(&src, body).unwrap();
    let status = std::process::Command::new(gcc)
        .args(["-c", &src.to_string_lossy(), "-o", &obj.to_string_lossy()])
        .current_dir(dir)
        .status()
        .unwrap();
    assert!(status.success(), "gcc -c should succeed for {name}.c");
}

#[tokio::test]
#[ignore] // Integration test — starts a real daemon + gcc. Run with `test --full`.
async fn test_dll_cache_miss_then_hit() {
    let gcc_path = match zccache::test_support::find_on_path("gcc") {
        Some(p) => p,
        None => {
            eprintln!("skipping test: gcc not found on PATH");
            return;
        }
    };

    let tmp = tempfile::tempdir().unwrap();
    compile_object(
        &gcc_path,
        tmp.path(),
        "add",
        "__declspec(dllexport) int add(int a, int b) { return a + b; }\n",
    );
    compile_object(
        &gcc_path,
        tmp.path(),
        "mul",
        "__declspec(dllexport) int mul(int a, int b) { return a * b; }\n",
    );

    let output_dll = tmp.path().join("libmath.dll");

    let (endpoint, server_handle, shutdown) = start_daemon().await;
    let mut client = zccache::ipc::connect(&endpoint).await.unwrap();

    // Clear persisted artifacts to ensure test isolation from prior runs
    client.send(&Request::Clear).await.unwrap();
    let _: Option<Response> = client.recv().await.unwrap();

    let make_args = |dll: &std::path::Path, dir: &std::path::Path| -> Vec<String> {
        vec![
            "-shared".to_string(),
            "-o".to_string(),
            dll.to_string_lossy().into_owned(),
            dir.join("add.o").to_string_lossy().into_owned(),
            dir.join("mul.o").to_string_lossy().into_owned(),
        ]
    };

    // First link — should be a cache miss
    client
        .send(&Request::LinkEphemeral {
            client_pid: std::process::id(),

            tool: gcc_path.to_string_lossy().into_owned().into(),
            args: make_args(&output_dll, tmp.path()),
            cwd: tmp.path().to_string_lossy().into_owned().into(),
            env: None,
        })
        .await
        .unwrap();

    let resp = client.recv().await.unwrap();
    match resp {
        Some(Response::LinkResult {
            exit_code,
            cached,
            warning,
            ..
        }) => {
            assert_eq!(exit_code, 0, "gcc -shared should succeed");
            assert!(!cached, "first link should be a cache miss");
            assert!(
                warning.is_none(),
                "deterministic link — no warning expected"
            );
        }
        other => panic!("expected LinkResult, got: {other:?}"),
    }

    assert!(output_dll.exists(), "DLL should exist after first link");
    let first_contents = std::fs::read(&output_dll).unwrap();
    assert!(!first_contents.is_empty(), "DLL should not be empty");

    // Delete the output so we can verify cache restores it
    std::fs::remove_file(&output_dll).unwrap();
    assert!(!output_dll.exists(), "DLL should be deleted");

    // Second link — should be a cache hit
    client
        .send(&Request::LinkEphemeral {
            client_pid: std::process::id(),

            tool: gcc_path.to_string_lossy().into_owned().into(),
            args: make_args(&output_dll, tmp.path()),
            cwd: tmp.path().to_string_lossy().into_owned().into(),
            env: None,
        })
        .await
        .unwrap();

    let resp = client.recv().await.unwrap();
    match resp {
        Some(Response::LinkResult {
            exit_code, cached, ..
        }) => {
            assert_eq!(exit_code, 0, "cached link should succeed");
            assert!(cached, "second link should be a cache hit");
        }
        other => panic!("expected LinkResult, got: {other:?}"),
    }

    // Verify the cached output was restored byte-identical
    assert!(output_dll.exists(), "cache hit should restore the DLL");
    let second_contents = std::fs::read(&output_dll).unwrap();
    assert_eq!(
        first_contents, second_contents,
        "cached DLL should be byte-identical"
    );

    shutdown.notify_one();
    server_handle.await.unwrap();
}

#[tokio::test]
#[ignore] // Integration test — starts a real daemon + gcc. Run with `test --full`.
async fn test_dll_cache_invalidated_on_input_change() {
    let gcc_path = match zccache::test_support::find_on_path("gcc") {
        Some(p) => p,
        None => {
            eprintln!("skipping test: gcc not found on PATH");
            return;
        }
    };

    let tmp = tempfile::tempdir().unwrap();
    compile_object(
        &gcc_path,
        tmp.path(),
        "func",
        "__declspec(dllexport) int func(void) { return 42; }\n",
    );

    let output_dll = tmp.path().join("libfunc.dll");

    let (endpoint, server_handle, shutdown) = start_daemon().await;
    let mut client = zccache::ipc::connect(&endpoint).await.unwrap();

    // Clear persisted artifacts to ensure test isolation from prior runs
    client.send(&Request::Clear).await.unwrap();
    let _: Option<Response> = client.recv().await.unwrap();

    let make_args = |dll: &std::path::Path, dir: &std::path::Path| -> Vec<String> {
        vec![
            "-shared".to_string(),
            "-o".to_string(),
            dll.to_string_lossy().into_owned(),
            dir.join("func.o").to_string_lossy().into_owned(),
        ]
    };

    // First link — cache miss
    client
        .send(&Request::LinkEphemeral {
            client_pid: std::process::id(),

            tool: gcc_path.to_string_lossy().into_owned().into(),
            args: make_args(&output_dll, tmp.path()),
            cwd: tmp.path().to_string_lossy().into_owned().into(),
            env: None,
        })
        .await
        .unwrap();

    let resp = client.recv().await.unwrap();
    match &resp {
        Some(Response::LinkResult {
            exit_code, cached, ..
        }) => {
            assert_eq!(*exit_code, 0);
            assert!(!cached, "first link should miss");
        }
        other => panic!("expected LinkResult, got: {other:?}"),
    }

    let original_dll = std::fs::read(&output_dll).unwrap();

    // Recompile with different function body
    compile_object(
        &gcc_path,
        tmp.path(),
        "func",
        "__declspec(dllexport) int func(void) { return 99; }\n",
    );

    // Delete output to verify it gets recreated
    std::fs::remove_file(&output_dll).unwrap();

    // Second link — should be a cache miss (input changed)
    client
        .send(&Request::LinkEphemeral {
            client_pid: std::process::id(),

            tool: gcc_path.to_string_lossy().into_owned().into(),
            args: make_args(&output_dll, tmp.path()),
            cwd: tmp.path().to_string_lossy().into_owned().into(),
            env: None,
        })
        .await
        .unwrap();

    let resp = client.recv().await.unwrap();
    match resp {
        Some(Response::LinkResult {
            exit_code, cached, ..
        }) => {
            assert_eq!(exit_code, 0);
            assert!(!cached, "link after input change should be a cache miss");
        }
        other => panic!("expected LinkResult, got: {other:?}"),
    }

    // The new DLL should differ from the original
    let new_dll = std::fs::read(&output_dll).unwrap();
    assert_ne!(
        original_dll, new_dll,
        "DLL should differ after input change"
    );

    shutdown.notify_one();
    server_handle.await.unwrap();
}

#[tokio::test]
#[ignore] // Integration test — starts a real daemon + gcc. Run with `test --full`.
async fn test_dll_non_deterministic_warning() {
    let gcc_path = match zccache::test_support::find_on_path("gcc") {
        Some(p) => p,
        None => {
            eprintln!("skipping test: gcc not found on PATH");
            return;
        }
    };

    let tmp = tempfile::tempdir().unwrap();
    compile_object(
        &gcc_path,
        tmp.path(),
        "warn",
        "__declspec(dllexport) int warn_fn(void) { return 1; }\n",
    );

    let output_dll = tmp.path().join("libwarn.dll");

    let (endpoint, server_handle, shutdown) = start_daemon().await;
    let mut client = zccache::ipc::connect(&endpoint).await.unwrap();

    // Clear persisted artifacts to ensure test isolation from prior runs
    client.send(&Request::Clear).await.unwrap();
    let _: Option<Response> = client.recv().await.unwrap();

    // gcc -shared -Wl,--build-id=uuid → non-deterministic, should warn
    client
        .send(&Request::LinkEphemeral {
            client_pid: std::process::id(),

            tool: gcc_path.to_string_lossy().into_owned().into(),
            args: vec![
                "-shared".to_string(),
                "-Wl,--build-id=uuid".to_string(),
                "-o".to_string(),
                output_dll.to_string_lossy().into_owned(),
                tmp.path().join("warn.o").to_string_lossy().into_owned(),
            ],
            cwd: tmp.path().to_string_lossy().into_owned().into(),
            env: None,
        })
        .await
        .unwrap();

    let resp = client.recv().await.unwrap();
    match resp {
        Some(Response::LinkResult {
            exit_code,
            cached,
            warning,
            ..
        }) => {
            // The link should succeed (gcc may or may not support --build-id on Windows).
            // If gcc doesn't support it and fails, that's fine — we still test the daemon path.
            if exit_code == 0 {
                assert!(!cached, "first invocation should be a cache miss");
                assert!(
                    warning.is_some(),
                    "should warn about non-deterministic invocation"
                );
                let w = warning.unwrap();
                assert!(
                    w.contains("non-deterministic"),
                    "warning should mention non-determinism: {w}"
                );
            }
            // If exit_code != 0, the linker doesn't support --build-id on this platform,
            // which is fine — the daemon still correctly ran the tool.
        }
        other => panic!("expected LinkResult, got: {other:?}"),
    }

    shutdown.notify_one();
    server_handle.await.unwrap();
}

#[tokio::test]
#[ignore] // Integration test — starts a real daemon + gcc. Run with `test --full`.
async fn test_exe_cache_miss_then_hit() {
    let gcc_path = match zccache::test_support::find_on_path("gcc") {
        Some(p) => p,
        None => {
            eprintln!("skipping test: gcc not found on PATH");
            return;
        }
    };

    let tmp = tempfile::tempdir().unwrap();
    compile_object(
        &gcc_path,
        tmp.path(),
        "main",
        "int main(void) { return 0; }\n",
    );

    let output_exe = tmp.path().join("main.exe");

    let (endpoint, server_handle, shutdown) = start_daemon().await;
    let mut client = zccache::ipc::connect(&endpoint).await.unwrap();

    // Clear persisted artifacts to ensure test isolation from prior runs
    client.send(&Request::Clear).await.unwrap();
    let _: Option<Response> = client.recv().await.unwrap();

    let make_args = |exe: &std::path::Path, dir: &std::path::Path| -> Vec<String> {
        vec![
            "-o".to_string(),
            exe.to_string_lossy().into_owned(),
            dir.join("main.o").to_string_lossy().into_owned(),
        ]
    };

    // First exe link — should be a cache miss
    client
        .send(&Request::LinkEphemeral {
            client_pid: std::process::id(),

            tool: gcc_path.to_string_lossy().into_owned().into(),
            args: make_args(&output_exe, tmp.path()),
            cwd: tmp.path().to_string_lossy().into_owned().into(),
            env: None,
        })
        .await
        .unwrap();

    let resp = client.recv().await.unwrap();
    match resp {
        Some(Response::LinkResult {
            exit_code, cached, ..
        }) => {
            assert_eq!(exit_code, 0, "gcc should succeed for exe linking");
            assert!(!cached, "first exe link should be a cache miss");
        }
        other => panic!("expected LinkResult, got: {other:?}"),
    }

    assert!(
        output_exe.exists(),
        "executable should exist after first link"
    );
    let first_contents = std::fs::read(&output_exe).unwrap();
    assert!(!first_contents.is_empty(), "executable should not be empty");

    // Delete the output so we can verify cache restores it
    std::fs::remove_file(&output_exe).unwrap();
    assert!(!output_exe.exists(), "executable should be deleted");

    // Second exe link — should be a cache hit
    client
        .send(&Request::LinkEphemeral {
            client_pid: std::process::id(),

            tool: gcc_path.to_string_lossy().into_owned().into(),
            args: make_args(&output_exe, tmp.path()),
            cwd: tmp.path().to_string_lossy().into_owned().into(),
            env: None,
        })
        .await
        .unwrap();

    let resp = client.recv().await.unwrap();
    match resp {
        Some(Response::LinkResult {
            exit_code, cached, ..
        }) => {
            assert_eq!(exit_code, 0, "cached exe link should succeed");
            assert!(cached, "second exe link should be a cache hit");
        }
        other => panic!("expected LinkResult, got: {other:?}"),
    }

    // Verify the cached output was restored byte-identical
    assert!(
        output_exe.exists(),
        "cache hit should restore the executable"
    );
    let second_contents = std::fs::read(&output_exe).unwrap();
    assert_eq!(
        first_contents, second_contents,
        "cached executable should be byte-identical"
    );

    shutdown.notify_one();
    server_handle.await.unwrap();
}

/// Compile a minimal C source to an object file using any compiler (clang, gcc, etc.).
fn compile_object_with(compiler: &std::path::Path, dir: &std::path::Path, name: &str, body: &str) {
    let src = dir.join(format!("{name}.c"));
    let obj = dir.join(format!("{name}.o"));
    std::fs::write(&src, body).unwrap();
    let status = std::process::Command::new(compiler)
        .args(["-c", &src.to_string_lossy(), "-o", &obj.to_string_lossy()])
        .current_dir(dir)
        .status()
        .unwrap();
    assert!(
        status.success(),
        "compile should succeed for {name}.c with {}",
        compiler.display()
    );
}

/// End-to-end test: compile with clang, link via clang as compiler driver,
/// verify cache miss then cache hit. Confirms clang-based link caching works
/// (same path emcc/em++ would take as compiler drivers).
#[tokio::test]
#[ignore] // Integration test — starts a real daemon + clang. Run with `test --full`.
async fn test_clang_link_cache_miss_then_hit() {
    let clang = match zccache::test_support::find_clang() {
        Some(p) => p,
        None => return,
    };

    let tmp = tempfile::tempdir().unwrap();
    compile_object_with(&clang, tmp.path(), "main", "int main(void) { return 0; }\n");

    let output_exe = tmp
        .path()
        .join(if cfg!(windows) { "main.exe" } else { "main" });

    let (endpoint, server_handle, shutdown) = start_daemon().await;
    let mut client = zccache::ipc::connect(&endpoint).await.unwrap();

    // Clear persisted artifacts to ensure test isolation
    client.send(&Request::Clear).await.unwrap();
    let _: Option<Response> = client.recv().await.unwrap();

    let make_args = |exe: &std::path::Path, dir: &std::path::Path| -> Vec<String> {
        vec![
            "-o".to_string(),
            exe.to_string_lossy().into_owned(),
            dir.join("main.o").to_string_lossy().into_owned(),
        ]
    };

    // First link — should be a cache miss
    client
        .send(&Request::LinkEphemeral {
            client_pid: std::process::id(),

            tool: clang.to_string_lossy().into_owned().into(),
            args: make_args(&output_exe, tmp.path()),
            cwd: tmp.path().to_string_lossy().into_owned().into(),
            env: None,
        })
        .await
        .unwrap();

    let resp = client.recv().await.unwrap();
    match resp {
        Some(Response::LinkResult {
            exit_code, cached, ..
        }) => {
            assert_eq!(exit_code, 0, "clang should succeed for exe linking");
            assert!(!cached, "first link should be a cache miss");
        }
        other => panic!("expected LinkResult, got: {other:?}"),
    }

    assert!(
        output_exe.exists(),
        "executable should exist after first link"
    );
    let first_contents = std::fs::read(&output_exe).unwrap();
    assert!(!first_contents.is_empty(), "executable should not be empty");

    // Delete the output so we can verify cache restores it
    std::fs::remove_file(&output_exe).unwrap();

    // Second link — should be a cache hit
    client
        .send(&Request::LinkEphemeral {
            client_pid: std::process::id(),

            tool: clang.to_string_lossy().into_owned().into(),
            args: make_args(&output_exe, tmp.path()),
            cwd: tmp.path().to_string_lossy().into_owned().into(),
            env: None,
        })
        .await
        .unwrap();

    let resp = client.recv().await.unwrap();
    match resp {
        Some(Response::LinkResult {
            exit_code, cached, ..
        }) => {
            assert_eq!(exit_code, 0, "cached link should succeed");
            assert!(cached, "second link should be a cache hit");
        }
        other => panic!("expected LinkResult, got: {other:?}"),
    }

    // Verify the cached output was restored byte-identical
    assert!(
        output_exe.exists(),
        "cache hit should restore the executable"
    );
    let second_contents = std::fs::read(&output_exe).unwrap();
    assert_eq!(
        first_contents, second_contents,
        "cached executable should be byte-identical"
    );

    shutdown.notify_one();
    server_handle.await.unwrap();
}
