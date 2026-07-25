//! End-to-end cache contract for Dylint's nested compiler invocation.

#![cfg(unix)]
#![allow(
    clippy::expect_used,
    clippy::panic,
    clippy::panic_in_result_fn,
    clippy::unwrap_used
)]

use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;

use zccache::core::NormalizedPath;
use zccache::daemon::DaemonServer;
use zccache::protocol::{Request, Response};

type ClientConn = zccache::ipc::IpcConnection;

async fn start_daemon() -> (
    String,
    tokio::task::JoinHandle<()>,
    Arc<tokio::sync::Notify>,
) {
    let endpoint = zccache::ipc::unique_test_endpoint();
    let mut server = DaemonServer::bind(&endpoint).unwrap();
    let shutdown = server.shutdown_handle();
    let handle = tokio::spawn(async move {
        server.run(0).await.unwrap();
    });
    (endpoint, handle, shutdown)
}

async fn start_session(client: &mut ClientConn) -> String {
    client
        .send(&Request::SessionStart {
            client_pid: std::process::id(),
            working_dir: std::env::current_dir().unwrap().into(),
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
        other => panic!("expected SessionStarted, got {other:?}"),
    }
}

async fn compile(
    client: &mut ClientConn,
    session_id: &str,
    driver: &std::path::Path,
    args: &[String],
    cwd: &std::path::Path,
    dylint_libs: &str,
    dylint_metadata: &str,
) -> (i32, bool, Vec<u8>, Vec<u8>) {
    let mut env: Vec<(String, String)> = std::env::vars().collect();
    env.retain(|(name, _)| {
        name != "DYLINT_LIBS"
            && name != "DYLINT_METADATA"
            && name != "ZCCACHE_DYLINT_CACHE_INPUT_HASH"
    });
    env.push(("DYLINT_LIBS".to_string(), dylint_libs.to_string()));
    env.push(("DYLINT_METADATA".to_string(), dylint_metadata.to_string()));
    client
        .send(&Request::Compile {
            session_id: session_id.to_string(),
            args: args.to_vec(),
            cwd: cwd.into(),
            compiler: NormalizedPath::new(driver),
            env: Some(env),
            stdin: Vec::new(),
        })
        .await
        .unwrap();

    match client.recv().await.unwrap() {
        Some(Response::CompileResult {
            exit_code,
            cached,
            stdout,
            stderr,
        }) => (
            exit_code,
            cached,
            Arc::unwrap_or_clone(stdout),
            Arc::unwrap_or_clone(stderr),
        ),
        Some(Response::Error { message }) => panic!("compile error: {message}"),
        other => panic!("unexpected response: {other:?}"),
    }
}

fn write_driver(path: &std::path::Path, diagnostic: &str) {
    std::fs::write(
        path,
        format!(
            "#!/bin/sh\nprintf '%s\\n' '{diagnostic}' >&2\ninner=\"$1\"\nshift\nexec \"$inner\" \"$@\"\n"
        ),
    )
    .unwrap();
    let mut permissions = std::fs::metadata(path).unwrap().permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(path, permissions).unwrap();
}

fn write_dylint_link(path: &std::path::Path) {
    std::fs::write(
        path,
        r#"#!/bin/sh
set -eu
cc "$@"
out=
previous=
for arg in "$@"; do
    if [ "$previous" = "-o" ]; then out="$arg"; break; fi
    previous="$arg"
done
[ -n "$out" ]
parent=$(dirname "$out")
if [ "$(basename "$parent")" = "deps" ]; then parent=$(dirname "$parent"); fi
pkg=$(printf '%s' "$CARGO_PKG_NAME" | tr '-' '_')
cp "$out" "$parent/lib${pkg}@${RUSTUP_TOOLCHAIN}.so"
"#,
    )
    .unwrap();
    let mut permissions = std::fs::metadata(path).unwrap().permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(path, permissions).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "integration-level: starts a real daemon and rustc"]
async fn nested_dylint_hits_and_invalidates_every_external_input() {
    let Some(rustc) = zccache::test_support::find_rustc() else {
        eprintln!("skipping test: rustc not found");
        return;
    };

    zccache::test_support::test_timeout(async move {
        let tmp = tempfile::tempdir().unwrap();
        let cache = tmp.path().join("cache");
        let previous_cache = std::env::var_os("ZCCACHE_CACHE_DIR");
        std::env::set_var("ZCCACHE_CACHE_DIR", &cache);

        let driver = tmp.path().join("dylint-driver");
        let library = tmp.path().join("libworkspace_lint.so");
        let source = tmp.path().join("lib.rs");
        let output = tmp.path().join("libchecked.rlib");
        write_driver(&driver, "workspace lint diagnostic v1");
        std::fs::write(&library, b"lint-library-v1").unwrap();
        std::fs::write(&source, "pub fn checked() -> u32 { 42 }\n").unwrap();

        let args = vec![
            rustc.display().to_string(),
            "--edition".to_string(),
            "2021".to_string(),
            "--crate-type".to_string(),
            "lib".to_string(),
            "--crate-name".to_string(),
            "checked".to_string(),
            "--emit=link".to_string(),
            source.display().to_string(),
            "-o".to_string(),
            output.display().to_string(),
        ];
        let dylint_libs = serde_json::to_string(&vec![library.clone()]).unwrap();

        let (endpoint, server_handle, shutdown) = start_daemon().await;
        let mut client = zccache::ipc::connect(&endpoint).await.unwrap();
        let session_id = start_session(&mut client).await;

        let first = compile(
            &mut client,
            &session_id,
            &driver,
            &args,
            tmp.path(),
            &dylint_libs,
            "metadata-v1",
        )
        .await;
        assert_eq!(first.0, 0);
        assert!(!first.1);
        assert!(String::from_utf8_lossy(&first.3).contains("workspace lint diagnostic v1"));

        std::fs::remove_file(&output).unwrap();
        let warm = compile(
            &mut client,
            &session_id,
            &driver,
            &args,
            tmp.path(),
            &dylint_libs,
            "metadata-v1",
        )
        .await;
        assert_eq!(warm.0, 0);
        assert!(warm.1);
        assert_eq!(warm.2, first.2);
        assert_eq!(warm.3, first.3, "cached diagnostics must be replayed");

        std::fs::write(&library, b"lint-library-v2-with-new-content").unwrap();
        std::fs::remove_file(&output).unwrap();
        let library_changed = compile(
            &mut client,
            &session_id,
            &driver,
            &args,
            tmp.path(),
            &dylint_libs,
            "metadata-v1",
        )
        .await;
        assert!(!library_changed.1, "library bytes must invalidate the hit");

        std::fs::remove_file(&output).unwrap();
        let env_changed = compile(
            &mut client,
            &session_id,
            &driver,
            &args,
            tmp.path(),
            &dylint_libs,
            "metadata-v2",
        )
        .await;
        assert!(!env_changed.1, "DYLINT_* output state must invalidate");

        write_driver(&driver, "workspace lint diagnostic version two");
        std::fs::remove_file(&output).unwrap();
        let driver_changed = compile(
            &mut client,
            &session_id,
            &driver,
            &args,
            tmp.path(),
            &dylint_libs,
            "metadata-v2",
        )
        .await;
        assert!(!driver_changed.1, "driver bytes must invalidate");
        assert!(String::from_utf8_lossy(&driver_changed.3)
            .contains("workspace lint diagnostic version two"));

        std::fs::remove_file(&output).unwrap();
        let malformed = compile(
            &mut client,
            &session_id,
            &driver,
            &args,
            tmp.path(),
            "not-json",
            "metadata-v2",
        )
        .await;
        assert_eq!(malformed.0, 0, "malformed state must fail open");
        assert!(!malformed.1);
        assert!(
            String::from_utf8_lossy(&malformed.3).contains("Dylint cache disabled"),
            "fail-open reason must be visible to the user"
        );

        shutdown.notify_one();
        server_handle.await.unwrap();
        match previous_cache {
            Some(value) => std::env::set_var("ZCCACHE_CACHE_DIR", value),
            None => std::env::remove_var("ZCCACHE_CACHE_DIR"),
        }
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "integration-level: starts a real daemon, rustc, and system linker"]
async fn perf_dylint_library_cdylib_restores_primary_and_toolchain_sidecar() {
    let Some(rustc) = zccache::test_support::find_rustc() else {
        eprintln!("skipping test: rustc not found");
        return;
    };

    zccache::test_support::test_timeout(async move {
        let tmp = tempfile::tempdir().unwrap();
        let cache = tmp.path().join("cache");
        let previous_cache = std::env::var_os("ZCCACHE_CACHE_DIR");
        std::env::set_var("ZCCACHE_CACHE_DIR", &cache);

        let linker = tmp.path().join("dylint-link");
        write_dylint_link(&linker);
        let source = tmp.path().join("lib.rs");
        std::fs::write(
            &source,
            "#[no_mangle]\npub extern \"C\" fn lint_fixture() -> u32 { 42 }\n",
        )
        .unwrap();
        let out_dir = tmp
            .path()
            .join("target/dylint/libraries/nightly/release/deps");
        std::fs::create_dir_all(&out_dir).unwrap();
        let primary = out_dir.join("liblint.so");
        let sidecar = out_dir
            .parent()
            .unwrap()
            .join("liblint@nightly-test-x86_64-unknown-linux-gnu.so");
        let args = vec![
            "--edition=2021".to_string(),
            "--crate-type=cdylib".to_string(),
            "--crate-name=lint".to_string(),
            "--emit=link".to_string(),
            format!("--out-dir={}", out_dir.display()),
            format!("-Clinker={}", linker.display()),
            source.display().to_string(),
        ];
        let (endpoint, server_handle, shutdown) = start_daemon().await;
        let mut client = zccache::ipc::connect(&endpoint).await.unwrap();
        let session_id = start_session(&mut client).await;
        let mut env: Vec<(String, String)> = std::env::vars().collect();
        env.push(("CARGO_PKG_NAME".to_string(), "lint".to_string()));
        env.push((
            "RUSTUP_TOOLCHAIN".to_string(),
            "nightly-test-x86_64-unknown-linux-gnu".to_string(),
        ));

        client
            .send(&Request::Compile {
                session_id: session_id.clone(),
                args: args.clone(),
                cwd: tmp.path().into(),
                compiler: NormalizedPath::new(&rustc),
                env: Some(env.clone()),
                stdin: Vec::new(),
            })
            .await
            .unwrap();
        let first = client.recv().await.unwrap().unwrap();
        assert!(matches!(
            first,
            Response::CompileResult {
                exit_code: 0,
                cached: false,
                ..
            }
        ));
        assert!(primary.is_file());
        assert!(sidecar.is_file());
        assert_eq!(
            std::fs::read(&primary).unwrap(),
            std::fs::read(&sidecar).unwrap()
        );

        std::fs::remove_file(&primary).unwrap();
        std::fs::remove_file(&sidecar).unwrap();
        client
            .send(&Request::Compile {
                session_id,
                args,
                cwd: tmp.path().into(),
                compiler: NormalizedPath::new(&rustc),
                env: Some(env),
                stdin: Vec::new(),
            })
            .await
            .unwrap();
        let warm = client.recv().await.unwrap().unwrap();
        assert!(matches!(
            warm,
            Response::CompileResult {
                exit_code: 0,
                cached: true,
                ..
            }
        ));
        assert!(primary.is_file());
        assert!(sidecar.is_file());
        assert_eq!(
            std::fs::read(&primary).unwrap(),
            std::fs::read(&sidecar).unwrap()
        );

        shutdown.notify_one();
        server_handle.await.unwrap();
        match previous_cache {
            Some(value) => std::env::set_var("ZCCACHE_CACHE_DIR", value),
            None => std::env::remove_var("ZCCACHE_CACHE_DIR"),
        }
    })
    .await;
}
