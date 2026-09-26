//! GCC implicit PCH invalidation (#1609).
//!
//! GCC substitutes `pch.h.gch` for `-include pch.h` without saying so: its
//! depfile lists neither the `.gch` nor the headers baked into it. After a
//! header that `pch.h` includes changes and the `.gch` is regenerated, the
//! consumer must miss. `pch.h` itself is byte-identical, so only the `.gch`
//! can carry that change into the key.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::panic_in_result_fn,
    clippy::unwrap_in_result
)]

use zccache::daemon::DaemonServer;
use zccache::protocol::{Request, Response};

#[cfg(unix)]
type ClientConn = zccache::ipc::IpcConnection;
#[cfg(windows)]
type ClientConn = zccache::ipc::IpcClientConnection;

async fn start_daemon() -> (
    String,
    tokio::task::JoinHandle<()>,
    std::sync::Arc<kernal_api::async_engine::Notify>,
    tempfile::TempDir,
) {
    let endpoint = zccache::ipc::unique_test_endpoint();
    let cache_root = tempfile::tempdir().expect("daemon cache tempdir");
    let cache_dir: zccache::core::NormalizedPath = cache_root.path().join("zccache-cache").into();
    let mut server = DaemonServer::bind_with_cache_dir(&endpoint, &cache_dir).unwrap();
    let shutdown = server.shutdown_handle();
    let handle = tokio::spawn(async move { server.run(0).await.unwrap() });
    (endpoint, handle, shutdown, cache_root)
}

async fn start_session(client: &mut ClientConn, cwd: &str) -> String {
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
    compiler: &str,
    args: Vec<String>,
    cwd: &str,
) -> (i32, bool, String) {
    client
        .send(&Request::Compile {
            session_id: session_id.to_string(),
            args,
            cwd: cwd.to_string().into(),
            compiler: compiler.to_string().into(),
            env: None,
            stdin: Vec::new(),
        })
        .await
        .unwrap();
    loop {
        match client.recv().await.unwrap() {
            Some(Response::CompileProgress { .. }) => continue,
            Some(Response::CompileResult {
                exit_code,
                cached,
                stderr,
                ..
            }) => {
                break (
                    exit_code,
                    cached,
                    String::from_utf8_lossy(&stderr).into_owned(),
                )
            }
            Some(Response::Error { message }) => panic!("compile error: {message}"),
            other => panic!("expected CompileResult, got: {other:?}"),
        }
    }
}

#[tokio::test]
#[ignore] // integration: spawns g++, run with --full
async fn gcc_implicit_pch_sub_header_change_misses() {
    let Some(gxx) = zccache::test_support::find_on_path("g++") else {
        eprintln!("skipping test: g++ not found");
        return;
    };
    let compiler = gxx.to_string_lossy().into_owned();
    let tmp = tempfile::tempdir().unwrap();
    let cwd = tmp.path().to_string_lossy().into_owned();
    let sub = tmp.path().join("sub.h");
    let header = tmp.path().join("pch.h");
    let gch = tmp.path().join("pch.h.gch");
    let source = tmp.path().join("main.cpp");
    let obj = tmp.path().join("main.o");
    std::fs::write(&sub, "#define SUB_VALUE 42\n").unwrap();
    std::fs::write(&header, "#include \"sub.h\"\n").unwrap();
    std::fs::write(&source, "int main() { return SUB_VALUE; }\n").unwrap();

    let path = |p: &std::path::Path| p.to_string_lossy().into_owned();
    let gch_args = || {
        let (h, g) = (path(&header), path(&gch));
        vec![
            "-x".into(),
            "c++-header".into(),
            "-c".into(),
            h,
            "-o".into(),
            g,
        ]
    };
    let consumer_args = || {
        let (s, o, h) = (path(&source), path(&obj), path(&header));
        vec!["-c".into(), s, "-o".into(), o, "-include".into(), h]
    };

    let (endpoint, server_handle, shutdown, _cache_root) = start_daemon().await;
    let mut client = zccache::ipc::connect(&endpoint).await.unwrap();
    let sid = start_session(&mut client, &cwd).await;

    let (code, _, err) = compile(&mut client, &sid, &compiler, gch_args(), &cwd).await;
    assert_eq!(code, 0, "gch generation failed: {err}");
    let (code, cached, err) = compile(&mut client, &sid, &compiler, consumer_args(), &cwd).await;
    assert_eq!(code, 0, "consumer failed: {err}");
    assert!(!cached, "first consumer compile should miss");
    std::fs::remove_file(&obj).unwrap();
    let (code, cached, _) = compile(&mut client, &sid, &compiler, consumer_args(), &cwd).await;
    assert_eq!(code, 0);
    assert!(cached, "unchanged consumer should hit");

    // sub.h changes; pch.h does not. Regenerate the .gch.
    std::thread::sleep(std::time::Duration::from_millis(100));
    std::fs::write(&sub, "#define SUB_VALUE 99\n").unwrap();
    std::thread::sleep(std::time::Duration::from_millis(1000));
    let (code, cached, _) = compile(&mut client, &sid, &compiler, gch_args(), &cwd).await;
    assert_eq!(code, 0);
    assert!(!cached, "gch regeneration after sub.h change must miss");

    std::fs::remove_file(&obj).unwrap();
    let (code, cached, _) = compile(&mut client, &sid, &compiler, consumer_args(), &cwd).await;
    assert_eq!(code, 0);
    assert!(
        !cached,
        "consumer after sub.h change MUST miss: GCC used the regenerated pch.h.gch"
    );
    std::fs::remove_file(&obj).unwrap();
    let (code, cached, _) = compile(&mut client, &sid, &compiler, consumer_args(), &cwd).await;
    assert_eq!(code, 0);
    assert!(cached, "consumer with the unchanged new gch should hit");

    shutdown.notify_one();
    server_handle.await.unwrap();
}
