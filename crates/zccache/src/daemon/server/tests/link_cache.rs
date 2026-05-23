//! Tests for the ephemeral link-cache path: after a cache hit on the
//! primary linker output, sibling side-effects (PDB, wasm map, ...) must
//! be restored from the cache too.

use std::path::Path;

use super::super::*;
use super::CacheDirEnvGuard;

#[cfg(unix)]
fn write_fake_linker(dir: &Path) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;

    let tool = dir.join("clang");
    std::fs::write(
        &tool,
        r#"#!/bin/sh
out=
while [ "$#" -gt 0 ]; do
if [ "$1" = "-o" ]; then
    shift
    out=$1
fi
shift || true
done
if [ -z "$out" ]; then
exit 2
fi
out_dir=$(dirname "$out")
printf 'binary\n' > "$out"
printf 'debug\n' > "$out_dir/app.pdb"
printf 'map\n' > "$out_dir/app.wasm.map"
"#,
    )
    .unwrap();
    let mut perms = std::fs::metadata(&tool).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&tool, perms).unwrap();
    tool
}

#[cfg(windows)]
fn write_fake_linker(dir: &Path) -> std::path::PathBuf {
    let tool = dir.join("clang.cmd");
    std::fs::write(
        &tool,
        r#"@echo off
set "OUT=%~2"
if "%OUT%"=="" exit /b 2
> "%OUT%" echo binary
for %%I in ("%OUT%") do set "OUTDIR=%%~dpI"
> "%OUTDIR%app.pdb" echo debug
> "%OUTDIR%app.wasm.map" echo map
exit /b 0
"#,
    )
    .unwrap();
    tool
}

#[tokio::test]
async fn link_cache_hit_restores_sibling_side_effects() {
    let tmp = tempfile::tempdir().unwrap();
    let fake_linker = write_fake_linker(tmp.path());
    let input = tmp.path().join("main.o");
    let output = tmp.path().join("app.exe");
    let pdb = tmp.path().join("app.pdb");
    let wasm_map = tmp.path().join("app.wasm.map");
    std::fs::write(&input, b"fake object").unwrap();

    let _cache_dir = CacheDirEnvGuard::set(&tmp.path().join("zccache-cache"));
    let server = DaemonServer::bind(&zccache::ipc::unique_test_endpoint()).unwrap();
    let args = vec![
        "-o".to_string(),
        output.to_string_lossy().into_owned(),
        input.to_string_lossy().into_owned(),
    ];

    let first = handle_link_ephemeral(
        &server.state,
        std::process::id(),
        &fake_linker,
        &args,
        tmp.path(),
        None,
    )
    .await;
    match first {
        Response::LinkResult {
            exit_code, cached, ..
        } => {
            assert_eq!(exit_code, 0);
            assert!(!cached, "first link should populate the cache");
        }
        other => panic!("expected LinkResult, got: {other:?}"),
    }
    assert!(
        output.exists(),
        "fresh link should create the primary output"
    );
    assert!(pdb.exists(), "fresh link should create a PDB sidecar");
    assert!(
        wasm_map.exists(),
        "fresh link should create a wasm map sidecar"
    );

    std::fs::remove_file(&pdb).unwrap();
    std::fs::remove_file(&wasm_map).unwrap();

    let second = handle_link_ephemeral(
        &server.state,
        std::process::id(),
        &fake_linker,
        &args,
        tmp.path(),
        None,
    )
    .await;
    match second {
        Response::LinkResult {
            exit_code, cached, ..
        } => {
            assert_eq!(exit_code, 0);
            assert!(cached, "second link should be served from cache");
        }
        other => panic!("expected LinkResult, got: {other:?}"),
    }

    assert!(output.exists(), "cache hit should keep the primary output");
    assert!(pdb.exists(), "cache hit should restore the PDB sidecar");
    assert!(
        wasm_map.exists(),
        "cache hit should restore the wasm map sidecar"
    );
}
