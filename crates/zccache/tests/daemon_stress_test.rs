//! Adversarial stress tests for the compilation cache.
//!
//! These tests intentionally try to break the caching with corner cases:
//! - Source modifications must invalidate cache
//! - Different flags must produce different cache entries
//! - Compile errors must NOT be cached
//! - Concurrent compilations must not corrupt state
//! - Local header changes must invalidate cache
//! - Edge cases in paths, empty files, etc.

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
// CORRECTNESS ATTACKS
// ═══════════════════════════════════════════════════════════════════════

#[tokio::test]
#[ignore] // integration: spawns clang, run with --full
async fn adversarial_different_flags_different_cache_entries() {
    let clang = match zccache::test_support::find_clang() {
        Some(p) => p,
        None => return,
    };
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("flags.cpp");
    let obj = tmp.path().join("flags.o");
    let log = tmp.path().join("log.txt");
    let cwd = tmp.path().to_string_lossy().into_owned();
    std::fs::write(
        &src,
        "static int x=0;\nint get(){return x++;}\nint main(){return get()+get();}\n",
    )
    .unwrap();

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
            "-O0",
        ],
        &cwd,
    )
    .await;
    assert_eq!(ec, 0);
    assert!(!c);
    let obj_o0 = std::fs::read(&obj).unwrap();

    let (ec, c) = compile(
        &mut client,
        &sid,
        &comp,
        &[
            "-c",
            &src.to_string_lossy(),
            "-o",
            &obj.to_string_lossy(),
            "-O2",
        ],
        &cwd,
    )
    .await;
    assert_eq!(ec, 0);
    assert!(!c, "-O2 compile MUST NOT hit the -O0 cache entry");
    let obj_o2 = std::fs::read(&obj).unwrap();
    assert_ne!(
        obj_o0, obj_o2,
        "-O0 and -O2 should produce different .o files"
    );

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
            "-O0",
        ],
        &cwd,
    )
    .await;
    assert_eq!(ec, 0);
    assert!(c, "-O0 recompile should be a cache hit");
    assert_eq!(
        std::fs::read(&obj).unwrap(),
        obj_o0,
        "cached -O0 must match original"
    );

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
            "-O2",
        ],
        &cwd,
    )
    .await;
    assert_eq!(ec, 0);
    assert!(c, "-O2 recompile should be a cache hit");
    assert_eq!(
        std::fs::read(&obj).unwrap(),
        obj_o2,
        "cached -O2 must match original"
    );

    shutdown.notify_one();
    server_handle.await.unwrap();
}

#[tokio::test]
#[ignore] // integration: spawns clang, run with --full
async fn adversarial_define_changes_invalidate_cache() {
    let clang = match zccache::test_support::find_clang() {
        Some(p) => p,
        None => return,
    };
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("defines.cpp");
    let obj = tmp.path().join("defines.o");
    let log = tmp.path().join("log.txt");
    let cwd = tmp.path().to_string_lossy().into_owned();
    std::fs::write(
        &src,
        "#ifdef RETURN_ZERO\nint main(){return 0;}\n#else\nint main(){return 1;}\n#endif\n",
    )
    .unwrap();

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
    assert_eq!(ec, 0);
    assert!(!c);
    let obj_no_define = std::fs::read(&obj).unwrap();

    let (ec, c) = compile(
        &mut client,
        &sid,
        &comp,
        &[
            "-c",
            &src.to_string_lossy(),
            "-o",
            &obj.to_string_lossy(),
            "-DRETURN_ZERO",
        ],
        &cwd,
    )
    .await;
    assert_eq!(ec, 0);
    assert!(!c, "-DRETURN_ZERO MUST be a cache miss");
    let obj_with_define = std::fs::read(&obj).unwrap();
    assert_ne!(
        obj_no_define, obj_with_define,
        "different defines must produce different .o"
    );

    shutdown.notify_one();
    server_handle.await.unwrap();
}

#[tokio::test]
#[ignore] // integration: spawns clang, run with --full
async fn adversarial_compile_errors_never_cached() {
    let clang = match zccache::test_support::find_clang() {
        Some(p) => p,
        None => return,
    };
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("errors.cpp");
    let obj = tmp.path().join("errors.o");
    let log = tmp.path().join("log.txt");
    let cwd = tmp.path().to_string_lossy().into_owned();
    std::fs::write(&src, "int main() { SYNTAX ERROR HERE }\n").unwrap();

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
    assert_ne!(ec, 0);
    assert!(!c, "failed compile must not be cached");

    let (ec2, c2) = compile(
        &mut client,
        &sid,
        &comp,
        &["-c", &src.to_string_lossy(), "-o", &obj.to_string_lossy()],
        &cwd,
    )
    .await;
    assert_ne!(ec2, 0);
    assert!(!c2, "failed compile must NEVER be returned from cache");

    std::fs::write(&src, "int main() { return 0; }\n").unwrap();
    let (ec3, c3) = compile(
        &mut client,
        &sid,
        &comp,
        &["-c", &src.to_string_lossy(), "-o", &obj.to_string_lossy()],
        &cwd,
    )
    .await;
    assert_eq!(ec3, 0);
    assert!(!c3, "new source should be a cache miss");

    shutdown.notify_one();
    server_handle.await.unwrap();
}

// ═══════════════════════════════════════════════════════════════════════
// CONCURRENCY ATTACKS
// ═══════════════════════════════════════════════════════════════════════

#[tokio::test]
#[ignore] // integration: spawns clang 8x concurrently, run with --full
async fn adversarial_concurrent_same_file() {
    let clang = match zccache::test_support::find_clang() {
        Some(p) => p,
        None => return,
    };
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("concurrent.cpp");
    std::fs::write(&src, "int main() { return 0; }\n").unwrap();
    let cwd = tmp.path().to_string_lossy().into_owned();

    let (endpoint, server_handle, shutdown) = start_daemon().await;
    let mut handles = Vec::new();
    for i in 0..8 {
        let ep = endpoint.clone();
        let clang = clang.clone();
        let src = src.clone();
        let cwd = cwd.clone();
        let out_dir = tmp.path().join(format!("out{i}"));
        std::fs::create_dir_all(&out_dir).unwrap();
        let obj = out_dir.join("concurrent.o");
        let log = out_dir.join("log.txt");
        handles.push(tokio::spawn(async move {
            let mut client = zccache::ipc::connect(&ep).await.unwrap();
            let (sid, comp) =
                start_session(&mut client, &clang, &cwd, &log.to_string_lossy()).await;
            let (ec, _c) = compile(
                &mut client,
                &sid,
                &comp,
                &["-c", &src.to_string_lossy(), "-o", &obj.to_string_lossy()],
                &cwd,
            )
            .await;
            assert_eq!(ec, 0, "concurrent compile {i} should succeed");
            assert!(obj.exists(), "output should exist for compile {i}");
            std::fs::read(&obj).unwrap()
        }));
    }
    let mut results = Vec::new();
    for h in handles {
        results.push(h.await.unwrap());
    }
    for (i, obj) in results.iter().enumerate() {
        assert!(!obj.is_empty(), "concurrent compile {i} produced empty .o");
    }
    let first_len = results[0].len();
    for (i, obj) in results.iter().enumerate().skip(1) {
        assert_eq!(
            first_len,
            obj.len(),
            "concurrent compile {i} produced .o of different size than compile 0"
        );
    }
    shutdown.notify_one();
    server_handle.await.unwrap();
}

#[tokio::test]
#[ignore] // integration: spawns clang 10x concurrently, run with --full
async fn adversarial_concurrent_different_files() {
    let clang = match zccache::test_support::find_clang() {
        Some(p) => p,
        None => return,
    };
    let tmp = tempfile::tempdir().unwrap();
    let cwd = tmp.path().to_string_lossy().into_owned();
    let (endpoint, server_handle, shutdown) = start_daemon().await;
    let n = 10;
    let mut handles = Vec::new();
    for i in 0..n {
        let ep = endpoint.clone();
        let clang = clang.clone();
        let cwd = cwd.clone();
        let src = tmp.path().join(format!("file{i}.cpp"));
        let obj = tmp.path().join(format!("file{i}.o"));
        let log = tmp.path().join(format!("log{i}.txt"));
        std::fs::write(&src, format!("int main() {{ return {i}; }}\n")).unwrap();
        handles.push(tokio::spawn(async move {
            let mut client = zccache::ipc::connect(&ep).await.unwrap();
            let (sid, comp) =
                start_session(&mut client, &clang, &cwd, &log.to_string_lossy()).await;
            let (ec, c) = compile(
                &mut client,
                &sid,
                &comp,
                &["-c", &src.to_string_lossy(), "-o", &obj.to_string_lossy()],
                &cwd,
            )
            .await;
            assert_eq!(ec, 0);
            assert!(!c, "first compile of file{i} should be a miss");
            let obj_data = std::fs::read(&obj).unwrap();
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
            assert!(c, "second compile of file{i} should be a hit");
            let obj_data2 = std::fs::read(&obj).unwrap();
            assert_eq!(obj_data, obj_data2, "cached .o for file{i} must match");
            obj_data
        }));
    }
    let mut results = Vec::new();
    for h in handles {
        results.push(h.await.unwrap());
    }
    for i in 1..results.len() {
        assert_ne!(
            results[0], results[i],
            "file0 and file{i} should produce different .o (different source)"
        );
    }
    shutdown.notify_one();
    server_handle.await.unwrap();
}

#[tokio::test]
#[ignore] // integration: spawns clang, run with --full
async fn adversarial_cross_session_cache_sharing() {
    let clang = match zccache::test_support::find_clang() {
        Some(p) => p,
        None => return,
    };
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("shared.cpp");
    let obj1 = tmp.path().join("shared1.o");
    let obj2 = tmp.path().join("shared2.o");
    let log1 = tmp.path().join("log1.txt");
    let log2 = tmp.path().join("log2.txt");
    let cwd = tmp.path().to_string_lossy().into_owned();
    std::fs::write(&src, "int main() { return 7; }\n").unwrap();

    let (endpoint, server_handle, shutdown) = start_daemon().await;
    let mut client1 = zccache::ipc::connect(&endpoint).await.unwrap();
    let (sid1, comp1) = start_session(&mut client1, &clang, &cwd, &log1.to_string_lossy()).await;
    let (ec, c) = compile(
        &mut client1,
        &sid1,
        &comp1,
        &["-c", &src.to_string_lossy(), "-o", &obj1.to_string_lossy()],
        &cwd,
    )
    .await;
    assert_eq!(ec, 0);
    assert!(!c, "session 1 first compile should be a miss");
    let obj1_data = std::fs::read(&obj1).unwrap();

    let mut client2 = zccache::ipc::connect(&endpoint).await.unwrap();
    let (sid2, comp2) = start_session(&mut client2, &clang, &cwd, &log2.to_string_lossy()).await;
    assert_ne!(sid1, sid2, "sessions should have different IDs");
    let (ec, c) = compile(
        &mut client2,
        &sid2,
        &comp2,
        &["-c", &src.to_string_lossy(), "-o", &obj2.to_string_lossy()],
        &cwd,
    )
    .await;
    assert_eq!(ec, 0);
    assert!(c, "session 2 should hit session 1's cached result");
    assert_eq!(
        obj1_data,
        std::fs::read(&obj2).unwrap(),
        "cross-session cached .o must match"
    );
    assert!(std::fs::read_to_string(&log1).unwrap().contains("[MISS]"));
    assert!(std::fs::read_to_string(&log2).unwrap().contains("[HIT]"));

    shutdown.notify_one();
    server_handle.await.unwrap();
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

// ═══════════════════════════════════════════════════════════════════════
// COMPILER OVERRIDE
// ═══════════════════════════════════════════════════════════════════════

#[tokio::test]
#[ignore] // integration: spawns clang, run with --full
async fn compiler_override_uses_wrapped_compiler() {
    let clangpp = match zccache::test_support::find_clang() {
        Some(p) => p,
        None => return,
    };
    let clang = clangpp
        .parent()
        .unwrap()
        .join(if cfg!(windows) { "clang.exe" } else { "clang" });
    if !clang.exists() {
        eprintln!("SKIP: clang not found at {}", clang.display());
        return;
    }

    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("test.c");
    let obj = tmp.path().join("test.o");
    let log = tmp.path().join("log.txt");
    let cwd = tmp.path().to_string_lossy().into_owned();
    std::fs::write(&src, "struct Point { int x; int y; };\nint main(void) {\n\tstruct Point p = { .x = 1, .y = 2 };\n\treturn p.x + p.y - 3;\n}\n").unwrap();

    let (endpoint, server_handle, shutdown) = start_daemon().await;
    let mut client = zccache::ipc::connect(&endpoint).await.unwrap();
    let (sid, _clangpp_compiler) =
        start_session(&mut client, &clangpp, &cwd, &log.to_string_lossy()).await;

    client
        .send(&Request::Compile {
            session_id: sid.clone(),
            args: vec![
                "-c".into(),
                "-std=c11".into(),
                src.to_string_lossy().into_owned(),
                "-o".into(),
                obj.to_string_lossy().into_owned(),
            ],
            cwd: cwd.clone().into(),
            compiler: clang.to_string_lossy().into_owned().into(),
            env: None,
            stdin: Vec::new(),
        })
        .await
        .unwrap();

    match client.recv().await.unwrap() {
        Some(Response::CompileResult {
            exit_code,
            cached,
            stderr,
            ..
        }) => {
            let stderr_str = String::from_utf8_lossy(&stderr);
            assert_eq!(
                exit_code, 0,
                "C file with -std=c11 should compile with clang override. stderr: {stderr_str}"
            );
            assert!(!cached, "first compile should be a miss");
            assert!(
                !stderr_str.contains("not valid for C++"),
                "compiler override should use clang, not clang++. stderr: {stderr_str}"
            );
        }
        Some(Response::Error { message }) => {
            panic!("compile error (compiler override not working?): {message}")
        }
        other => panic!("expected CompileResult, got: {other:?}"),
    }
    assert!(obj.exists(), "object file should be produced");
    shutdown.notify_one();
    server_handle.await.unwrap();
}
