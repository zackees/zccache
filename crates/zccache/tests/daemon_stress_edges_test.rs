//! Adversarial stress tests for the compilation cache — edge cases.
//!
//! These tests intentionally try to break the caching with corner cases:
//! - Invalid session IDs survive the daemon
//! - Non-cacheable invocations (e.g. `-E`) pass through without caching
//! - Empty source files, warnings, output path independence
//! - Workspace rename / path remap stability across roots
//! - Rapid recompile cycles, paths with spaces, -Werror vs -Wall
//!
//! See `daemon_stress_correctness_test.rs` for correctness + concurrency tests
//! and `daemon_stress_compiler_test.rs` for compiler-override coverage.

use zccache::daemon::DaemonServer;
use zccache::protocol::{Request, Response};

/// Platform-correct client connection type.
#[cfg(unix)]
type ClientConn = zccache::ipc::IpcConnection;
#[cfg(windows)]
type ClientConn = zccache::ipc::IpcClientConnection;

/// Helper: start a daemon server on a unique endpoint.
async fn start_daemon() -> (
    String,
    tokio::task::JoinHandle<()>,
    std::sync::Arc<tokio::sync::Notify>,
) {
    let endpoint = zccache::ipc::unique_test_endpoint();
    let mut server = DaemonServer::bind(&endpoint).unwrap();
    let shutdown = server.shutdown_handle();
    let handle = tokio::spawn(async move { server.run(0).await.unwrap() });
    (endpoint, handle, shutdown)
}

/// Helper: start a session. Returns (session_id, compiler_string).
async fn start_session(
    client: &mut ClientConn,
    clang: &std::path::Path,
    cwd: &str,
    log_file: &str,
) -> (String, String) {
    let compiler_str = clang.to_string_lossy().into_owned();
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
    let session_id = match client.recv().await.unwrap() {
        Some(Response::SessionStarted { session_id, .. }) => session_id,
        other => panic!("expected SessionStarted, got: {other:?}"),
    };
    (session_id, compiler_str)
}

/// Helper: send a compile request, return (exit_code, cached).
async fn compile(
    client: &mut ClientConn,
    session_id: &str,
    compiler: &str,
    args: &[&str],
    cwd: &str,
) -> (i32, bool) {
    compile_with_env(client, session_id, compiler, args, cwd, None).await
}

async fn compile_with_env(
    client: &mut ClientConn,
    session_id: &str,
    compiler: &str,
    args: &[&str],
    cwd: &str,
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

fn client_env_with_path_remap_auto() -> Vec<(String, String)> {
    let mut env: Vec<(String, String)> = std::env::vars_os()
        .filter_map(|(key, value)| {
            let key = key.into_string().ok()?;
            let value = value.into_string().ok()?;
            let root_var = key.eq_ignore_ascii_case("ZCCACHE_WORKTREE_ROOT");
            let remap_var = key.eq_ignore_ascii_case("ZCCACHE_PATH_REMAP");
            (!root_var && !remap_var).then_some((key, value))
        })
        .collect();
    env.push(("ZCCACHE_PATH_REMAP".to_string(), "auto".to_string()));
    env
}

fn bytes_contain(haystack: &[u8], needle: &str) -> bool {
    let needle = needle.as_bytes();
    !needle.is_empty()
        && haystack
            .windows(needle.len())
            .any(|window| window == needle)
}

// ═══════════════════════════════════════════════════════════════════════
// EDGE CASES
// ═══════════════════════════════════════════════════════════════════════

#[tokio::test]
#[ignore] // integration: requires clang on PATH, run with --full
async fn adversarial_invalid_session_id() {
    if zccache::test_support::find_clang().is_none() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("invalid_session.cpp");
    std::fs::write(&src, "int main() { return 0; }\n").unwrap();
    let cwd = tmp.path().to_string_lossy().into_owned();
    let (endpoint, server_handle, shutdown) = start_daemon().await;
    let mut client = zccache::ipc::connect(&endpoint).await.unwrap();

    client
        .send(&Request::Compile {
            session_id: 999999.to_string(),
            args: vec![
                "-c".into(),
                src.to_string_lossy().into_owned(),
                "-o".into(),
                "out.o".into(),
            ],
            cwd: cwd.into(),
            compiler: "/usr/bin/clang".to_string().into(),
            env: None,
            stdin: Vec::new(),
        })
        .await
        .unwrap();
    match client.recv().await.unwrap() {
        Some(Response::Error { message }) => assert!(
            message.contains("unknown session") || message.contains("invalid session"),
            "error should mention session error: {message}"
        ),
        other => panic!("expected Error for invalid session, got: {other:?}"),
    }
    client.send(&Request::Ping).await.unwrap();
    assert_eq!(
        client.recv().await.unwrap(),
        Some(Response::Pong),
        "server should survive bad requests"
    );
    shutdown.notify_one();
    server_handle.await.unwrap();
}

#[tokio::test]
#[ignore] // integration: spawns clang, run with --full
async fn adversarial_non_cacheable_passthrough() {
    let clang = match zccache::test_support::find_clang() {
        Some(p) => p,
        None => return,
    };
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("passthrough.cpp");
    let obj = tmp.path().join("passthrough.o");
    let log = tmp.path().join("log.txt");
    let cwd = tmp.path().to_string_lossy().into_owned();
    std::fs::write(&src, "#include <stdio.h>\nint main() { return 0; }\n").unwrap();

    let (endpoint, server_handle, shutdown) = start_daemon().await;
    let mut client = zccache::ipc::connect(&endpoint).await.unwrap();
    let (sid, comp) = start_session(&mut client, &clang, &cwd, &log.to_string_lossy()).await;

    client
        .send(&Request::Compile {
            session_id: sid.clone(),
            args: vec!["-E".into(), src.to_string_lossy().into_owned()],
            cwd: cwd.clone().into(),
            compiler: comp.clone().into(),
            env: None,
            stdin: Vec::new(),
        })
        .await
        .unwrap();
    match client.recv().await.unwrap() {
        Some(Response::CompileResult {
            exit_code, cached, ..
        }) => {
            assert_eq!(exit_code, 0, "preprocessing should succeed");
            assert!(!cached, "preprocessing must not be cached");
        }
        other => panic!("expected CompileResult, got: {other:?}"),
    }

    let (ec, c) = compile(
        &mut client,
        &sid,
        &comp,
        &["-c", &src.to_string_lossy(), "-o", &obj.to_string_lossy()],
        &cwd,
    )
    .await;
    assert_eq!(ec, 0);
    assert!(!c, "first real compile should be a miss");
    let log_text = std::fs::read_to_string(&log).unwrap();
    assert!(
        log_text.contains("[DIRECT]"),
        "log should mention direct (non-cacheable)"
    );
    assert!(log_text.contains("[MISS]"), "log should mention miss");
    shutdown.notify_one();
    server_handle.await.unwrap();
}

#[tokio::test]
#[ignore] // integration: spawns clang, run with --full
async fn adversarial_empty_source_file() {
    let clang = match zccache::test_support::find_clang() {
        Some(p) => p,
        None => return,
    };
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("empty.cpp");
    let obj = tmp.path().join("empty.o");
    let log = tmp.path().join("log.txt");
    let cwd = tmp.path().to_string_lossy().into_owned();
    std::fs::write(&src, "").unwrap();

    let (endpoint, server_handle, shutdown) = start_daemon().await;
    let mut client = zccache::ipc::connect(&endpoint).await.unwrap();
    let (sid, comp) = start_session(&mut client, &clang, &cwd, &log.to_string_lossy()).await;

    let (ec, c) = compile(
        &mut client,
        &sid,
        &comp,
        &["-c", &src.to_string_lossy(), "-o", &obj.to_string_lossy()],
        &cwd,
    )
    .await;
    assert_eq!(ec, 0, "empty source should compile");
    assert!(!c);
    std::fs::remove_file(&obj).unwrap();
    let (ec, c) = compile(
        &mut client,
        &sid,
        &comp,
        &["-c", &src.to_string_lossy(), "-o", &obj.to_string_lossy()],
        &cwd,
    )
    .await;
    assert_eq!(ec, 0);
    assert!(c, "empty source recompile should hit cache");
    shutdown.notify_one();
    server_handle.await.unwrap();
}

#[tokio::test]
#[ignore] // integration: spawns clang, run with --full
async fn adversarial_warnings_still_cached() {
    let clang = match zccache::test_support::find_clang() {
        Some(p) => p,
        None => return,
    };
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("warnings.cpp");
    let obj = tmp.path().join("warnings.o");
    let log = tmp.path().join("log.txt");
    let cwd = tmp.path().to_string_lossy().into_owned();
    std::fs::write(&src, "int main() { int unused_var = 42; return 0; }\n").unwrap();

    let (endpoint, server_handle, shutdown) = start_daemon().await;
    let mut client = zccache::ipc::connect(&endpoint).await.unwrap();
    let (sid, comp) = start_session(&mut client, &clang, &cwd, &log.to_string_lossy()).await;

    let (ec, c) = compile(
        &mut client,
        &sid,
        &comp,
        &[
            "-c",
            &src.to_string_lossy(),
            "-o",
            &obj.to_string_lossy(),
            "-Wall",
        ],
        &cwd,
    )
    .await;
    assert_eq!(ec, 0);
    assert!(!c);
    std::fs::remove_file(&obj).unwrap();
    let (ec, c) = compile(
        &mut client,
        &sid,
        &comp,
        &[
            "-c",
            &src.to_string_lossy(),
            "-o",
            &obj.to_string_lossy(),
            "-Wall",
        ],
        &cwd,
    )
    .await;
    assert_eq!(ec, 0);
    assert!(c, "warnings-only compilation should still be cached");
    shutdown.notify_one();
    server_handle.await.unwrap();
}

#[tokio::test]
#[ignore] // integration: spawns clang, run with --full
async fn adversarial_output_path_does_not_affect_cache_key() {
    let clang = match zccache::test_support::find_clang() {
        Some(p) => p,
        None => return,
    };
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("output_path.cpp");
    let obj_a = tmp.path().join("a.o");
    let obj_b = tmp.path().join("b.o");
    let log = tmp.path().join("log.txt");
    let cwd = tmp.path().to_string_lossy().into_owned();
    std::fs::write(&src, "int main() { return 0; }\n").unwrap();

    let (endpoint, server_handle, shutdown) = start_daemon().await;
    let mut client = zccache::ipc::connect(&endpoint).await.unwrap();
    let (sid, comp) = start_session(&mut client, &clang, &cwd, &log.to_string_lossy()).await;

    let (ec, c) = compile(
        &mut client,
        &sid,
        &comp,
        &["-c", &src.to_string_lossy(), "-o", &obj_a.to_string_lossy()],
        &cwd,
    )
    .await;
    assert_eq!(ec, 0);
    assert!(!c);
    let data_a = std::fs::read(&obj_a).unwrap();
    let (ec, c) = compile(
        &mut client,
        &sid,
        &comp,
        &["-c", &src.to_string_lossy(), "-o", &obj_b.to_string_lossy()],
        &cwd,
    )
    .await;
    assert_eq!(ec, 0);
    assert!(c, "same source to different output path should hit cache");
    assert_eq!(
        data_a,
        std::fs::read(&obj_b).unwrap(),
        "cached .o should be identical regardless of output path"
    );
    shutdown.notify_one();
    server_handle.await.unwrap();
}

#[tokio::test]
#[ignore] // integration: spawns clang twice, run with --full
async fn adversarial_workspace_rename_hits_cache() {
    let clang = match zccache::test_support::find_clang() {
        Some(p) => p,
        None => return,
    };
    let tmp = tempfile::tempdir().unwrap();
    let ws_a = tmp.path().join("workspace-a");
    let ws_b = tmp.path().join("workspace-b");
    let src_rel = std::path::Path::new("src").join("main.cpp");
    let header_rel = std::path::Path::new("include").join("demo.h");
    let obj_rel = std::path::Path::new("build").join("main.o");
    for ws in [&ws_a, &ws_b] {
        std::fs::create_dir_all(ws.join("src")).unwrap();
        std::fs::create_dir_all(ws.join("include")).unwrap();
        std::fs::create_dir_all(ws.join("build")).unwrap();
        std::fs::write(
            ws.join(&header_rel),
            "#pragma once\ninline int demo() { return 7; }\n",
        )
        .unwrap();
        std::fs::write(
            ws.join(&src_rel),
            "#include \"demo.h\"\nint main() { return demo(); }\n",
        )
        .unwrap();
    }
    let log = tmp.path().join("workspace-rename.log");

    let (endpoint, server_handle, shutdown) = start_daemon().await;
    let mut client_a = zccache::ipc::connect(&endpoint).await.unwrap();
    let cwd_a = ws_a.to_string_lossy().into_owned();
    let (sid_a, comp_a) =
        start_session(&mut client_a, &clang, &cwd_a, &log.to_string_lossy()).await;
    let src_a = ws_a.join(&src_rel);
    let obj_a = ws_a.join(&obj_rel);
    let include_a = ws_a.join("include");

    let (ec, cached) = compile(
        &mut client_a,
        &sid_a,
        &comp_a,
        &[
            "-I",
            &include_a.to_string_lossy(),
            "-c",
            &src_a.to_string_lossy(),
            "-o",
            &obj_a.to_string_lossy(),
        ],
        &cwd_a,
    )
    .await;
    assert_eq!(ec, 0);
    assert!(!cached);

    let mut client_b = zccache::ipc::connect(&endpoint).await.unwrap();
    let cwd_b = ws_b.to_string_lossy().into_owned();
    let (sid_b, comp_b) =
        start_session(&mut client_b, &clang, &cwd_b, &log.to_string_lossy()).await;
    let src_b = ws_b.join(&src_rel);
    let obj_b = ws_b.join(&obj_rel);
    let include_b = ws_b.join("include");

    let (ec, cached) = compile(
        &mut client_b,
        &sid_b,
        &comp_b,
        &[
            "-I",
            &include_b.to_string_lossy(),
            "-c",
            &src_b.to_string_lossy(),
            "-o",
            &obj_b.to_string_lossy(),
        ],
        &cwd_b,
    )
    .await;
    assert_eq!(ec, 0);
    assert!(
        cached,
        "same tree under a different workspace root should hit cache"
    );
    assert_eq!(
        std::fs::read(&obj_a).unwrap(),
        std::fs::read(&obj_b).unwrap()
    );

    shutdown.notify_one();
    server_handle.await.unwrap();
}

#[tokio::test]
#[ignore] // integration: spawns clang twice, run with --full
async fn path_remap_auto_compiles_file_macro_stably_across_git_roots() {
    let clang = match zccache::test_support::find_clang() {
        Some(p) => p,
        None => return,
    };
    let tmp = tempfile::tempdir().unwrap();
    let ws_a = tmp.path().join("workspace-a");
    let ws_b = tmp.path().join("workspace-b");
    let src_rel = std::path::Path::new("src").join("main.cpp");
    let obj_rel = std::path::Path::new("build").join("main.o");
    for ws in [&ws_a, &ws_b] {
        std::fs::create_dir_all(ws.join(".git")).unwrap();
        std::fs::create_dir_all(ws.join("src")).unwrap();
        std::fs::create_dir_all(ws.join("build")).unwrap();
        std::fs::write(
            ws.join(&src_rel),
            "const char* source_file = __FILE__;\nint main() { return source_file[0] == 0; }\n",
        )
        .unwrap();
    }
    let log = tmp.path().join("path-remap-auto.log");

    let (endpoint, server_handle, shutdown) = start_daemon().await;
    let env = client_env_with_path_remap_auto();

    let mut client_a = zccache::ipc::connect(&endpoint).await.unwrap();
    let cwd_a = ws_a.to_string_lossy().into_owned();
    let (sid_a, comp_a) =
        start_session(&mut client_a, &clang, &cwd_a, &log.to_string_lossy()).await;
    let src_a = ws_a.join(&src_rel);
    let obj_a = ws_a.join(&obj_rel);
    let (ec, cached) = compile_with_env(
        &mut client_a,
        &sid_a,
        &comp_a,
        &[
            "-g0",
            "-c",
            &src_a.to_string_lossy(),
            "-o",
            &obj_a.to_string_lossy(),
        ],
        &cwd_a,
        Some(env.clone()),
    )
    .await;
    assert_eq!(ec, 0);
    assert!(!cached);
    let obj_a_bytes = std::fs::read(&obj_a).unwrap();
    assert!(
        !bytes_contain(&obj_a_bytes, &ws_a.to_string_lossy()),
        "auto path remap should keep root A out of __FILE__ object bytes"
    );
    assert!(
        !bytes_contain(&obj_a_bytes, &ws_a.to_string_lossy().replace('\\', "/")),
        "auto path remap should keep slash-normalized root A out of __FILE__ object bytes"
    );

    let mut client_b = zccache::ipc::connect(&endpoint).await.unwrap();
    let cwd_b = ws_b.to_string_lossy().into_owned();
    let (sid_b, comp_b) =
        start_session(&mut client_b, &clang, &cwd_b, &log.to_string_lossy()).await;
    let src_b = ws_b.join(&src_rel);
    let obj_b = ws_b.join(&obj_rel);
    let (ec, cached) = compile_with_env(
        &mut client_b,
        &sid_b,
        &comp_b,
        &[
            "-g0",
            "-c",
            &src_b.to_string_lossy(),
            "-o",
            &obj_b.to_string_lossy(),
        ],
        &cwd_b,
        Some(env),
    )
    .await;
    assert_eq!(ec, 0);
    assert!(
        cached,
        "ZCCACHE_PATH_REMAP=auto should make equivalent __FILE__ compiles hit"
    );
    assert_eq!(obj_a_bytes, std::fs::read(&obj_b).unwrap());

    shutdown.notify_one();
    server_handle.await.unwrap();
}

#[tokio::test]
#[ignore] // integration: spawns clang 20x, run with --full
async fn adversarial_rapid_recompile_cycle() {
    let clang = match zccache::test_support::find_clang() {
        Some(p) => p,
        None => return,
    };
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("rapid.cpp");
    let obj = tmp.path().join("rapid.o");
    let log = tmp.path().join("log.txt");
    let cwd = tmp.path().to_string_lossy().into_owned();
    std::fs::write(&src, "int main() { return 0; }\n").unwrap();

    let (endpoint, server_handle, shutdown) = start_daemon().await;
    let mut client = zccache::ipc::connect(&endpoint).await.unwrap();
    let (sid, comp) = start_session(&mut client, &clang, &cwd, &log.to_string_lossy()).await;
    let src_str = src.to_string_lossy().into_owned();
    let obj_str = obj.to_string_lossy().into_owned();
    let args_owned: Vec<&str> = vec!["-c", &src_str, "-o", &obj_str];

    let (ec, c) = compile(&mut client, &sid, &comp, &args_owned, &cwd).await;
    assert_eq!(ec, 0);
    assert!(!c);
    let reference_obj = std::fs::read(&obj).unwrap();
    for i in 0..20 {
        std::fs::remove_file(&obj).unwrap();
        let (ec, c) = compile(&mut client, &sid, &comp, &args_owned, &cwd).await;
        assert_eq!(ec, 0, "rapid cycle {i} should succeed");
        assert!(c, "rapid cycle {i} should be a cache hit");
        assert_eq!(
            std::fs::read(&obj).unwrap(),
            reference_obj,
            "rapid cycle {i} .o should match reference"
        );
    }
    let log_text = std::fs::read_to_string(&log).unwrap();
    assert_eq!(
        log_text.matches("[MISS]").count(),
        1,
        "expected exactly 1 miss"
    );
    let hit_count = log_text.matches("[HIT]").count() + log_text.matches("[HIT_FAST]").count();
    assert_eq!(hit_count, 20, "expected exactly 20 hits");
    shutdown.notify_one();
    server_handle.await.unwrap();
}

#[tokio::test]
#[ignore] // integration: spawns clang, run with --full
async fn adversarial_spaces_in_filename() {
    let clang = match zccache::test_support::find_clang() {
        Some(p) => p,
        None => return,
    };
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("hello world.cpp");
    let obj = tmp.path().join("hello world.o");
    let log = tmp.path().join("log.txt");
    let cwd = tmp.path().to_string_lossy().into_owned();
    std::fs::write(&src, "int main() { return 0; }\n").unwrap();

    let (endpoint, server_handle, shutdown) = start_daemon().await;
    let mut client = zccache::ipc::connect(&endpoint).await.unwrap();
    let (sid, comp) = start_session(&mut client, &clang, &cwd, &log.to_string_lossy()).await;
    let (ec, c) = compile(
        &mut client,
        &sid,
        &comp,
        &["-c", &src.to_string_lossy(), "-o", &obj.to_string_lossy()],
        &cwd,
    )
    .await;
    assert_eq!(ec, 0, "file with spaces should compile");
    assert!(!c);
    std::fs::remove_file(&obj).unwrap();
    let (ec, c) = compile(
        &mut client,
        &sid,
        &comp,
        &["-c", &src.to_string_lossy(), "-o", &obj.to_string_lossy()],
        &cwd,
    )
    .await;
    assert_eq!(ec, 0);
    assert!(c, "recompile of file with spaces should hit cache");
    shutdown.notify_one();
    server_handle.await.unwrap();
}

#[tokio::test]
#[ignore] // integration: spawns clang, run with --full
async fn adversarial_werror_vs_no_werror() {
    let clang = match zccache::test_support::find_clang() {
        Some(p) => p,
        None => return,
    };
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("werror.cpp");
    let obj = tmp.path().join("werror.o");
    let log = tmp.path().join("log.txt");
    let cwd = tmp.path().to_string_lossy().into_owned();
    std::fs::write(&src, "int main() { int x = 0; return 0; }\n").unwrap();

    let (endpoint, server_handle, shutdown) = start_daemon().await;
    let mut client = zccache::ipc::connect(&endpoint).await.unwrap();
    let (sid, comp) = start_session(&mut client, &clang, &cwd, &log.to_string_lossy()).await;

    let (ec, c) = compile(
        &mut client,
        &sid,
        &comp,
        &[
            "-c",
            &src.to_string_lossy(),
            "-o",
            &obj.to_string_lossy(),
            "-Wall",
            "-Werror",
        ],
        &cwd,
    )
    .await;
    assert_ne!(ec, 0, "-Werror should cause compile failure");
    assert!(!c, "failed -Werror compile must not be cached");

    let (ec, c) = compile(
        &mut client,
        &sid,
        &comp,
        &[
            "-c",
            &src.to_string_lossy(),
            "-o",
            &obj.to_string_lossy(),
            "-Wall",
        ],
        &cwd,
    )
    .await;
    assert_eq!(ec, 0, "without -Werror should succeed");
    assert!(!c, "different flags = different cache key = miss");

    std::fs::remove_file(&obj).unwrap();
    let (ec, c) = compile(
        &mut client,
        &sid,
        &comp,
        &[
            "-c",
            &src.to_string_lossy(),
            "-o",
            &obj.to_string_lossy(),
            "-Wall",
        ],
        &cwd,
    )
    .await;
    assert_eq!(ec, 0);
    assert!(c, "-Wall recompile (no -Werror) should hit cache");
    shutdown.notify_one();
    server_handle.await.unwrap();
}
