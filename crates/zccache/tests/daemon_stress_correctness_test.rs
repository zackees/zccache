//! Adversarial stress tests for the compilation cache — correctness + concurrency.
//!
//! These tests intentionally try to break the caching with corner cases:
//! - Source modifications must invalidate cache
//! - Different flags must produce different cache entries
//! - Compile errors must NOT be cached
//! - Concurrent compilations must not corrupt state
//!
//! See `daemon_stress_edges_test.rs` for path/output/empty-source edge cases and
//! `daemon_stress_compiler_test.rs` for compiler-override coverage.

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
