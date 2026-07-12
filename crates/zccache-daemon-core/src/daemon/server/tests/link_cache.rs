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

#[cfg(unix)]
fn write_fake_archiver(dir: &Path) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;

    let tool = dir.join("ar");
    std::fs::write(
        &tool,
        r#"#!/bin/sh
printf 'tick\n' >> "$(dirname "$0")/archive-ticks.txt"
printf 'archive\n' > "$2"
"#,
    )
    .unwrap();
    let mut permissions = std::fs::metadata(&tool).unwrap().permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&tool, permissions).unwrap();
    tool
}

#[cfg(unix)]
fn write_pure_linker(dir: &Path) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;

    let tool = dir.join("pure-clang");
    std::fs::write(
        &tool,
        r#"#!/bin/sh
printf 'tick\n' >> "$(dirname "$0")/link-ticks.txt"
printf 'linked\n' > "$2"
"#,
    )
    .unwrap();
    let mut permissions = std::fs::metadata(&tool).unwrap().permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&tool, permissions).unwrap();
    tool
}

#[cfg(windows)]
fn write_pure_linker(dir: &Path) -> std::path::PathBuf {
    let tool = dir.join("pure-clang.cmd");
    std::fs::write(
        &tool,
        r#"@echo off
>> "%~dp0link-ticks.txt" echo tick
> "%~2" echo linked
exit /b 0
"#,
    )
    .unwrap();
    tool
}

#[cfg(windows)]
fn write_fake_archiver(dir: &Path) -> std::path::PathBuf {
    let tool = dir.join("ar.cmd");
    std::fs::write(
        &tool,
        r#"@echo off
>> "%~dp0archive-ticks.txt" echo tick
> "%~2" echo archive
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
    let fake_linker = write_fake_linker(tmp.path());
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
async fn staged_archive_publication_fault_salvages_then_populates_v2() {
    if !staged_link_lane_enabled() {
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let tool = write_fake_archiver(temp.path());
    let input = temp.path().join("input.o");
    let output = temp.path().join("libfixture.a");
    let ticks = temp.path().join("archive-ticks.txt");
    std::fs::write(&input, b"object").unwrap();
    let _cache_dir = CacheDirEnvGuard::set(&temp.path().join("cache"));
    let server = DaemonServer::bind(&crate::ipc::unique_test_endpoint()).unwrap();
    let args = vec![
        "rcs".to_string(),
        output.to_string_lossy().into_owned(),
        input.to_string_lossy().into_owned(),
    ];
    let fault = StagedFaultGuard::arm(
        &server.state.artifact_dir,
        [StagedFaultPoint::PointerCommit],
    );

    let first = handle_link_ephemeral(
        &server.state,
        std::process::id(),
        &tool,
        &args,
        temp.path(),
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
    assert!(
        output.is_file(),
        "publication failure did not salvage output"
    );
    let staged = server.state.profiler.staged.snapshot();
    assert_eq!(staged.counters["publication_failure"], 1);
    assert_eq!(staged.counters["salvage_success"], 1);
    fault.assert_all_consumed();

    std::fs::remove_file(&output).unwrap();
    let second = handle_link_ephemeral(
        &server.state,
        std::process::id(),
        &tool,
        &args,
        temp.path(),
        None,
    )
    .await;
    assert!(matches!(
        second,
        Response::LinkResult {
            exit_code: 0,
            cached: false,
            ..
        }
    ));
    assert!(
        server
            .state
            .artifact_dir
            .join(".staged-v2")
            .read_dir()
            .unwrap()
            .flatten()
            .any(|entry| entry.path().extension().is_some_and(|ext| ext == "current")),
        "archive did not publish a v2 visibility pointer"
    );

    std::fs::remove_file(&output).unwrap();
    let third = handle_link_ephemeral(
        &server.state,
        std::process::id(),
        &tool,
        &args,
        temp.path(),
        None,
    )
    .await;
    assert!(matches!(
        third,
        Response::LinkResult {
            exit_code: 0,
            cached: true,
            ..
        }
    ));
    assert_eq!(
        std::fs::read_to_string(ticks).unwrap().lines().count(),
        2,
        "cache hit unexpectedly reran the archiver"
    );
}

#[tokio::test]
async fn staged_declared_linker_publishes_v2_then_hits() {
    if !staged_link_lane_enabled() {
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let tool = write_pure_linker(temp.path());
    let work = temp.path().join("work");
    std::fs::create_dir(&work).unwrap();
    let input = work.join("input.o");
    let output = work.join("application.exe");
    std::fs::write(&input, b"object").unwrap();
    let _cache_dir = CacheDirEnvGuard::set(&temp.path().join("cache"));
    let server = DaemonServer::bind(&crate::ipc::unique_test_endpoint()).unwrap();
    let args = vec![
        "-o".to_string(),
        output.to_string_lossy().into_owned(),
        input.to_string_lossy().into_owned(),
    ];

    let first =
        handle_link_ephemeral(&server.state, std::process::id(), &tool, &args, &work, None).await;
    assert!(matches!(
        first,
        Response::LinkResult {
            exit_code: 0,
            cached: false,
            ..
        }
    ));
    assert_eq!(
        server.state.profiler.staged.snapshot().counters["publication_success"],
        1
    );
    assert!(
        server
            .state
            .artifact_dir
            .join(".staged-v2")
            .read_dir()
            .unwrap()
            .flatten()
            .any(|entry| entry.path().extension().is_some_and(|ext| ext == "current")),
        "declared linker did not publish a v2 visibility pointer"
    );

    std::fs::remove_file(&output).unwrap();
    let second =
        handle_link_ephemeral(&server.state, std::process::id(), &tool, &args, &work, None).await;
    assert!(matches!(
        second,
        Response::LinkResult {
            exit_code: 0,
            cached: true,
            ..
        }
    ));
    assert_eq!(
        std::fs::read_to_string(temp.path().join("link-ticks.txt"))
            .unwrap()
            .lines()
            .count(),
        1,
        "cache hit unexpectedly reran the linker"
    );
}
