//! #1806: the linker's environment is a link-cache input.
//!
//! `LIBRARY_PATH` / `LIB` / `LINK` decide which libraries a link resolves, so
//! two links that differ only in one of them must never share a cached
//! output; a variable the linker never reads must not split the key.

#![cfg(unix)]

use std::path::Path;

use super::super::*;
use super::CacheDirEnvGuard;

/// A linker whose output is the value of `LIBRARY_PATH` it ran with, so a
/// wrongly shared artifact is visible in the bytes.
fn write_env_echo_linker(dir: &Path) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;

    let tool = dir.join("clang");
    std::fs::write(
        &tool,
        "#!/bin/sh\n\
         out=\n\
         while [ \"$#\" -gt 0 ]; do\n\
         if [ \"$1\" = \"-o\" ]; then shift; out=$1; fi\n\
         shift || true\n\
         done\n\
         printf 'lp=%s\\n' \"$LIBRARY_PATH\" > \"$out\"\n",
    )
    .unwrap();
    let mut perms = std::fs::metadata(&tool).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&tool, perms).unwrap();
    tool
}

async fn link(
    server: &DaemonServer,
    tool: &Path,
    args: &[String],
    cwd: &Path,
    env: &[(&str, &str)],
) -> bool {
    let env: Vec<(String, String)> = env
        .iter()
        .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
        .collect();
    match handle_link_ephemeral(
        &server.state,
        std::process::id(),
        tool,
        args,
        cwd,
        Some(env),
    )
    .await
    {
        Response::LinkResult {
            exit_code, cached, ..
        } => {
            assert_eq!(exit_code, 0);
            cached
        }
        other => panic!("expected LinkResult, got: {other:?}"),
    }
}

#[tokio::test]
async fn linker_env_splits_the_link_key_and_unrelated_env_does_not() {
    if staged_link_lane_enabled() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let tool = write_env_echo_linker(tmp.path());
    let input = tmp.path().join("main.o");
    let output = tmp.path().join("app.exe");
    std::fs::write(&input, b"fake object").unwrap();
    let _cache_dir = CacheDirEnvGuard::set(&tmp.path().join("zccache-cache"));
    let server = DaemonServer::bind(&crate::ipc::unique_test_endpoint()).unwrap();
    let args = vec![
        "-o".to_string(),
        output.to_string_lossy().into_owned(),
        input.to_string_lossy().into_owned(),
    ];
    let cwd = tmp.path();

    assert!(!link(&server, &tool, &args, cwd, &[("LIBRARY_PATH", "/lib-a")]).await);
    assert_eq!(std::fs::read_to_string(&output).unwrap(), "lp=/lib-a\n");

    assert!(
        !link(&server, &tool, &args, cwd, &[("LIBRARY_PATH", "/lib-b")]).await,
        "a different LIBRARY_PATH must not hit"
    );
    assert_eq!(std::fs::read_to_string(&output).unwrap(), "lp=/lib-b\n");

    // Unrelated variables (here also changing) still share the entry.
    assert!(
        link(
            &server,
            &tool,
            &args,
            cwd,
            &[("LIBRARY_PATH", "/lib-b"), ("CARGO_PKG_DESCRIPTION", "x")]
        )
        .await,
        "a variable the linker never reads must not split the key"
    );
    assert_eq!(std::fs::read_to_string(&output).unwrap(), "lp=/lib-b\n");
}
