//! Tests for the ephemeral link-cache path: after a cache hit on the
//! primary linker output, sibling side-effects (PDB, wasm map, ...) must
//! be restored from the cache too.

use std::path::Path;

use super::super::*;
use super::{bind_isolated_server, CacheDirEnvGuard};

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

#[cfg(unix)]
fn write_fake_primary_linker(dir: &Path) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;

    let tool = dir.join("gcc");
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
printf 'binary\n' > "$out"
"#,
    )
    .unwrap();
    let mut permissions = std::fs::metadata(&tool).unwrap().permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&tool, permissions).unwrap();
    tool
}

/// A linker that leaves a marker only when two linker processes run in the
/// same output directory at once. Its active marker is a directory, which the
/// side-effect scanner intentionally ignores; the collision marker is a file.
#[cfg(unix)]
fn write_overlap_detecting_linker(dir: &Path) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;

    let tool = dir.join("overlap-clang");
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
if ! mkdir "$out_dir/.zccache-link-active" 2>/dev/null; then
    printf 'overlap\n' > "$out_dir/parallel-link-collision"
fi
sleep 0.2
printf 'binary-%s\n' "$(basename "$out")" > "$out"
printf 'sidecar-%s\n' "$(basename "$out")" > "$out.sidecar"
rmdir "$out_dir/.zccache-link-active" 2>/dev/null || true
"#,
    )
    .unwrap();
    let mut permissions = std::fs::metadata(&tool).unwrap().permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&tool, permissions).unwrap();
    tool
}

#[cfg(windows)]
fn write_overlap_detecting_linker(dir: &Path) -> std::path::PathBuf {
    let tool = dir.join("overlap-clang.cmd");
    std::fs::write(
        &tool,
        r#"@echo off
set "OUT=%~2"
if "%OUT%"=="" exit /b 2
for %%I in ("%OUT%") do set "OUTDIR=%%~dpI"
mkdir "%OUTDIR%\.zccache-link-active" >nul 2>nul
if errorlevel 1 > "%OUTDIR%parallel-link-collision" echo overlap
ping -n 2 127.0.0.1 >nul
> "%OUT%" echo binary-%~nx2
> "%OUT%.sidecar" echo sidecar-%~nx2
rmdir "%OUTDIR%\.zccache-link-active" >nul 2>nul
exit /b 0
"#,
    )
    .unwrap();
    tool
}

#[cfg(unix)]
fn write_fake_dsymutil(dir: &Path) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;

    let tool = dir.join("dsymutil");
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
mkdir -p "$out/Contents/Resources/DWARF"
printf 'debug-binary\n' > "$out/Contents/Resources/DWARF/app"
printf 'plist\n' > "$out/Contents/Info.plist"
chmod 755 "$out/Contents/Resources/DWARF/app"
"#,
    )
    .unwrap();
    let mut permissions = std::fs::metadata(&tool).unwrap().permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&tool, permissions).unwrap();
    tool
}

#[cfg(windows)]
fn write_fake_dsymutil(dir: &Path) -> std::path::PathBuf {
    let tool = dir.join("dsymutil.cmd");
    std::fs::write(
        &tool,
        r#"@echo off
set "OUT="
:args
if "%~1"=="" goto run
if "%~1"=="-o" (
  set "OUT=%~2"
  shift
)
shift
goto args
:run
if "%OUT%"=="" exit /b 2
mkdir "%OUT%\Contents\Resources\DWARF" >nul 2>nul
> "%OUT%\Contents\Resources\DWARF\app" echo debug-binary
> "%OUT%\Contents\Info.plist" echo plist
exit /b 0
"#,
    )
    .unwrap();
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

#[cfg(windows)]
fn write_fake_primary_linker(dir: &Path) -> std::path::PathBuf {
    let tool = dir.join("gcc.cmd");
    std::fs::write(
        &tool,
        r#"@echo off
set "OUT=%~2"
if "%OUT%"=="" exit /b 2
> "%OUT%" echo binary
exit /b 0
"#,
    )
    .unwrap();
    tool
}

#[tokio::test]
async fn link_cache_hit_restores_sibling_side_effects() {
    if staged_link_lane_enabled() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let fake_linker = write_fake_linker(tmp.path());
    let input = tmp.path().join("main.o");
    let output = tmp.path().join("app.exe");
    let pdb = tmp.path().join("app.pdb");
    let wasm_map = tmp.path().join("app.wasm.map");
    std::fs::write(&input, b"fake object").unwrap();

    let _cache_dir = CacheDirEnvGuard::set(&tmp.path().join("zccache-cache"));
    let server = DaemonServer::bind(&crate::ipc::unique_test_endpoint()).unwrap();
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

/// #912: two cold links in the same output directory must not overlap their
/// snapshot/link/rescan windows. Otherwise each can claim the other's newly
/// created sidecar as part of its own cached artifact.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn parallel_same_directory_links_are_isolated() {
    if staged_link_lane_enabled() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let fake_linker = write_overlap_detecting_linker(tmp.path());
    let input_a = tmp.path().join("a.o");
    let input_b = tmp.path().join("b.o");
    let output_a = tmp.path().join("a.exe");
    let output_b = tmp.path().join("b.exe");
    let sidecar_a = output_a.with_extension("exe.sidecar");
    let sidecar_b = output_b.with_extension("exe.sidecar");
    std::fs::write(&input_a, b"input-a").unwrap();
    std::fs::write(&input_b, b"input-b").unwrap();

    let _cache_dir = CacheDirEnvGuard::set(&tmp.path().join("zccache-cache"));
    let server = DaemonServer::bind(&crate::ipc::unique_test_endpoint()).unwrap();
    let args_a = vec![
        "-o".to_string(),
        output_a.to_string_lossy().into_owned(),
        input_a.to_string_lossy().into_owned(),
    ];
    let args_b = vec![
        "-o".to_string(),
        output_b.to_string_lossy().into_owned(),
        input_b.to_string_lossy().into_owned(),
    ];

    let (first_a, first_b) = tokio::join!(
        handle_link_ephemeral(
            &server.state,
            std::process::id(),
            &fake_linker,
            &args_a,
            tmp.path(),
            None,
        ),
        handle_link_ephemeral(
            &server.state,
            std::process::id(),
            &fake_linker,
            &args_b,
            tmp.path(),
            None,
        )
    );
    assert!(matches!(
        first_a,
        Response::LinkResult {
            exit_code: 0,
            cached: false,
            ..
        }
    ));
    assert!(matches!(
        first_b,
        Response::LinkResult {
            exit_code: 0,
            cached: false,
            ..
        }
    ));
    assert!(
        !tmp.path().join("parallel-link-collision").exists(),
        "links sharing an output directory must not overlap"
    );

    std::fs::remove_file(&output_a).unwrap();
    std::fs::remove_file(&sidecar_a).unwrap();
    std::fs::remove_file(&sidecar_b).unwrap();
    let hit_a = handle_link_ephemeral(
        &server.state,
        std::process::id(),
        &fake_linker,
        &args_a,
        tmp.path(),
        None,
    )
    .await;
    assert!(matches!(
        hit_a,
        Response::LinkResult {
            exit_code: 0,
            cached: true,
            ..
        }
    ));
    assert!(std::fs::read(sidecar_a)
        .unwrap()
        .starts_with(b"sidecar-a.exe"));
    assert!(
        !sidecar_b.exists(),
        "a cache hit must not restore b's sidecar"
    );
}

/// #912: the side-effect lock is keyed by output directory, not globally.
#[tokio::test]
async fn side_effect_lock_allows_different_output_directories() {
    let tmp = tempfile::tempdir().unwrap();
    let server = bind_isolated_server(tmp.path());
    let lock_a = server.state.link_output_lock(tmp.path().join("a").into());
    let same_lock_a = server.state.link_output_lock(tmp.path().join("a").into());
    let lock_b = server.state.link_output_lock(tmp.path().join("b").into());
    assert!(std::sync::Arc::ptr_eq(&lock_a, &same_lock_a));
    assert!(!std::sync::Arc::ptr_eq(&lock_a, &lock_b));

    // #1254: these were `tokio::time::timeout(50ms, lock_owned())`, which made
    // a *logical* claim ("b is not blocked by a") depend on wall-clock
    // scheduling. Acquiring an uncontended mutex is instant, but under the
    // parallel test load of this crate the task can simply fail to be
    // scheduled inside 50 ms, and the test then reports contention that never
    // happened. It failed on arm in CI and on Windows locally within an hour.
    //
    // `try_lock_owned` answers the same question with no deadline at all:
    // uncontended succeeds immediately, contended fails immediately. There is
    // no duration left for load to distort.
    let _guard_a = lock_a
        .try_lock_owned()
        .expect("an unheld lock must be acquirable");
    let _guard_b = lock_b
        .try_lock_owned()
        .expect("a lock held for one output directory must not block another");
    assert!(
        same_lock_a.try_lock_owned().is_err(),
        "the same output directory must remain exclusive"
    );
}

/// Issue #563: the input-hash loop is parallelized via rayon. `par_iter`
/// preserves iteration order, so the cache key bytes are identical to
/// the serial computation. This test asserts:
///
/// 1. With 12 unique input files, the first link populates the cache
///    and the second link with the SAME input order hits.
/// 2. With the same 12 inputs in REVERSED order, the second link
///    MISSES — order is part of the cache key, so a reordering must
///    produce a different key.
///
/// If rayon's collect ever stopped preserving order (or my change
/// inadvertently moved to a Set / unordered structure), case (2) would
/// degrade to a hit and this test would fail.
#[tokio::test]
async fn link_cache_key_preserves_input_order_under_parallel_hashing() {
    let tmp = tempfile::tempdir().unwrap();
    let fake_linker = write_fake_primary_linker(tmp.path());
    let output = tmp.path().join("app.exe");

    // 12 inputs — enough to exercise rayon's work-stealing across
    // multiple threads on the 4-core CI runner.
    let mut input_paths: Vec<std::path::PathBuf> = Vec::with_capacity(12);
    for i in 0..12 {
        let p = tmp.path().join(format!("input-{i}.o"));
        std::fs::write(&p, format!("payload-bytes-{i}-{}", "x".repeat(64))).unwrap();
        input_paths.push(p);
    }

    let _cache_dir = CacheDirEnvGuard::set(&tmp.path().join("zccache-cache"));
    let server = DaemonServer::bind(&crate::ipc::unique_test_endpoint()).unwrap();

    let make_args = |inputs: &[std::path::PathBuf]| -> Vec<String> {
        let mut a = vec!["-o".to_string(), output.to_string_lossy().into_owned()];
        for p in inputs {
            a.push(p.to_string_lossy().into_owned());
        }
        a
    };

    // (1) First link with inputs in natural order — populates cache.
    let first_args = make_args(&input_paths);
    let first = handle_link_ephemeral(
        &server.state,
        std::process::id(),
        &fake_linker,
        &first_args,
        tmp.path(),
        None,
    )
    .await;
    assert!(
        matches!(
            first,
            Response::LinkResult {
                cached: false,
                exit_code: 0,
                ..
            }
        ),
        "first link must be a miss + 0 exit, got: {first:?}"
    );

    // (2) Repeat with same order — must hit.
    let second = handle_link_ephemeral(
        &server.state,
        std::process::id(),
        &fake_linker,
        &first_args,
        tmp.path(),
        None,
    )
    .await;
    assert!(
        matches!(second, Response::LinkResult { cached: true, exit_code: 0, .. }),
        "same-order repeat must HIT (parallel hash must preserve input order in cache key), got: {second:?}"
    );

    // (3) Same inputs, REVERSED order — must miss. If parallel hashing
    // ever lost order, this would falsely report a hit and corrupt
    // the cache key invariant.
    let mut reversed = input_paths.clone();
    reversed.reverse();
    let reversed_args = make_args(&reversed);
    let third = handle_link_ephemeral(
        &server.state,
        std::process::id(),
        &fake_linker,
        &reversed_args,
        tmp.path(),
        None,
    )
    .await;
    assert!(
        matches!(
            third,
            Response::LinkResult {
                cached: false,
                exit_code: 0,
                ..
            }
        ),
        "reversed-order link must MISS (input order is part of the cache key), got: {third:?}"
    );
}

#[tokio::test]
async fn directory_bundle_cache_hit_restores_complete_tree() {
    if !staged_link_lane_enabled() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let fake_dsymutil = write_fake_dsymutil(tmp.path());
    let input = tmp.path().join("app");
    let output = tmp.path().join("app.dSYM");
    std::fs::write(&input, b"fake executable with debug information").unwrap();

    let _cache_dir = CacheDirEnvGuard::set(&tmp.path().join("zccache-cache"));
    let server = DaemonServer::bind(&crate::ipc::unique_test_endpoint()).unwrap();
    let args = vec![
        input.to_string_lossy().into_owned(),
        "-o".to_string(),
        output.to_string_lossy().into_owned(),
    ];

    let first = handle_link_ephemeral(
        &server.state,
        std::process::id(),
        &fake_dsymutil,
        &args,
        tmp.path(),
        None,
    )
    .await;
    assert!(matches!(
        first,
        Response::LinkResult {
            exit_code: 0,
            cached: false,
            ..
        }
    ));
    let dwarf = output.join("Contents/Resources/DWARF/app");
    let plist = output.join("Contents/Info.plist");
    let dwarf_mtime = std::fs::metadata(&dwarf).unwrap().modified().unwrap();
    let dwarf_bytes = std::fs::read(&dwarf).unwrap();
    assert!(dwarf_bytes.starts_with(b"debug-binary"));
    assert!(plist.exists());

    std::fs::remove_dir_all(&output).unwrap();
    let second = handle_link_ephemeral(
        &server.state,
        std::process::id(),
        &fake_dsymutil,
        &args,
        tmp.path(),
        None,
    )
    .await;
    assert!(matches!(
        second,
        Response::LinkResult {
            exit_code: 0,
            cached: true,
            ..
        }
    ));
    assert_eq!(std::fs::read(&dwarf).unwrap(), dwarf_bytes);
    assert!(plist.exists());
    assert_eq!(
        std::fs::metadata(&dwarf).unwrap().modified().unwrap(),
        dwarf_mtime
    );

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(dwarf).unwrap().permissions().mode() & 0o777,
            0o755
        );
    }
}
