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

/// Writes a fake `cc` that mirrors what the staged compile lane needs from
/// a real compiler:
///
/// * With `-o <path>` (the staged per-unit invocation shape): writes a
///   deterministic `object-for:<abs source>` payload to `<path>`.
/// * Without `-o` (bare `cc -c a.c b.c`): writes `<stem>.o` next to each
///   `.c` argument, exactly like gcc/clang.
/// * Discovery probes (`-v -E ...`, `-###`) see no `.c` args and no `-o`,
///   and become successful no-ops.
///
/// `-MF`/`-MT` values are skipped so a depfile path ending in a source-like
/// name can never be mistaken for an input.
#[cfg(unix)]
fn write_fake_multi_cc(dir: &Path) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;

    let tool = dir.join("cc");
    std::fs::write(
        &tool,
        r#"#!/bin/sh
out=
srcs=
while [ "$#" -gt 0 ]; do
    case "$1" in
        -o) shift; out=$1 ;;
        -MF|-MT) shift ;;
        *.c) srcs="$srcs $1" ;;
    esac
    shift || true
done
if [ -n "$out" ]; then
    for s in $srcs; do
        printf 'object-for:%s\n' "$s" > "$out"
    done
else
    for s in $srcs; do
        printf 'object-for:%s\n' "$s" > "${s%.c}.o"
    done
fi
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
fn write_fake_multi_cc(dir: &Path) -> PathBuf {
    let tool = dir.join("cc.cmd");
    std::fs::write(
        &tool,
        r#"@echo off
setlocal enabledelayedexpansion
set "OUT="
set "SRCS="
:loop
if "%~1"=="" goto run
if "%~1"=="-o" (
    set "OUT=%~2"
    shift
    shift
    goto loop
)
if "%~1"=="-MF" (
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
    exit /b 0
)
for %%S in (!SRCS!) do (
    > "%%~dpnS.o" echo object-for:%%~S
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
fn restore_dep_graph_from_disk(server: &DaemonServer) {
    let path = crate::depgraph::depgraph_file_path();
    if let crate::depgraph::DepGraphLoadOutcome::Loaded { graph } =
        crate::depgraph::classify_load(&path)
    {
        server.set_dep_graph(graph);
    }
}

fn save_dep_graph_to_disk(server: &DaemonServer) {
    let dg = server.state.dep_graph.load_full();
    let path = crate::depgraph::depgraph_file_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    crate::depgraph::save_to_file(&dg, &path).expect("depgraph save must succeed");
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
fn spawn_index_writer(server: &mut DaemonServer) -> tokio::task::JoinHandle<()> {
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
async fn quiesce_and_persist(
    server: &DaemonServer,
    index_writer_handle: tokio::task::JoinHandle<()>,
) {
    let _publication_guard =
        drain_durable_state_for_shutdown(&server.state, Some(index_writer_handle)).await;
    save_dep_graph_to_disk(server);
}

/// A two-source multi-file compile must hit warm after a graceful daemon
/// restart, exactly like an equivalent single-source compile already does.
#[tokio::test]
async fn multi_file_compile_hits_warm_after_restart() {
    let tmp = tempfile::tempdir().unwrap();
    let cache_root = tmp.path().join("zccache-cache");
    let _guard = CacheDirEnvGuard::set_with_staged_artifacts(&cache_root, "all");
    // Mirror the Integration workflow's `legacy_path_validation.rs` flow:
    // ZCCACHE_STAGED_ARTIFACTS=all routes the multi misses through
    // `staged::try_handle_staged_misses` (per-unit staged compile with an
    // explicit `-o`), not the inline hardlink lane.

    let cc = write_fake_multi_cc(tmp.path());
    let work = tmp.path().join("work");
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
    let mut cold_server = DaemonServer::bind_with_cache_dir(
        &crate::ipc::unique_test_endpoint(),
        &cache_root.clone().into(),
    )
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
    quiesce_and_persist(&cold_server, index_writer_handle).await;
    eprintln!(
        "[diag] cold shutdown artifact_index_entries={} depgraph_contexts_with_artifact_key={}",
        cold_server.state.artifact_store.len(),
        cold_server
            .state
            .dep_graph
            .load()
            .contexts_with_artifact_key(),
    );
    drop(cold_server);

    // Clear outputs so the warm assertion below can only pass via a real
    // cache-materialized hit, not a leftover cold-phase file.
    std::fs::remove_file(&out_a).unwrap();
    std::fs::remove_file(&out_b).unwrap();

    // ── Warm daemon: fresh process-equivalent state, same cache root ────
    let warm_server = DaemonServer::bind_with_cache_dir(
        &crate::ipc::unique_test_endpoint(),
        &cache_root.clone().into(),
    )
    .unwrap();
    restore_dep_graph_from_disk(&warm_server);
    eprintln!(
        "[diag] warm daemon depgraph contexts_with_artifact_key = {}",
        warm_server
            .state
            .dep_graph
            .load()
            .contexts_with_artifact_key()
    );

    let diag_log = tmp.path().join("ephemeral.log");
    std::env::set_var("ZCCACHE_EPHEMERAL_LOG", &diag_log);
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
    std::env::remove_var("ZCCACHE_EPHEMERAL_LOG");
    if let Ok(contents) = std::fs::read_to_string(&diag_log) {
        eprintln!("[diag] session log:\n{contents}");
    }
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
async fn single_file_compile_hits_warm_after_restart() {
    let tmp = tempfile::tempdir().unwrap();
    let cache_root = tmp.path().join("zccache-cache");
    let _guard = CacheDirEnvGuard::set_with_staged_artifacts(&cache_root, "all");
    // Same staged lane as the multi variant + the Integration workflow.

    let cc = write_fake_multi_cc(tmp.path());
    let work = tmp.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    let single = work.join("single.c");
    std::fs::write(&single, "int single(void) { return 1; }\n").unwrap();
    let out = work.join("single.o");

    let args = vec!["-c".to_string(), single.to_string_lossy().into_owned()];

    let mut cold_server = DaemonServer::bind_with_cache_dir(
        &crate::ipc::unique_test_endpoint(),
        &cache_root.clone().into(),
    )
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

    quiesce_and_persist(&cold_server, index_writer_handle).await;
    drop(cold_server);
    std::fs::remove_file(&out).unwrap();

    let warm_server = DaemonServer::bind_with_cache_dir(
        &crate::ipc::unique_test_endpoint(),
        &cache_root.clone().into(),
    )
    .unwrap();
    restore_dep_graph_from_disk(&warm_server);

    let diag_log = tmp.path().join("ephemeral-single.log");
    std::env::set_var("ZCCACHE_EPHEMERAL_LOG", &diag_log);
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
    std::env::remove_var("ZCCACHE_EPHEMERAL_LOG");
    if let Ok(contents) = std::fs::read_to_string(&diag_log) {
        eprintln!("[diag] single session log:\n{contents}");
    }
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
