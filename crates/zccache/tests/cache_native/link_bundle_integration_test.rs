//! Real-tool integration coverage for transactional linker directory and side outputs.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use std::path::Path;
use zccache::daemon::DaemonServer;
use zccache::protocol::{Request, Response};

#[cfg(unix)]
type TestClientConnection = zccache::ipc::IpcConnection;
#[cfg(windows)]
type TestClientConnection = zccache::ipc::IpcClientConnection;

struct EnvGuard {
    cache_dir: Option<std::ffi::OsString>,
    staged: Option<std::ffi::OsString>,
}

impl EnvGuard {
    fn set(cache_dir: &Path) -> Self {
        let guard = Self {
            cache_dir: std::env::var_os("ZCCACHE_CACHE_DIR"),
            staged: std::env::var_os("ZCCACHE_STAGED_ARTIFACTS"),
        };
        std::env::set_var("ZCCACHE_CACHE_DIR", cache_dir);
        std::env::set_var("ZCCACHE_STAGED_ARTIFACTS", "all");
        guard
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        restore_env("ZCCACHE_CACHE_DIR", self.cache_dir.as_ref());
        restore_env("ZCCACHE_STAGED_ARTIFACTS", self.staged.as_ref());
    }
}

fn restore_env(name: &str, value: Option<&std::ffi::OsString>) {
    match value {
        Some(value) => std::env::set_var(name, value),
        None => std::env::remove_var(name),
    }
}

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

async fn link_request(
    client: &mut TestClientConnection,
    tool: &Path,
    args: &[String],
    cwd: &Path,
) -> Response {
    client
        .send(&Request::LinkEphemeral {
            client_pid: std::process::id(),
            tool: tool.to_string_lossy().into_owned().into(),
            args: args.to_vec(),
            cwd: cwd.to_string_lossy().into_owned().into(),
            env: None,
        })
        .await
        .unwrap();
    client.recv().await.unwrap().expect("daemon response")
}

fn assert_link(response: Response, expected_cached: bool) {
    assert!(
        matches!(
            response,
            Response::LinkResult {
                exit_code: 0,
                cached,
                ..
            } if cached == expected_cached
        ),
        "expected successful link with cached={expected_cached}"
    );
}

fn run(command: &mut std::process::Command, description: &str) -> bool {
    let Ok(output) = command.output() else {
        eprintln!("skipping: could not run {description}");
        return false;
    };
    if output.status.success() {
        return true;
    }
    eprintln!(
        "skipping: {description} failed\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    false
}

#[cfg(target_os = "macos")]
#[tokio::test]
#[ignore] // Real clang + dsymutil; run through `test --full`.
async fn real_dsymutil_bundle_miss_delete_hit() {
    use std::os::unix::fs::PermissionsExt;

    let Some(clang) = zccache::test_support::find_on_path("clang") else {
        eprintln!("skipping: clang not found");
        return;
    };
    let Some(dsymutil) = zccache::test_support::find_on_path("dsymutil") else {
        eprintln!("skipping: dsymutil not found");
        return;
    };
    let temp = tempfile::tempdir().unwrap();
    let _env = EnvGuard::set(&temp.path().join("cache"));
    let source = temp.path().join("main.c");
    let executable = temp.path().join("app");
    let bundle = temp.path().join("app.dSYM");
    std::fs::write(&source, "int main(void) { return 0; }\n").unwrap();
    if !run(
        std::process::Command::new(clang)
            .args(["-g", "-O0"])
            .arg(&source)
            .arg("-o")
            .arg(&executable),
        "clang debug build",
    ) {
        return;
    }
    let args = vec![
        executable.to_string_lossy().into_owned(),
        "-o".to_string(),
        bundle.to_string_lossy().into_owned(),
    ];
    let (endpoint, handle, shutdown) = start_daemon().await;
    let mut client = zccache::ipc::connect(&endpoint).await.unwrap();
    assert_link(
        link_request(&mut client, &dsymutil, &args, temp.path()).await,
        false,
    );
    let dwarf = bundle.join("Contents/Resources/DWARF/app");
    let expected = std::fs::read(&dwarf).unwrap();
    let metadata = std::fs::metadata(&dwarf).unwrap();
    let mode = metadata.permissions().mode();
    let mtime = metadata.modified().unwrap();
    std::fs::remove_dir_all(&bundle).unwrap();
    assert_link(
        link_request(&mut client, &dsymutil, &args, temp.path()).await,
        true,
    );
    let metadata = std::fs::metadata(&dwarf).unwrap();
    assert_eq!(std::fs::read(dwarf).unwrap(), expected);
    assert_eq!(metadata.permissions().mode(), mode);
    assert_eq!(metadata.modified().unwrap(), mtime);
    shutdown.notify_one();
    handle.await.unwrap();
}

#[cfg(target_os = "linux")]
#[tokio::test]
#[ignore] // Real clang + LLD; run through `test --full`.
async fn real_lld_semantic_outputs_miss_delete_hit() {
    let Some(clang) = zccache::test_support::find_on_path("clang") else {
        eprintln!("skipping: clang not found");
        return;
    };
    if zccache::test_support::find_on_path("ld.lld").is_none() {
        eprintln!("skipping: ld.lld not found");
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let _env = EnvGuard::set(&temp.path().join("cache"));
    let source = temp.path().join("main.c");
    let object = temp.path().join("main.o");
    let executable = temp.path().join("app");
    let map = temp.path().join("app.map");
    let dependency = temp.path().join("app.d");
    std::fs::write(&source, "int main(void) { return 0; }\n").unwrap();
    if !run(
        std::process::Command::new(&clang)
            .args(["-c", "-g"])
            .arg(&source)
            .arg("-o")
            .arg(&object),
        "clang object build",
    ) {
        return;
    }
    let args = vec![
        "-fuse-ld=lld".to_string(),
        "-o".to_string(),
        executable.to_string_lossy().into_owned(),
        object.to_string_lossy().into_owned(),
        format!(
            "-Wl,-Map,{},--dependency-file={}",
            map.display(),
            dependency.display()
        ),
    ];
    let (endpoint, handle, shutdown) = start_daemon().await;
    let mut client = zccache::ipc::connect(&endpoint).await.unwrap();
    assert_link(
        link_request(&mut client, &clang, &args, temp.path()).await,
        false,
    );
    let expected = [
        std::fs::read(&executable).unwrap(),
        std::fs::read(&map).unwrap(),
        std::fs::read(&dependency).unwrap(),
    ];
    for path in [&executable, &map, &dependency] {
        std::fs::remove_file(path).unwrap();
    }
    assert_link(
        link_request(&mut client, &clang, &args, temp.path()).await,
        true,
    );
    assert_eq!(std::fs::read(executable).unwrap(), expected[0]);
    assert_eq!(std::fs::read(map).unwrap(), expected[1]);
    assert_eq!(std::fs::read(dependency).unwrap(), expected[2]);
    shutdown.notify_one();
    handle.await.unwrap();
}

#[cfg(windows)]
#[tokio::test]
#[ignore] // Real cl.exe + link.exe from a Developer Command Prompt.
async fn real_msvc_ltcg_output_miss_delete_hit() {
    let Some(cl) = zccache::test_support::find_on_path("cl.exe") else {
        eprintln!("skipping: cl.exe not found");
        return;
    };
    let Some(link) = zccache::test_support::find_on_path("link.exe") else {
        eprintln!("skipping: link.exe not found");
        return;
    };
    let temp = tempfile::tempdir().unwrap();
    let _env = EnvGuard::set(&temp.path().join("cache"));
    let source = temp.path().join("main.c");
    let object = temp.path().join("main.obj");
    let executable = temp.path().join("app.exe");
    let iobj = temp.path().join("app.iobj");
    std::fs::write(&source, "int main(void) { return 0; }\n").unwrap();
    if !run(
        std::process::Command::new(cl)
            .arg("/nologo")
            .arg("/c")
            .arg("/GL")
            .arg(format!("/Fo:{}", object.display()))
            .arg(&source),
        "MSVC LTCG object build",
    ) {
        return;
    }
    let args = vec![
        "/NOLOGO".to_string(),
        format!("/OUT:{}", executable.display()),
        "/LTCG:INCREMENTAL".to_string(),
        format!("/LTCGOUT:{}", iobj.display()),
        object.to_string_lossy().into_owned(),
    ];
    let (endpoint, handle, shutdown) = start_daemon().await;
    let mut client = zccache::ipc::connect(&endpoint).await.unwrap();
    assert_link(
        link_request(&mut client, &link, &args, temp.path()).await,
        false,
    );
    let expected_exe = std::fs::read(&executable).unwrap();
    let expected_iobj = std::fs::read(&iobj).unwrap();
    std::fs::remove_file(&executable).unwrap();
    std::fs::remove_file(&iobj).unwrap();
    assert_link(
        link_request(&mut client, &link, &args, temp.path()).await,
        true,
    );
    assert_eq!(std::fs::read(executable).unwrap(), expected_exe);
    assert_eq!(std::fs::read(iobj).unwrap(), expected_iobj);
    shutdown.notify_one();
    handle.await.unwrap();
}
