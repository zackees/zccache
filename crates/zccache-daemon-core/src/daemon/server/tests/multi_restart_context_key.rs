//! Cross-restart regression test for the warm-multi `context_not_found`
//! class of miss reproduced by
//! `crates/zccache-daemon-core/tests/legacy_path_validation.rs`
//! `strict_layout_validation_aggregates_all_runtime_flows` on the 2-core CI
//! runner: a multi-source compile that hits cold-cache should also hit after
//! a graceful daemon restart (depgraph snapshot round-trip), exactly like
//! the single-source path already does.
//!
//! Windows-runnable counterpart to `legacy_path_validation.rs` (which is
//! `#[cfg(unix)]`-gated and needs a real clang/ar). This test uses a tiny
//! fake `cc` shim (`.cmd` on Windows, shell script on Unix) instead, and
//! drives the compile pipeline directly via `handle_compile_ephemeral` —
//! no IPC socket, no real compiler.
//!
//! ## Root-cause finding
//!
//! Both `multi_file_compile_hits_warm_after_restart` and
//! `single_file_compile_hits_warm_after_restart` pass once the harness
//! properly quiesces async publication before "shutting down" the cold
//! daemon (`quiesce_and_persist` below) — there is no context-key /
//! depgraph-registration divergence between the single- and multi-file
//! paths (`register_with_root_and_salt` bare `.key` vs
//! `register_with_root_and_salt_result(..).map_key` was audited and is not
//! the cause: a targeted depgraph-level round-trip test
//! (`crates/zccache-depgraph/src/graph/tests.rs`
//! `diag_multi_style_register_survives_snapshot_roundtrip`) proves
//! `resolve_instance_key` still resolves the bare logical key correctly
//! after a snapshot round-trip, because `from_snapshot` rebuilds
//! `equivalent_contexts`).
//!
//! What DOES reproduce a miss in this harness is skipping either of two
//! async-durability steps that `DaemonServer::run()`'s Shutdown arm performs
//! for real (see `spawn_index_writer` / `quiesce_and_persist`):
//! 1. The background index-writer task that drains `index_writer_tx` into
//!    the durable `ArtifactStore`/redb index — never spawned outside
//!    `run()`.
//! 2. The single-file miss path's backgrounded artifact persist
//!    (`handle_compile/miss_store.rs`, gated behind `persist_semaphore`,
//!    tracked via `state.in_flight_bytes`) — unlike the multi-file path's
//!    synchronous inline hardlink.
//!
//! This matches the `#1161`/`#799` class of "shutdown doesn't durably drain
//! publication" gap already called out in `legacy_path_validation.rs`'s
//! `wait_for_staged_publication` / `wait_for_depgraph_contexts` comments.
//! Notably, `wait_for_depgraph_contexts` there polls raw
//! `DaemonStatus::dep_graph_contexts` (`DepGraph::stats().context_count`),
//! which counts contexts in ANY state — including freshly `register()`-ed
//! `Cold` entries with no `artifact_key` yet. It does not guarantee the
//! stronger invariant this test polls for
//! (`contexts_with_artifact_key() >= N`), so it can be satisfied well before
//! the artifact-durability work above has actually finished — a plausible
//! reason the CI wait still races on a contended 2-core runner even though
//! production's real `run()` Shutdown handler does perform the drain
//! correctly (see `pending_writes::await_all` in `daemon/server/run.rs`).

use std::path::{Path, PathBuf};

use super::super::*;
use super::CacheDirEnvGuard;

/// Writes a fake `cc` that mirrors the C/C++ compile shape needed by this
/// test:
///
/// * With `-o <path>` (the staged per-unit invocation shape): writes a
///   deterministic `object-for:<abs source>` payload to `<path>`.
/// * Without `-o` (bare `cc -c a.c b.c`): writes `<stem>.o` next to each
///   `.c` argument, exactly like gcc/clang.
/// * Discovery probes (`-v -E ...`, `-###`) see no `.c` args and no `-o`,
///   and become successful no-ops.
///
/// `-MF` is captured and `-MT` is skipped so their values can never be
/// mistaken for inputs. Depfile sources are cwd-relative, matching the test
/// working directory without exposing host TEMP-path escaping to the shim.
#[cfg(unix)]
pub(super) fn write_fake_multi_cc(dir: &Path) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;

    let tool = dir.join("cc");
    std::fs::write(
        &tool,
        r#"#!/bin/sh
out=
depfile=
operand=
for arg do
    if [ -n "$operand" ]; then
        case "$operand" in
            out) out=$arg ;;
            depfile) depfile=$arg ;;
            skip) ;;
        esac
        operand=
        continue
    fi
    case "$arg" in
        -v)
            printf '%s\n' '#include <...> search starts here:' >&2
            printf '%s\n' ' /usr/include' >&2
            printf '%s\n' 'End of search list.' >&2
            exit 0
            ;;
        -o) operand=out ;;
        -MF) operand=depfile ;;
        -MT) operand=skip ;;
    esac
done
if [ -n "$depfile" ]; then
    printf 'object:' > "$depfile"
fi
operand=
for arg do
    if [ -n "$operand" ]; then
        operand=
        continue
    fi
    case "$arg" in
        -o|-MF|-MT) operand=value ;;
        *.c)
            if [ -n "$out" ]; then
                printf 'object-for:%s\n' "$arg" > "$out"
            else
                printf 'object-for:%s\n' "$arg" > "${arg%.c}.o"
            fi
            if [ -n "$depfile" ]; then
                printf ' %s' "${arg##*/}" >> "$depfile"
            fi
            ;;
    esac
done
if [ -n "$depfile" ]; then printf '\n' >> "$depfile"; fi
exit 0
"#,
    )
    .unwrap();
    let mut perms = std::fs::metadata(&tool).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&tool, perms).unwrap();
    tool
}

#[cfg(windows)]
pub(super) fn write_fake_multi_cc(dir: &Path) -> PathBuf {
    let tool = dir.join("cc.cmd");
    std::fs::write(
        &tool,
        r#"@echo off
setlocal enabledelayedexpansion
set "OUT="
set "SRCS="
set "DEPFILE="
:loop
if "%~1"=="" goto run
if "%~1"=="-o" (
    set "OUT=%~2"
    shift
    shift
    goto loop
)
if "%~1"=="-v" (
    >&2 echo #include ^<...^> search starts here:
    >&2 echo  C:\fake-system-include
    >&2 echo End of search list.
    exit /b 0
)
if "%~1"=="-MF" (
    set "DEPFILE=%~2"
    shift
    shift
    goto loop
)
if "%~1"=="-MT" (
    shift
    shift
    goto loop
)
set "ARG=%~1"
if /I "!ARG:~-2!"==".c" set SRCS=!SRCS! "%~f1"
shift
goto loop
:run
if defined OUT (
    for %%S in (!SRCS!) do (
        > "!OUT!" echo object-for:%%~S
    )
) else (
    for %%S in (!SRCS!) do (
        > "%%~dpnS.o" echo object-for:%%~S
    )
)
if defined DEPFILE (
    > "!DEPFILE!" <nul set /p ="object:"
    for %%S in (!SRCS!) do (
        >> "!DEPFILE!" <nul set /p =" %%~nxS"
    )
    >> "!DEPFILE!" echo.
)
exit /b 0
"#,
    )
    .unwrap();
    tool
}

/// Reload the on-disk depgraph snapshot into a freshly-bound `DaemonServer`,
/// mirroring the production startup path (`daemon::entry`) and the pattern
/// already used by `legacy_path_validation.rs`'s `Daemon::start`.
fn restore_dep_graph_from_disk(server: &DaemonServer, path: &Path) {
    if let crate::depgraph::DepGraphLoadOutcome::Loaded { graph } =
        crate::depgraph::classify_load(path)
    {
        server.set_dep_graph(graph);
    }
}

pub(super) fn save_dep_graph_to_disk(server: &DaemonServer, path: &Path) {
    let dg = server.state.dep_graph.load_full();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    crate::depgraph::save_to_file(&dg, path).expect("depgraph save must succeed");
}

/// `DaemonServer::run` spawns the background index-writer task (drains
/// `index_writer_tx` into the durable `ArtifactStore`/redb index) as its
/// first action. Tests that drive the compile pipeline directly via
/// `handle_compile_ephemeral` (bypassing `run()`) never spawn it, so
/// `IndexWriterCommand::Insert` messages sent by a cache-miss just pile up
/// unconsumed in the channel — the in-memory `state.artifacts` DashMap has
/// the entry, but the on-disk index never does. Mirrors `run.rs`'s startup
/// snippet exactly so a synthetic restart in this harness sees what a real
/// restart would see.
pub(super) fn spawn_index_writer(server: &mut DaemonServer) -> tokio::task::JoinHandle<()> {
    let rx = server
        .index_writer_rx
        .take()
        .expect("index_writer_rx must not already be taken");
    let store = std::sync::Arc::clone(&server.state.artifact_store);
    let shutdown = std::sync::Arc::clone(&server.state.index_writer_shutdown);
    tokio::spawn(run_index_writer(rx, store, shutdown))
}

/// Graceful-shutdown equivalent for this harness: run the EXACT production
/// durability drain (`wal::drain_durable_state_for_shutdown`, the same
/// function `DaemonServer::run`'s Shutdown arm calls — pending persist
/// tasks → publication write barrier → index-writer WAL flush ack → writer
/// stop → final store flush), then snapshot the depgraph while the returned
/// publication guard is still held, mirroring `run()`'s ordering.
///
/// Using the shared production function (not a test-side approximation) is
/// the point: the single-file miss path backgrounds its artifact persist
/// behind `persist_semaphore` (`handle_compile/miss_store.rs`) and only its
/// `pending_cache_writes` registration makes that observable — a previous
/// version of this helper polled `in_flight_bytes` instead and still raced
/// CI timing on the Linux runner (#1161).
pub(super) async fn quiesce_and_persist(
    server: &DaemonServer,
    index_writer_handle: tokio::task::JoinHandle<()>,
    depgraph_path: &Path,
) {
    let _publication_guard =
        drain_durable_state_for_shutdown(&server.state, Some(index_writer_handle)).await;
    save_dep_graph_to_disk(server, depgraph_path);
}

/// A two-source legacy C/C++ compile must hit warm after a graceful daemon
/// restart. Unix intentionally suppresses staged multi-source publication
/// without a trustworthy per-file change sequence, so this verifies the
/// supported durable path on every platform.
#[tokio::test]
// Holding the env-policy lock across the whole async test IS the point:
// it serializes the process-global staged-artifact policy for the test's
// full duration. Single-threaded test runtime, no lock-ordering hazard.
#[allow(clippy::await_holding_lock)]
async fn multi_file_compile_hits_warm_after_restart() {
    let tmp = tempfile::tempdir().unwrap();
    let cache_root: crate::core::NormalizedPath = tmp.path().join("zccache-cache").into();
    // Both servers use explicit cache roots.  Do not set ZCCACHE_CACHE_DIR:
    // unrelated parallel tests that call `DaemonServer::bind()` would then
    // accidentally adopt this test's cache and corrupt its restart fixture.
    // The lock only prevents other tests from changing the process-global
    // staged-artifact policy while this C/C++ (default-disabled) lane runs.
    let _env_lock = CacheDirEnvGuard::lock();
    let depgraph_path =
        crate::core::config::depgraph_dir_from_cache_dir(&cache_root).join("depgraph.bin");

    let cc = write_fake_multi_cc(tmp.path());
    let work = tmp.path().join("work with spaces");
    std::fs::create_dir_all(&work).unwrap();
    let a = work.join("multi_a.c");
    let b = work.join("multi_b.c");
    std::fs::write(&a, "int multi_a(void) { return 2; }\n").unwrap();
    std::fs::write(&b, "int multi_b(void) { return 3; }\n").unwrap();
    let out_a = work.join("multi_a.o");
    let out_b = work.join("multi_b.o");

    let args = vec![
        "-c".to_string(),
        a.to_string_lossy().into_owned(),
        b.to_string_lossy().into_owned(),
    ];

    // ── Cold daemon: multi-file compile is a cold miss ──────────────────
    let mut cold_server =
        DaemonServer::bind_with_cache_dir(&crate::ipc::unique_test_endpoint(), &cache_root)
            .unwrap();
    let index_writer_handle = spawn_index_writer(&mut cold_server);
    let cold_resp = handle_compile_ephemeral(
        &cold_server.state,
        std::process::id(),
        &work,
        &cc,
        &args,
        &work,
        None,
        Vec::new(),
    )
    .await;
    match &cold_resp {
        Response::CompileResult {
            exit_code, cached, ..
        } => {
            assert_eq!(*exit_code, 0, "cold multi-file compile must succeed");
            assert!(!*cached, "first multi-file compile must be a cold miss");
        }
        other => panic!("expected CompileResult on cold path, got {other:?}"),
    }
    let cold_a = std::fs::read(&out_a).expect("cold multi_a.o must exist");
    let cold_b = std::fs::read(&out_b).expect("cold multi_b.o must exist");

    // Run the production shutdown drain + persist the depgraph, exactly
    // like `DaemonServer::run`'s Shutdown arm does.
    quiesce_and_persist(&cold_server, index_writer_handle, &depgraph_path).await;
    drop(cold_server);

    // Clear outputs so the warm assertion below can only pass via a real
    // cache-materialized hit, not a leftover cold-phase file.
    std::fs::remove_file(&out_a).unwrap();
    std::fs::remove_file(&out_b).unwrap();

    // ── Warm daemon: fresh process-equivalent state, same cache root ────
    let warm_server =
        DaemonServer::bind_with_cache_dir(&crate::ipc::unique_test_endpoint(), &cache_root)
            .unwrap();
    restore_dep_graph_from_disk(&warm_server, &depgraph_path);
    let warm_resp = handle_compile_ephemeral(
        &warm_server.state,
        std::process::id(),
        &work,
        &cc,
        &args,
        &work,
        None,
        Vec::new(),
    )
    .await;
    match &warm_resp {
        Response::CompileResult {
            exit_code, cached, ..
        } => {
            assert_eq!(*exit_code, 0, "warm multi-file compile must succeed");
            assert!(
                *cached,
                "warm multi-file compile must hit after a graceful restart, \
                 exactly like the single-source path — a miss here reproduces \
                 the CI context_not_found regression"
            );
        }
        other => panic!("expected CompileResult on warm path, got {other:?}"),
    }
    let warm_a = std::fs::read(&out_a).expect("warm multi_a.o must exist");
    let warm_b = std::fs::read(&out_b).expect("warm multi_b.o must exist");
    assert_eq!(
        cold_a, warm_a,
        "warm-hit multi_a.o content must match the cold-miss content"
    );
    assert_eq!(
        cold_b, warm_b,
        "warm-hit multi_b.o content must match the cold-miss content"
    );
}

/// Baseline sanity check: the single-source path already hits warm after a
/// restart under this same harness. Keeps the multi-file regression above
/// honest — if this one ever goes RED too, the bug moved somewhere shared
/// (e.g. depgraph snapshot round-trip) rather than being multi-file-specific.
#[tokio::test]
// Same rationale as multi_file_compile_hits_warm_after_restart: the env
// lock must span every await in the test body.
#[allow(clippy::await_holding_lock)]
async fn single_file_compile_hits_warm_after_restart() {
    let tmp = tempfile::tempdir().unwrap();
    let cache_root: crate::core::NormalizedPath = tmp.path().join("zccache-cache").into();
    let _env_lock = CacheDirEnvGuard::lock();
    let depgraph_path =
        crate::core::config::depgraph_dir_from_cache_dir(&cache_root).join("depgraph.bin");

    let cc = write_fake_multi_cc(tmp.path());
    let work = tmp.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    let single = work.join("single.c");
    std::fs::write(&single, "int single(void) { return 1; }\n").unwrap();
    let out = work.join("single.o");

    let args = vec!["-c".to_string(), single.to_string_lossy().into_owned()];

    let mut cold_server =
        DaemonServer::bind_with_cache_dir(&crate::ipc::unique_test_endpoint(), &cache_root)
            .unwrap();
    let index_writer_handle = spawn_index_writer(&mut cold_server);
    let cold_resp = handle_compile_ephemeral(
        &cold_server.state,
        std::process::id(),
        &work,
        &cc,
        &args,
        &work,
        None,
        Vec::new(),
    )
    .await;
    match &cold_resp {
        Response::CompileResult {
            exit_code, cached, ..
        } => {
            assert_eq!(*exit_code, 0);
            assert!(!*cached);
        }
        other => panic!("expected CompileResult on cold path, got {other:?}"),
    }
    let cold_bytes = std::fs::read(&out).expect("cold single.o must exist");

    quiesce_and_persist(&cold_server, index_writer_handle, &depgraph_path).await;
    drop(cold_server);
    std::fs::remove_file(&out).unwrap();

    let warm_server =
        DaemonServer::bind_with_cache_dir(&crate::ipc::unique_test_endpoint(), &cache_root)
            .unwrap();
    restore_dep_graph_from_disk(&warm_server, &depgraph_path);

    let warm_resp = handle_compile_ephemeral(
        &warm_server.state,
        std::process::id(),
        &work,
        &cc,
        &args,
        &work,
        None,
        Vec::new(),
    )
    .await;
    match &warm_resp {
        Response::CompileResult {
            exit_code, cached, ..
        } => {
            assert_eq!(*exit_code, 0);
            assert!(*cached, "single-source path must hit warm after restart");
        }
        other => panic!("expected CompileResult on warm path, got {other:?}"),
    }
    let warm_bytes = std::fs::read(&out).expect("warm single.o must exist");
    assert_eq!(cold_bytes, warm_bytes);
}
