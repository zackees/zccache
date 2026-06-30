//! Integration tests for static library (.a) caching.
//!
//! Tests the full flow: compile .o files → `ar rcsD` → cache hit/miss.

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

/// Create fake object files (ar doesn't validate content).
fn write_fake_objects(dir: &std::path::Path, names: &[&str]) {
    for (i, name) in names.iter().enumerate() {
        let content = format!("fake object file {} content {}", name, i);
        std::fs::write(dir.join(name), content).unwrap();
    }
}

fn run_test_command(cmd: &mut std::process::Command, description: &str) -> Result<(), String> {
    let output = cmd
        .output()
        .map_err(|e| format!("failed to run {description}: {e}"))?;
    if output.status.success() {
        return Ok(());
    }

    Err(format!(
        "{description} failed with status {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    ))
}

fn setup_equivalent_c_root(
    compiler: &std::path::Path,
    archiver: &std::path::Path,
    root: &std::path::Path,
) -> Result<(), String> {
    std::fs::create_dir_all(root.join(".git")).unwrap();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::create_dir_all(root.join("build")).unwrap();
    std::fs::create_dir_all(root.join("lib")).unwrap();
    std::fs::write(
        root.join("src/main.c"),
        "int dep(void);\nint main(void) { return dep(); }\n",
    )
    .unwrap();
    std::fs::write(root.join("src/dep.c"), "int dep(void) { return 0; }\n").unwrap();

    let mut cmd = std::process::Command::new(compiler);
    cmd.args(["-g0", "-c", "src/main.c", "-o", "build/main.o"])
        .current_dir(root);
    run_test_command(&mut cmd, "compile test object")?;

    let mut cmd = std::process::Command::new(compiler);
    cmd.args(["-g0", "-c", "src/dep.c", "-o", "build/dep.o"])
        .current_dir(root);
    run_test_command(&mut cmd, "compile test library object")?;

    let mut cmd = std::process::Command::new(archiver);
    cmd.args(["rcsD", "lib/libdep.a", "build/dep.o"])
        .current_dir(root);
    if run_test_command(&mut cmd, "archive test library").is_ok() {
        return Ok(());
    }

    let mut cmd = std::process::Command::new(archiver);
    cmd.args(["rcs", "lib/libdep.a", "build/dep.o"])
        .current_dir(root);
    run_test_command(&mut cmd, "archive test library")
}

fn linked_binary_name(stem: &str) -> String {
    if cfg!(windows) {
        format!("{stem}.exe")
    } else {
        stem.to_string()
    }
}

fn compiler_driver_link_args(root: &std::path::Path, output: &std::path::Path) -> Vec<String> {
    vec![
        "-o".to_string(),
        output.to_string_lossy().into_owned(),
        "build/main.o".to_string(),
        format!("-L{}", root.join("lib").to_string_lossy()),
        "-ldep".to_string(),
    ]
}

fn compiler_driver_link_is_feasible(
    compiler: &std::path::Path,
    archiver: &std::path::Path,
) -> Result<(), String> {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("repo");
    setup_equivalent_c_root(compiler, archiver, &root)?;

    let output = root.join("build").join(linked_binary_name("probe"));
    let args = compiler_driver_link_args(&root, &output);
    let mut cmd = std::process::Command::new(compiler);
    cmd.args(&args).current_dir(&root);
    run_test_command(&mut cmd, "probe compiler-driver link")?;
    std::fs::remove_file(output).ok();
    Ok(())
}

fn client_env_with_path_remap_auto() -> Vec<(String, String)> {
    let mut env: Vec<(String, String)> = std::env::vars_os()
        .filter_map(|(key, value)| {
            let key = key.into_string().ok()?;
            let value = value.into_string().ok()?;
            let zccache_root_var = key.eq_ignore_ascii_case("ZCCACHE_WORKTREE_ROOT");
            let zccache_remap_var = key.eq_ignore_ascii_case("ZCCACHE_PATH_REMAP");
            (!zccache_root_var && !zccache_remap_var).then_some((key, value))
        })
        .collect();
    env.push(("ZCCACHE_PATH_REMAP".to_string(), "auto".to_string()));
    env
}

#[tokio::test]
#[ignore] // Integration test — starts a real daemon. Run with `test --full`.
async fn test_ar_cache_miss_then_hit() {
    let ar_path = match zccache::test_support::find_on_path("ar") {
        Some(p) => p,
        None => {
            eprintln!("skipping test: ar not found on PATH");
            return;
        }
    };

    let tmp = tempfile::tempdir().unwrap();
    write_fake_objects(tmp.path(), &["a.o", "b.o"]);

    let output_lib = tmp.path().join("libfoo.a");

    let (endpoint, server_handle, shutdown) = start_daemon().await;
    let mut client = zccache::ipc::connect(&endpoint).await.unwrap();

    // First link — should be a cache miss
    client
        .send(&Request::LinkEphemeral {
            client_pid: std::process::id(),

            tool: ar_path.to_string_lossy().into_owned().into(),
            args: vec![
                "rcsD".to_string(),
                output_lib.to_string_lossy().into_owned(),
                tmp.path().join("a.o").to_string_lossy().into_owned(),
                tmp.path().join("b.o").to_string_lossy().into_owned(),
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
            assert_eq!(exit_code, 0, "ar should succeed");
            assert!(!cached, "first link should be a cache miss");
            assert!(warning.is_none(), "D flag present — no warning expected");
        }
        other => panic!("expected LinkResult, got: {other:?}"),
    }

    assert!(
        output_lib.exists(),
        "libfoo.a should exist after first link"
    );
    let first_size = std::fs::metadata(&output_lib).unwrap().len();
    assert!(first_size > 0, "archive should not be empty");
    let first_contents = std::fs::read(&output_lib).unwrap();

    // Delete the output so we can verify cache restores it
    std::fs::remove_file(&output_lib).unwrap();
    assert!(!output_lib.exists(), "libfoo.a should be deleted");

    // Second link — should be a cache hit
    client
        .send(&Request::LinkEphemeral {
            client_pid: std::process::id(),

            tool: ar_path.to_string_lossy().into_owned().into(),
            args: vec![
                "rcsD".to_string(),
                output_lib.to_string_lossy().into_owned(),
                tmp.path().join("a.o").to_string_lossy().into_owned(),
                tmp.path().join("b.o").to_string_lossy().into_owned(),
            ],
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
            assert_eq!(exit_code, 0, "cached ar should succeed");
            assert!(cached, "second link should be a cache hit");
        }
        other => panic!("expected LinkResult, got: {other:?}"),
    }

    // Verify the cached output was restored
    assert!(
        output_lib.exists(),
        "cache hit should restore the archive file"
    );
    let second_contents = std::fs::read(&output_lib).unwrap();
    assert_eq!(
        first_contents, second_contents,
        "cached archive should be byte-identical"
    );

    shutdown.notify_one();
    server_handle.await.unwrap();
}

#[tokio::test]
#[ignore] // Integration test — starts a real daemon + compiler driver. Run with `test --full`.
async fn test_link_path_remap_auto_hits_across_sibling_git_roots() {
    let archiver = match zccache::test_support::find_on_path("ar")
        .or_else(|| zccache::test_support::find_on_path("llvm-ar"))
    {
        Some(path) => path,
        None => {
            eprintln!("skipping test: neither ar nor llvm-ar found on PATH");
            return;
        }
    };
    let mut skipped = Vec::new();
    let mut selected_compiler = None;
    for name in ["clang", "gcc"] {
        let Some(path) = zccache::test_support::find_on_path(name) else {
            skipped.push(format!("{name}: not found on PATH"));
            continue;
        };
        match compiler_driver_link_is_feasible(&path, &archiver) {
            Ok(()) => {
                selected_compiler = Some(path);
                break;
            }
            Err(e) => skipped.push(format!("{name}: {e}")),
        }
    }

    let compiler_path = match selected_compiler {
        Some(path) => path,
        None => {
            eprintln!(
                "skipping test: no usable clang/gcc compiler-driver link found\n{}",
                skipped.join("\n")
            );
            return;
        }
    };

    let tmp = tempfile::tempdir().unwrap();
    let root_a = tmp.path().join("workspace-a");
    let root_b = tmp.path().join("workspace-b");
    setup_equivalent_c_root(&compiler_path, &archiver, &root_a).unwrap();
    setup_equivalent_c_root(&compiler_path, &archiver, &root_b).unwrap();

    let object_a = std::fs::read(root_a.join("build/main.o")).unwrap();
    let object_b = std::fs::read(root_b.join("build/main.o")).unwrap();
    if object_a != object_b {
        eprintln!("skipping test: compiler produced root-specific object bytes");
        return;
    }
    let lib_a = std::fs::read(root_a.join("lib/libdep.a")).unwrap();
    let lib_b = std::fs::read(root_b.join("lib/libdep.a")).unwrap();
    if lib_a != lib_b {
        eprintln!("skipping test: archiver produced root-specific library bytes");
        return;
    }

    let output_a = root_a.join("build").join(linked_binary_name("app"));
    let output_b = root_b.join("build").join(linked_binary_name("app"));
    assert_ne!(
        output_a, output_b,
        "test must use distinct physical output paths"
    );

    let (endpoint, server_handle, shutdown) = start_daemon().await;
    let mut client = zccache::ipc::connect(&endpoint).await.unwrap();

    // Clear persisted artifacts to ensure test isolation from prior runs.
    client.send(&Request::Clear).await.unwrap();
    let _: Option<Response> = client.recv().await.unwrap();

    let remap_env = client_env_with_path_remap_auto();

    // First root: populate the link cache. The absolute -L path is under root A.
    client
        .send(&Request::LinkEphemeral {
            client_pid: std::process::id(),
            tool: compiler_path.to_string_lossy().into_owned().into(),
            args: compiler_driver_link_args(&root_a, &output_a),
            cwd: root_a.to_string_lossy().into_owned().into(),
            env: Some(remap_env.clone()),
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
            assert_eq!(exit_code, 0, "first compiler-driver link should succeed");
            assert!(!cached, "first link in root A should be a cache miss");
            assert!(
                warning.is_none(),
                "deterministic compiler-driver link should not warn"
            );
        }
        other => panic!("expected LinkResult, got: {other:?}"),
    }

    assert!(output_a.exists(), "root A output should exist after miss");
    assert!(
        !output_b.exists(),
        "root B output should not exist before its link"
    );
    let first_contents = std::fs::read(&output_a).unwrap();

    // Second root: same object bytes and root-equivalent -L path should hit.
    client
        .send(&Request::LinkEphemeral {
            client_pid: std::process::id(),
            tool: compiler_path.to_string_lossy().into_owned().into(),
            args: compiler_driver_link_args(&root_b, &output_b),
            cwd: root_b.to_string_lossy().into_owned().into(),
            env: Some(remap_env),
        })
        .await
        .unwrap();

    let resp = client.recv().await.unwrap();
    match resp {
        Some(Response::LinkResult {
            exit_code, cached, ..
        }) => {
            assert_eq!(exit_code, 0, "cached compiler-driver link should succeed");
            assert!(
                cached,
                "ZCCACHE_PATH_REMAP=auto should make root-equivalent -L flags hit"
            );
        }
        other => panic!("expected LinkResult, got: {other:?}"),
    }

    assert!(
        output_a.exists(),
        "cache hit in root B should preserve root A output"
    );
    assert!(
        output_b.exists(),
        "cache hit should restore output at root B's physical path"
    );
    assert_eq!(
        first_contents,
        std::fs::read(&output_b).unwrap(),
        "root B hit should restore the cached root A link output"
    );

    shutdown.notify_one();
    server_handle.await.unwrap();
}

#[tokio::test]
#[ignore] // Integration test — starts a real daemon. Run with `test --full`.
async fn test_ar_cache_invalidated_on_input_change() {
    let ar_path = match zccache::test_support::find_on_path("ar") {
        Some(p) => p,
        None => {
            eprintln!("skipping test: ar not found on PATH");
            return;
        }
    };

    let tmp = tempfile::tempdir().unwrap();
    write_fake_objects(tmp.path(), &["x.o", "y.o"]);

    let output_lib = tmp.path().join("libbar.a");

    let (endpoint, server_handle, shutdown) = start_daemon().await;
    let mut client = zccache::ipc::connect(&endpoint).await.unwrap();

    let make_args = |lib: &std::path::Path, dir: &std::path::Path| -> Vec<String> {
        vec![
            "rcsD".to_string(),
            lib.to_string_lossy().into_owned(),
            dir.join("x.o").to_string_lossy().into_owned(),
            dir.join("y.o").to_string_lossy().into_owned(),
        ]
    };

    // First link — cache miss
    client
        .send(&Request::LinkEphemeral {
            client_pid: std::process::id(),

            tool: ar_path.to_string_lossy().into_owned().into(),
            args: make_args(&output_lib, tmp.path()),
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

    let original_archive = std::fs::read(&output_lib).unwrap();

    // Modify one input file
    std::fs::write(tmp.path().join("x.o"), "MODIFIED content for x.o").unwrap();

    // Delete output so we can verify it gets recreated
    std::fs::remove_file(&output_lib).unwrap();

    // Third link — should be a cache miss (input changed)
    client
        .send(&Request::LinkEphemeral {
            client_pid: std::process::id(),

            tool: ar_path.to_string_lossy().into_owned().into(),
            args: make_args(&output_lib, tmp.path()),
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

    // The new archive should differ from the original
    let new_archive = std::fs::read(&output_lib).unwrap();
    assert_ne!(
        original_archive, new_archive,
        "archive should differ after input change"
    );

    shutdown.notify_one();
    server_handle.await.unwrap();
}

#[tokio::test]
#[ignore] // Integration test — starts a real daemon. Run with `test --full`.
async fn test_ar_non_deterministic_warning() {
    let ar_path = match zccache::test_support::find_on_path("ar") {
        Some(p) => p,
        None => {
            eprintln!("skipping test: ar not found on PATH");
            return;
        }
    };

    let tmp = tempfile::tempdir().unwrap();
    write_fake_objects(tmp.path(), &["a.o"]);

    let output_lib = tmp.path().join("libwarn.a");

    let (endpoint, server_handle, shutdown) = start_daemon().await;
    let mut client = zccache::ipc::connect(&endpoint).await.unwrap();

    // ar rcs (no D flag) — should warn about non-determinism
    client
        .send(&Request::LinkEphemeral {
            client_pid: std::process::id(),

            tool: ar_path.to_string_lossy().into_owned().into(),
            args: vec![
                "rcs".to_string(),
                output_lib.to_string_lossy().into_owned(),
                tmp.path().join("a.o").to_string_lossy().into_owned(),
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
            assert_eq!(exit_code, 0, "ar should succeed even without D flag");
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
        other => panic!("expected LinkResult, got: {other:?}"),
    }

    // The archive should still be produced
    assert!(output_lib.exists(), "ar should produce output");

    shutdown.notify_one();
    server_handle.await.unwrap();
}

#[tokio::test]
#[ignore] // Integration test — starts a real daemon. Run with `test --full`.
async fn test_ar_non_cacheable_passthrough() {
    let ar_path = match zccache::test_support::find_on_path("ar") {
        Some(p) => p,
        None => {
            eprintln!("skipping test: ar not found on PATH");
            return;
        }
    };

    let tmp = tempfile::tempdir().unwrap();
    write_fake_objects(tmp.path(), &["a.o"]);

    // First, create an archive we can list
    let lib_path = tmp.path().join("liblist.a");
    let status = std::process::Command::new(&ar_path)
        .args([
            "rcsD",
            &lib_path.to_string_lossy(),
            &tmp.path().join("a.o").to_string_lossy(),
        ])
        .status()
        .unwrap();
    assert!(status.success(), "ar rcsD should succeed");

    let (endpoint, server_handle, shutdown) = start_daemon().await;
    let mut client = zccache::ipc::connect(&endpoint).await.unwrap();

    // ar t (list operation) — non-cacheable, should pass through
    client
        .send(&Request::LinkEphemeral {
            client_pid: std::process::id(),

            tool: ar_path.to_string_lossy().into_owned().into(),
            args: vec!["t".to_string(), lib_path.to_string_lossy().into_owned()],
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
            stdout,
            ..
        }) => {
            assert_eq!(exit_code, 0, "ar t should succeed");
            assert!(!cached, "non-cacheable operation should not be cached");
            let output = String::from_utf8_lossy(&stdout);
            assert!(
                output.contains("a.o"),
                "ar t should list archive members: {output}"
            );
        }
        other => panic!("expected LinkResult, got: {other:?}"),
    }

    shutdown.notify_one();
    server_handle.await.unwrap();
}

#[tokio::test]
#[ignore] // Integration test — starts a real daemon. Run with `test --full`.
async fn test_link_stats_in_status() {
    let ar_path = match zccache::test_support::find_on_path("ar") {
        Some(p) => p,
        None => {
            eprintln!("skipping test: ar not found on PATH");
            return;
        }
    };

    let tmp = tempfile::tempdir().unwrap();
    write_fake_objects(tmp.path(), &["s.o"]);

    let output_lib = tmp.path().join("libstats.a");

    let (endpoint, server_handle, shutdown) = start_daemon().await;
    let mut client = zccache::ipc::connect(&endpoint).await.unwrap();

    // One deterministic link — cache miss
    client
        .send(&Request::LinkEphemeral {
            client_pid: std::process::id(),

            tool: ar_path.to_string_lossy().into_owned().into(),
            args: vec![
                "rcsD".to_string(),
                output_lib.to_string_lossy().into_owned(),
                tmp.path().join("s.o").to_string_lossy().into_owned(),
            ],
            cwd: tmp.path().to_string_lossy().into_owned().into(),
            env: None,
        })
        .await
        .unwrap();
    let _: Option<Response> = client.recv().await.unwrap(); // consume response

    // Check status — should show link stats
    client.send(&Request::Status).await.unwrap();
    let resp = client.recv().await.unwrap();
    match resp {
        Some(Response::Status(s)) => {
            assert!(
                s.total_links >= 1,
                "status should show at least 1 link: total_links={}",
                s.total_links
            );
            assert!(
                s.link_misses >= 1,
                "status should show at least 1 link miss: link_misses={}",
                s.link_misses
            );
        }
        other => panic!("expected Status response, got: {other:?}"),
    }

    shutdown.notify_one();
    server_handle.await.unwrap();
}
