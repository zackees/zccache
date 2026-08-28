//! End-to-end cache contract for Dylint's nested compiler invocation.

#![cfg(unix)]
#![allow(
    clippy::expect_used,
    clippy::panic,
    clippy::panic_in_result_fn,
    clippy::unwrap_used
)]

use std::os::unix::fs::PermissionsExt;
use std::sync::{Arc, Mutex, MutexGuard};

use zccache::core::NormalizedPath;
use zccache::daemon::DaemonServer;
use zccache::protocol::{Request, Response};

type ClientConn = zccache::ipc::IpcConnection;

/// These integration tests start real daemons that resolve the cache root
/// from the process-global environment on each request. Keep their temporary
/// cache roots isolated even when the test harness runs them concurrently.
static CACHE_DIR_ENV_LOCK: Mutex<()> = Mutex::new(());

struct CacheDirEnvGuard {
    previous: Option<std::ffi::OsString>,
    _lock: MutexGuard<'static, ()>,
}

impl CacheDirEnvGuard {
    fn set(cache_dir: &std::path::Path) -> Self {
        let lock = CACHE_DIR_ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let previous = std::env::var_os("ZCCACHE_CACHE_DIR");
        std::env::set_var("ZCCACHE_CACHE_DIR", cache_dir);
        Self {
            previous,
            _lock: lock,
        }
    }
}

impl Drop for CacheDirEnvGuard {
    fn drop(&mut self) {
        match self.previous.take() {
            Some(value) => std::env::set_var("ZCCACHE_CACHE_DIR", value),
            None => std::env::remove_var("ZCCACHE_CACHE_DIR"),
        }
    }
}

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

async fn start_session(client: &mut ClientConn, log_file: Option<NormalizedPath>) -> String {
    client
        .send(&Request::SessionStart {
            client_pid: std::process::id(),
            working_dir: std::env::current_dir().unwrap().into(),
            log_file,
            track_stats: true,
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

#[allow(clippy::too_many_arguments)] // Test request fixture mirrors the wire fields.
async fn compile(
    client: &mut ClientConn,
    session_id: &str,
    driver: &std::path::Path,
    args: &[String],
    cwd: &std::path::Path,
    dylint_libs: &str,
    dylint_metadata: &str,
    path_remap: bool,
) -> (i32, bool, Vec<u8>, Vec<u8>) {
    let mut env: Vec<(String, String)> = std::env::vars().collect();
    env.retain(|(name, _)| {
        name != "DYLINT_LIBS"
            && name != "DYLINT_METADATA"
            && name != "ZCCACHE_PATH_REMAP"
            && name != "ZCCACHE_WORKTREE_ROOT"
            && name != "ZCCACHE_DYLINT_CACHE_INPUT_HASH"
    });
    env.push(("DYLINT_LIBS".to_string(), dylint_libs.to_string()));
    env.push(("DYLINT_METADATA".to_string(), dylint_metadata.to_string()));
    if path_remap {
        env.push(("ZCCACHE_PATH_REMAP".to_string(), "auto".to_string()));
    }
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

    loop {
        match client.recv().await.unwrap() {
            Some(Response::CompileProgress { .. }) => continue,
            Some(Response::CompileResult {
                exit_code,
                cached,
                stdout,
                stderr,
            }) => {
                break (
                    exit_code,
                    cached,
                    Arc::unwrap_or_clone(stdout),
                    Arc::unwrap_or_clone(stderr),
                )
            }
            Some(Response::Error { message }) => panic!("compile error: {message}"),
            other => panic!("unexpected response: {other:?}"),
        }
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
        let _cache_env = CacheDirEnvGuard::set(&cache);

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
        let session_id = start_session(&mut client, None).await;

        let first = compile(
            &mut client,
            &session_id,
            &driver,
            &args,
            tmp.path(),
            &dylint_libs,
            "metadata-v1",
            false,
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
            false,
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
            false,
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
            false,
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
            false,
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
            false,
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
    })
    .await;
}

/// A Dylint driver's build-style invocation can supply a later Cargo
/// check-style metadata request through the rustc emit-compat alias.  The
/// cache entry's cold filename selects the payload, but must never replace the
/// current request's identity-derived destination.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "integration-level: starts a real daemon and rustc"]
async fn dylint_metadata_compat_hit_uses_current_cargo_destination() {
    let Some(rustc) = zccache::test_support::find_rustc() else {
        eprintln!("skipping test: rustc not found");
        return;
    };

    zccache::test_support::test_timeout(async move {
        let tmp = tempfile::tempdir().unwrap();
        let cache = tmp.path().join("cache");
        let _cache_env = CacheDirEnvGuard::set(&cache);
        let driver = tmp.path().join("dylint-driver");
        let library = tmp.path().join("libworkspace_lint.so");
        let source = tmp.path().join("lib.rs");
        let cold_dir = tmp.path().join("target/debug/deps");
        let warm_dir = tmp.path().join("target/check/deps");
        std::fs::create_dir_all(&cold_dir).unwrap();
        std::fs::create_dir_all(&warm_dir).unwrap();
        write_driver(&driver, "metadata-compatible Dylint diagnostic");
        std::fs::write(&library, b"metadata-compatible-lint-library").unwrap();
        std::fs::write(&source, "pub fn checked() -> u32 { 42 }\n").unwrap();

        let dylint_libs = serde_json::to_string(&vec![library]).unwrap();
        let cold_metadata = cold_dir.join("libchecked-cold.rmeta");
        let warm_metadata = warm_dir.join("libchecked-warm.rmeta");
        let cold_args = vec![
            rustc.display().to_string(),
            "--edition=2021".to_string(),
            "--crate-type=lib".to_string(),
            "--crate-name=checked".to_string(),
            "--emit=dep-info,metadata,link".to_string(),
            "-Cmetadata=cold".to_string(),
            "-Cextra-filename=-cold".to_string(),
            "--out-dir".to_string(),
            cold_dir.display().to_string(),
            source.display().to_string(),
        ];
        let warm_args = vec![
            rustc.display().to_string(),
            "--edition=2021".to_string(),
            "--crate-type=lib".to_string(),
            "--crate-name=checked".to_string(),
            "--emit=dep-info,metadata".to_string(),
            "-Cmetadata=warm".to_string(),
            "-Cextra-filename=-warm".to_string(),
            "--out-dir".to_string(),
            warm_dir.display().to_string(),
            source.display().to_string(),
        ];

        let (endpoint, server_handle, shutdown) = start_daemon().await;
        let mut client = zccache::ipc::connect(&endpoint).await.unwrap();
        let session_id = start_session(&mut client, None).await;

        let cold = compile(
            &mut client,
            &session_id,
            &driver,
            &cold_args,
            tmp.path(),
            &dylint_libs,
            "same-dylint-verdict",
            false,
        )
        .await;
        assert_eq!(cold.0, 0, "cold Dylint build should succeed");
        assert!(!cold.1, "cold Dylint build should populate the cache");
        let expected_metadata = std::fs::read(&cold_metadata).unwrap();
        std::fs::remove_file(&cold_metadata).unwrap();
        assert!(
            !warm_metadata.exists(),
            "the distinct Cargo check destination must start absent"
        );

        let warm = compile(
            &mut client,
            &session_id,
            &driver,
            &warm_args,
            tmp.path(),
            &dylint_libs,
            "same-dylint-verdict",
            false,
        )
        .await;
        assert_eq!(warm.0, 0, "metadata-compatible request should succeed");
        assert!(
            warm.1,
            "check-style request must use the cached build artifact"
        );
        assert_eq!(std::fs::read(&warm_metadata).unwrap(), expected_metadata);
        assert!(
            !cold_metadata.exists(),
            "a compatible hit must not replay into the stale cold-build destination"
        );

        shutdown.notify_one();
        server_handle.await.unwrap();
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "integration-level: starts a real daemon and rustc"]
async fn plain_and_dylint_share_artifact_bytes_but_not_verdicts() {
    let Some(rustc) = zccache::test_support::find_rustc() else {
        eprintln!("skipping test: rustc not found");
        return;
    };

    zccache::test_support::test_timeout(async move {
        let tmp = tempfile::tempdir().unwrap();
        let cache = tmp.path().join("cache");
        let _cache_env = CacheDirEnvGuard::set(&cache);
        let driver = tmp.path().join("dylint-driver");
        let library = tmp.path().join("libworkspace_lint.so");
        let source = tmp.path().join("lib.rs");
        let output = tmp.path().join("libchecked.rlib");
        write_driver(&driver, "workspace lint verdict");
        std::fs::write(&library, b"lint-library").unwrap();
        std::fs::write(&source, "pub fn checked() -> u32 { 42 }\n").unwrap();

        let plain_args = vec![
            "--edition=2021".to_string(),
            "--crate-type=lib".to_string(),
            "--crate-name=checked".to_string(),
            "--emit=link".to_string(),
            source.display().to_string(),
            "-o".to_string(),
            output.display().to_string(),
        ];
        let mut dylint_args = vec![rustc.display().to_string()];
        dylint_args.extend(plain_args.clone());
        let dylint_libs = serde_json::to_string(&vec![library]).unwrap();

        let (endpoint, server_handle, shutdown) = start_daemon().await;
        let mut client = zccache::ipc::connect(&endpoint).await.unwrap();
        let session_log = tmp.path().join("session.log");
        let session_id = start_session(&mut client, Some(session_log.clone().into())).await;

        let plain_miss = compile(
            &mut client,
            &session_id,
            &rustc,
            &plain_args,
            tmp.path(),
            "[]",
            "plain",
            false,
        )
        .await;
        assert_eq!(plain_miss.0, 0);
        assert!(!plain_miss.1);
        assert!(!String::from_utf8_lossy(&plain_miss.3).contains("workspace lint verdict"));

        std::fs::remove_file(&output).unwrap();
        let dylint_miss = compile(
            &mut client,
            &session_id,
            &driver,
            &dylint_args,
            tmp.path(),
            &dylint_libs,
            "lint-metadata",
            false,
        )
        .await;
        assert_eq!(dylint_miss.0, 0);
        assert!(
            !dylint_miss.1,
            "a plain artifact must not satisfy a missing Dylint verdict"
        );
        assert!(String::from_utf8_lossy(&dylint_miss.3).contains("workspace lint verdict"));

        std::fs::remove_file(&output).unwrap();
        let dylint_hit = compile(
            &mut client,
            &session_id,
            &driver,
            &dylint_args,
            tmp.path(),
            &dylint_libs,
            "lint-metadata",
            false,
        )
        .await;
        assert!(dylint_hit.1);
        assert_eq!(dylint_hit.3, dylint_miss.3);

        std::fs::remove_file(&output).unwrap();
        let plain_hit = compile(
            &mut client,
            &session_id,
            &rustc,
            &plain_args,
            tmp.path(),
            "[]",
            "plain",
            false,
        )
        .await;
        assert!(plain_hit.1);
        assert!(
            !String::from_utf8_lossy(&plain_hit.3).contains("workspace lint verdict"),
            "plain hits must replay the plain verdict, never Dylint diagnostics"
        );

        let log = std::fs::read_to_string(session_log).unwrap();
        let update_keys = log
            .lines()
            .filter(|line| line.contains("[DIAG] update:"))
            .filter_map(|line| line.split("artifact_key=").nth(1))
            .filter_map(|tail| tail.split_whitespace().next())
            .collect::<Vec<_>>();
        assert_eq!(
            update_keys.len(),
            2,
            "plain and Dylint should each run once"
        );
        assert_eq!(
            update_keys[0], update_keys[1],
            "plain and Dylint misses must publish the same artifact-byte key"
        );

        shutdown.notify_one();
        server_handle.await.unwrap();
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "integration-level: starts a real daemon and rustc"]
async fn nested_dylint_hits_across_sibling_worktrees() {
    let Some(rustc) = zccache::test_support::find_rustc() else {
        eprintln!("skipping test: rustc not found");
        return;
    };

    zccache::test_support::test_timeout(async move {
        let tmp = tempfile::tempdir().unwrap();
        let cache = tmp.path().join("cache");
        let _cache_env = CacheDirEnvGuard::set(&cache);
        let driver = tmp.path().join("dylint-driver");
        write_driver(&driver, "workspace lint diagnostic");

        let roots = [tmp.path().join("checkout-a"), tmp.path().join("checkout-b")];
        for root in &roots {
            std::fs::create_dir_all(root.join("src")).unwrap();
            std::fs::write(
                root.join("src/lib.rs"),
                "pub const LIBS: Option<&str> = option_env!(\"DYLINT_LIBS\");\n\
                 pub const METADATA: Option<&str> = option_env!(\"DYLINT_METADATA\");\n\
                 pub fn checked() -> u32 { 42 }\n",
            )
            .unwrap();
            std::fs::write(root.join("libworkspace_lint.so"), b"same-lint-library").unwrap();
        }
        std::fs::create_dir(roots[0].join(".git")).unwrap();
        std::fs::write(
            roots[1].join(".git"),
            "gitdir: ../.git/worktrees/checkout-b\n",
        )
        .unwrap();

        let (endpoint, server_handle, shutdown) = start_daemon().await;
        let mut client = zccache::ipc::connect(&endpoint).await.unwrap();
        let session_log = tmp.path().join("sibling-session.log");
        let session_id = start_session(&mut client, Some(session_log.clone().into())).await;

        let mut outcomes = Vec::new();
        for root in &roots {
            let deps = root.join("target/dylint/target/nightly/debug/deps");
            let incremental = root.join("target/dylint/target/nightly/debug/incremental");
            std::fs::create_dir_all(&deps).unwrap();
            std::fs::create_dir_all(&incremental).unwrap();
            let args = vec![
                rustc.display().to_string(),
                "--crate-name".to_string(),
                "checked".to_string(),
                "--edition=2021".to_string(),
                "src/lib.rs".to_string(),
                "--error-format=json".to_string(),
                "--json=diagnostic-rendered-ansi,artifacts,future-incompat".to_string(),
                "--crate-type".to_string(),
                "lib".to_string(),
                "--emit=dep-info,metadata".to_string(),
                "-C".to_string(),
                "embed-bitcode=no".to_string(),
                "-C".to_string(),
                "metadata=fixture".to_string(),
                "-C".to_string(),
                "extra-filename=-fixture".to_string(),
                "--out-dir".to_string(),
                deps.display().to_string(),
                "-C".to_string(),
                format!("incremental={}", incremental.display()),
                "-C".to_string(),
                "strip=debuginfo".to_string(),
                "-L".to_string(),
                format!("dependency={}", deps.display()),
            ];
            let dylint_libs =
                serde_json::to_string(&vec![root.join("libworkspace_lint.so")]).unwrap();
            outcomes.push(
                compile(
                    &mut client,
                    &session_id,
                    &driver,
                    &args,
                    root,
                    &dylint_libs,
                    "same-metadata",
                    true,
                )
                .await,
            );
        }

        assert_eq!(outcomes[0].0, 0);
        assert!(!outcomes[0].1);
        assert_eq!(outcomes[1].0, 0);
        assert!(
            outcomes[1].1,
            "identical nested Dylint requests must hit across checkout roots\n{}",
            std::fs::read_to_string(session_log).unwrap_or_default()
        );

        shutdown.notify_one();
        server_handle.await.unwrap();
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
        let _cache_env = CacheDirEnvGuard::set(&cache);

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
        let session_log = tmp.path().join("session.log");
        let session_id = start_session(&mut client, Some(session_log.clone().into())).await;
        let mut env: Vec<(String, String)> = std::env::vars().collect();
        env.retain(|(name, _)| name != "CARGO_PKG_NAME" && name != "RUSTUP_TOOLCHAIN");
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
        if !matches!(
            warm,
            Response::CompileResult {
                exit_code: 0,
                cached: true,
                ..
            }
        ) {
            let log = std::fs::read_to_string(&session_log).unwrap_or_default();
            panic!("expected warm Dylint cdylib hit, got {warm:?}\nsession log:\n{log}");
        }
        assert!(primary.is_file());
        assert!(sidecar.is_file());
        assert_eq!(
            std::fs::read(&primary).unwrap(),
            std::fs::read(&sidecar).unwrap()
        );

        shutdown.notify_one();
        server_handle.await.unwrap();
    })
    .await;
}
