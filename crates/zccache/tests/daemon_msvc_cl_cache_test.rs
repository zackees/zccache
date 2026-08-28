//! Integration test for issue #1530: MSVC `cl.exe` compiles must cache.
//!
//! zccache 1.13.12 passed every `cl.exe` compile through as non-cacheable:
//! zero artifacts stored, zero hits, on a 2,300-compile LLVM build. The cause
//! was not the argument parser (`compiler_clang_cl_classification.rs` already
//! pins that `cl.exe /nologo /c src.c /Fosrc.obj` classifies as `Cacheable`)
//! but system-include discovery: the daemon probed `cl.exe -v -E -x c++ NUL`,
//! `cl.exe` has no such discovery mode, the probe printed no
//! `#include <...> search starts here:` section, and the resulting
//! "succeeded but zero paths" outcome tripped the issue #1167 degraded-probe
//! guard, which diverts the compile to the uncached bypass. #1167 also (by
//! design) never memoizes an empty result, so the probe re-ran and the bypass
//! re-fired on every single compile.
//!
//! `cl.exe` resolves `#include <...>` against `%INCLUDE%`, exported by
//! `vcvars`, so the daemon now reads the roots from the forwarded client
//! environment and never probes.
//!
//! This test is `#[ignore]`d and additionally skips unless `cl.exe` is on
//! PATH — it needs a Developer Command Prompt (`vcvars64`). Run with
//! `./test --full` from a vcvars shell on Windows.

#![cfg(windows)]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::panic_in_result_fn,
    clippy::unwrap_in_result
)]

use zccache::daemon::DaemonServer;
use zccache::protocol::{Request, Response};

type ClientConn = zccache::ipc::IpcClientConnection;

async fn start_daemon() -> (
    String,
    tokio::task::JoinHandle<()>,
    std::sync::Arc<tokio::sync::Notify>,
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

async fn start_session(client: &mut ClientConn, cwd: &str, log_file: &str) -> String {
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
    match client.recv().await.unwrap() {
        Some(Response::SessionStarted { session_id, .. }) => session_id,
        other => panic!("expected SessionStarted, got: {other:?}"),
    }
}

struct CompileOutcome {
    exit_code: i32,
    cached: bool,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

async fn compile_full(
    client: &mut ClientConn,
    session_id: &str,
    compiler: &str,
    args: Vec<String>,
    cwd: &str,
) -> CompileOutcome {
    // The wrapper forwards the full client environment; the daemon reads
    // `%INCLUDE%` out of it for `cl.exe`. Passing `None` here would leave the
    // compiler unable to find `<stdio.h>` and is not what a real client does.
    let env: Vec<(String, String)> = std::env::vars().collect();
    client
        .send(&Request::Compile {
            session_id: session_id.to_string(),
            args,
            cwd: cwd.to_string().into(),
            compiler: compiler.to_string().into(),
            env: Some(env),
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
                stdout,
                stderr,
            }) => {
                break CompileOutcome {
                    exit_code,
                    cached,
                    stdout: (*stdout).clone(),
                    stderr: (*stderr).clone(),
                }
            }
            Some(Response::Error { message }) => panic!("compile error: {message}"),
            other => panic!("expected CompileResult, got: {other:?}"),
        }
    }
}

async fn compile(
    client: &mut ClientConn,
    session_id: &str,
    compiler: &str,
    args: Vec<String>,
    cwd: &str,
) -> (i32, bool, Vec<u8>) {
    let outcome = compile_full(client, session_id, compiler, args, cwd).await;
    (outcome.exit_code, outcome.cached, outcome.stderr)
}

/// The issue's exact repro: the same `cl.exe` compile twice. The second one
/// must be a cache hit. Before the fix both were reported non-cacheable and
/// the artifact store stayed empty.
#[tokio::test]
#[ignore] // integration: needs a vcvars Developer Command Prompt with cl.exe
async fn msvc_cl_second_identical_compile_is_a_cache_hit() {
    let Some(cl) = zccache::test_support::find_on_path("cl.exe") else {
        eprintln!("skipping: cl.exe not found (run from a vcvars shell)");
        return;
    };
    if std::env::var_os("INCLUDE").is_none() {
        eprintln!("skipping: INCLUDE unset (run from a vcvars shell)");
        return;
    }

    let tmp = tempfile::tempdir().unwrap();
    let cwd = tmp.path().to_string_lossy().into_owned();
    let log_dir = tmp.path().join("logs");
    std::fs::create_dir_all(&log_dir).unwrap();
    let log = log_dir.join("session.log");

    let source = tmp.path().join("simple.c");
    let object = tmp.path().join("simple.obj");
    std::fs::write(&source, "#include <stdio.h>\nint f(void){return 42;}\n").unwrap();

    let compiler = cl.to_string_lossy().into_owned();
    let args = vec![
        "/nologo".to_string(),
        "/c".to_string(),
        source.to_string_lossy().into_owned(),
        format!("/Fo{}", object.display()),
    ];

    let (endpoint, server_handle, shutdown, _cache_root) = start_daemon().await;
    let mut client = zccache::ipc::connect(&endpoint).await.unwrap();
    let sid = start_session(&mut client, &cwd, &log.to_string_lossy()).await;

    let (code, cached, stderr) = compile(&mut client, &sid, &compiler, args.clone(), &cwd).await;
    assert_eq!(
        code,
        0,
        "first compile failed: {}",
        String::from_utf8_lossy(&stderr)
    );
    assert!(!cached, "first compile must be a miss");
    assert!(object.exists(), "first compile produced no object");
    let expected = std::fs::read(&object).unwrap();

    // Delete the object so a hit has to materialize it, not just leave it be.
    std::fs::remove_file(&object).unwrap();

    let (code, cached, stderr) = compile(&mut client, &sid, &compiler, args, &cwd).await;
    assert_eq!(
        code,
        0,
        "second compile failed: {}",
        String::from_utf8_lossy(&stderr)
    );
    assert!(
        cached,
        "issue #1530: the second identical cl.exe compile must be a cache hit, \
         not another non-cacheable passthrough"
    );
    assert_eq!(
        std::fs::read(&object).unwrap(),
        expected,
        "cache hit materialized a different object"
    );

    shutdown.notify_one();
    server_handle.await.unwrap();
}

/// Editing a header must invalidate the cached object.
///
/// The same issue #1530 change is what makes this reachable at all — but it
/// also exposed a second defect: MSVC writes its `/showIncludes` notes to
/// **stdout**, while the daemon scanned stderr. Zero dependencies were
/// recorded, so the compile below used to come back as a (wrong) cache hit
/// carrying the pre-edit object.
#[tokio::test]
#[ignore] // integration: needs a vcvars Developer Command Prompt with cl.exe
async fn msvc_cl_header_edit_invalidates_the_cached_object() {
    let Some(cl) = zccache::test_support::find_on_path("cl.exe") else {
        eprintln!("skipping: cl.exe not found (run from a vcvars shell)");
        return;
    };
    if std::env::var_os("INCLUDE").is_none() {
        eprintln!("skipping: INCLUDE unset (run from a vcvars shell)");
        return;
    }

    let tmp = tempfile::tempdir().unwrap();
    let cwd = tmp.path().to_string_lossy().into_owned();
    let log_dir = tmp.path().join("logs");
    std::fs::create_dir_all(&log_dir).unwrap();
    let log = log_dir.join("session.log");

    let header = tmp.path().join("local.h");
    let source = tmp.path().join("hdr.c");
    let object = tmp.path().join("hdr.obj");
    std::fs::write(&header, "#define LOCAL_VALUE 42\n").unwrap();
    // A *computed* include: zccache's own header scanner cannot resolve
    // `#include H` without preprocessing, so this dependency can only come from
    // the `/showIncludes` scan. A plain `#include "local.h"` would be resolved
    // by the scanner and would pass even with the stdout defect still present.
    std::fs::write(
        &source,
        "#include <stdio.h>\n#define H \"local.h\"\n#include H\nint f(void){ return LOCAL_VALUE + (int)sizeof(FILE); }\n",
    )
    .unwrap();

    let compiler = cl.to_string_lossy().into_owned();
    let args = vec![
        "/nologo".to_string(),
        "/c".to_string(),
        source.to_string_lossy().into_owned(),
        format!("/Fo{}", object.display()),
    ];

    let (endpoint, server_handle, shutdown, _cache_root) = start_daemon().await;
    let mut client = zccache::ipc::connect(&endpoint).await.unwrap();
    let sid = start_session(&mut client, &cwd, &log.to_string_lossy()).await;

    let (code, cached, stderr) = compile(&mut client, &sid, &compiler, args.clone(), &cwd).await;
    assert_eq!(
        code,
        0,
        "cold compile failed: {}",
        String::from_utf8_lossy(&stderr)
    );
    assert!(!cached, "cold compile must be a miss");
    let before = std::fs::read(&object).unwrap();

    let (_, cached, _) = compile(&mut client, &sid, &compiler, args.clone(), &cwd).await;
    assert!(cached, "unchanged recompile must be a hit");

    // Change the header's *content*, not just its timestamp.
    std::fs::write(&header, "#define LOCAL_VALUE 43\n").unwrap();
    // NTFS mtime granularity does not always advance on rapid rewrites.
    std::thread::sleep(std::time::Duration::from_millis(1100));

    let (code, cached, stderr) = compile(&mut client, &sid, &compiler, args, &cwd).await;
    assert_eq!(
        code,
        0,
        "post-edit compile failed: {}",
        String::from_utf8_lossy(&stderr)
    );
    assert!(
        !cached,
        "issue #1530: editing local.h must invalidate the cached object — a hit \
         here means /showIncludes dependencies were never recorded"
    );
    assert_ne!(
        std::fs::read(&object).unwrap(),
        before,
        "post-edit object is byte-identical to the pre-edit one"
    );

    shutdown.notify_one();
    server_handle.await.unwrap();
}

/// A caller that passes `/showIncludes` itself must keep receiving the notes —
/// on a cache hit as well as on a miss.
///
/// CMake + Ninja MSVC builds pass the flag and parse `Note: including file:`
/// out of stdout to build their own depfiles. Two failure modes are guarded
/// here: stripping notes the caller asked for, and letting a plain compile's
/// stripped stdout be replayed to a `/showIncludes` caller, which is possible
/// because the argument parser consumes the flag before it reaches the cache
/// key (hence `keys::msvc_show_includes_key_flags`).
#[tokio::test]
#[ignore] // integration: needs a vcvars Developer Command Prompt with cl.exe
async fn msvc_cl_caller_supplied_show_includes_survives_a_cache_hit() {
    let Some(cl) = zccache::test_support::find_on_path("cl.exe") else {
        eprintln!("skipping: cl.exe not found (run from a vcvars shell)");
        return;
    };
    if std::env::var_os("INCLUDE").is_none() {
        eprintln!("skipping: INCLUDE unset (run from a vcvars shell)");
        return;
    }

    let tmp = tempfile::tempdir().unwrap();
    let cwd = tmp.path().to_string_lossy().into_owned();
    let log_dir = tmp.path().join("logs");
    std::fs::create_dir_all(&log_dir).unwrap();
    let log = log_dir.join("session.log");

    let source = tmp.path().join("notes.c");
    let object = tmp.path().join("notes.obj");
    std::fs::write(
        &source,
        "#include <stdio.h>\nint f(void){return (int)sizeof(FILE);}\n",
    )
    .unwrap();

    let compiler = cl.to_string_lossy().into_owned();
    let plain = vec![
        "/nologo".to_string(),
        "/c".to_string(),
        source.to_string_lossy().into_owned(),
        format!("/Fo{}", object.display()),
    ];
    let mut with_notes = plain.clone();
    with_notes.insert(1, "/showIncludes".to_string());

    const NOTE: &str = "Note: including file:";

    let (endpoint, server_handle, shutdown, _cache_root) = start_daemon().await;
    let mut client = zccache::ipc::connect(&endpoint).await.unwrap();
    let sid = start_session(&mut client, &cwd, &log.to_string_lossy()).await;

    // Populate the cache with a PLAIN compile. Its stored stdout has the
    // daemon's injected notes stripped; it must never be replayed below.
    let cold = compile_full(&mut client, &sid, &compiler, plain.clone(), &cwd).await;
    assert_eq!(
        cold.exit_code,
        0,
        "plain compile failed: {}",
        String::from_utf8_lossy(&cold.stderr)
    );
    let warm = compile_full(&mut client, &sid, &compiler, plain, &cwd).await;
    assert!(warm.cached, "plain recompile must be a hit");
    assert!(
        !String::from_utf8_lossy(&warm.stdout).contains(NOTE),
        "daemon-injected /showIncludes notes leaked into a plain caller's stdout"
    );

    // The same compile with the caller's own /showIncludes: a miss (different
    // key), and the notes must be present.
    let cold = compile_full(&mut client, &sid, &compiler, with_notes.clone(), &cwd).await;
    assert_eq!(
        cold.exit_code,
        0,
        "/showIncludes compile failed: {}",
        String::from_utf8_lossy(&cold.stderr)
    );
    assert!(
        !cold.cached,
        "a caller-passed /showIncludes compile must not reuse the plain compile's entry"
    );
    assert!(
        String::from_utf8_lossy(&cold.stdout).contains(NOTE),
        "caller-requested /showIncludes notes were stripped on a miss"
    );

    let warm = compile_full(&mut client, &sid, &compiler, with_notes, &cwd).await;
    assert!(warm.cached, "second /showIncludes compile must be a hit");
    assert!(
        String::from_utf8_lossy(&warm.stdout).contains(NOTE),
        "caller-requested /showIncludes notes were missing from the replayed hit — \
         Ninja would write an empty depfile and silently under-rebuild"
    );

    shutdown.notify_one();
    server_handle.await.unwrap();
}
