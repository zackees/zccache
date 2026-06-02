//! Adversarial tests for the cold-path deferred-write contract (#610, DD-025).
//!
//! DD-025 condition 4 requires adversarial tests to land *before* the
//! defer-the-index optimization. This module owns those tests.
//!
//! ## Invariants under test
//!
//! For any artifact key `k`:
//! - **No wrong hit**: a lookup for `k` returns either correct content for
//!   `k` or `miss` (which triggers recompile). It never returns content for
//!   a different key, nor partial/zeroed content.
//! - **Eventual consistency**: after the deferred-write spawn completes,
//!   subsequent lookups for `k` either hit-with-correct-content or report
//!   miss — they never return wrong content because of a race.
//!
//! ## Scope today
//!
//! The optimization itself is not yet implemented. These tests assert the
//! current (synchronous-store) path already upholds the invariant — a
//! conformance baseline. When the deferred-write path lands, the same
//! tests must continue to pass under both the synchronous and deferred
//! code paths (selectable via env var during the optimization rollout).
//!
//! Future iterations will add:
//! - Same-key concurrent-lookup race coverage with timing pressure
//! - Cross-key contention with DashMap shard collision
//! - Crash-mid-flight (daemon-restart) recovery assertion
//! - Notify-timeout fall-through to miss
//! - Loom + thread-sanitizer harness (separate test target)

#![cfg(unix)] // Test fixtures shell out to /bin/sh; Windows variant deferred.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use super::super::*;
use super::CacheDirEnvGuard;

/// Writes a tiny fake C compiler at `dir/cc` that produces deterministic
/// output bytes derived from the source path. Avoids depending on a real
/// gcc/clang being installed.
fn write_fake_cc(dir: &Path) -> PathBuf {
    let tool = dir.join("cc");
    std::fs::write(
        &tool,
        r#"#!/bin/sh
# Minimal cc shim: accept `-c <src> -o <out>` and write a deterministic
# byte payload derived from the source path so test assertions can verify
# "we got the right output" without depending on a real toolchain.
src=
out=
while [ "$#" -gt 0 ]; do
    case "$1" in
        -c) shift; src=$1 ;;
        -o) shift; out=$1 ;;
    esac
    shift || true
done
if [ -z "$src" ] || [ -z "$out" ]; then
    exit 2
fi
# Deterministic, source-dependent payload — different sources produce
# different bytes, so any "wrong hit" is observable.
printf 'object-for:%s\n' "$src" > "$out"
"#,
    )
    .unwrap();
    let mut perms = std::fs::metadata(&tool).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&tool, perms).unwrap();
    tool
}

/// Cold-miss followed by an immediate warm-hit must return identical content.
///
/// This is the baseline conformance assertion that holds today under the
/// synchronous-store path and must continue to hold once `dep_graph.update`
/// and `state.artifacts.insert` are deferred per #610.
#[tokio::test]
async fn cold_then_warm_returns_identical_content() {
    let tmp = tempfile::tempdir().unwrap();
    let _guard = CacheDirEnvGuard::set(&tmp.path().join("zccache-cache"));
    let server = DaemonServer::bind(&crate::ipc::unique_test_endpoint()).unwrap();

    let cc = write_fake_cc(tmp.path());
    let src = tmp.path().join("foo.c");
    std::fs::write(&src, b"int foo(void) { return 1; }\n").unwrap();
    let out = tmp.path().join("foo.o");

    let args = vec![
        "-c".to_string(),
        src.to_string_lossy().into_owned(),
        "-o".to_string(),
        out.to_string_lossy().into_owned(),
    ];

    // Cold miss: artifact directory is fresh.
    let cold = handle_compile_ephemeral(
        &server.state,
        std::process::id(),
        tmp.path(),
        &cc,
        &args,
        tmp.path(),
        None,
        Vec::new(),
    )
    .await;
    let (cold_exit, cold_cached) = match cold {
        Response::CompileResult {
            exit_code, cached, ..
        } => (exit_code, cached),
        other => panic!("expected CompileResult on cold path, got {other:?}"),
    };
    assert_eq!(cold_exit, 0, "cold compile must succeed");
    assert!(!cold_cached, "first compile must report cached=false");
    let cold_bytes = std::fs::read(&out).expect("cold output must exist on disk");
    assert!(
        !cold_bytes.is_empty(),
        "cold output must not be empty (got {} bytes)",
        cold_bytes.len()
    );

    // Warm hit: same args. Re-clear the output file first so we can prove
    // the response materialized fresh content from the cache.
    std::fs::remove_file(&out).ok();
    let warm = handle_compile_ephemeral(
        &server.state,
        std::process::id(),
        tmp.path(),
        &cc,
        &args,
        tmp.path(),
        None,
        Vec::new(),
    )
    .await;
    let warm_exit = match warm {
        Response::CompileResult { exit_code, .. } => exit_code,
        other => panic!("expected CompileResult on warm path, got {other:?}"),
    };
    assert_eq!(warm_exit, 0, "warm compile must succeed");
    let warm_bytes = std::fs::read(&out).expect("warm output must exist on disk");

    // Core invariant: content from a cache hit must match the cold-miss content
    // byte-for-byte. A wrong hit returning different content would fail here.
    assert_eq!(
        cold_bytes, warm_bytes,
        "warm-hit content must match cold-miss content byte-for-byte"
    );
}

/// Two compiles with DIFFERENT sources must produce DIFFERENT cached outputs.
///
/// This is the "no cross-key wrong hit" guarantee. If a lookup for source A
/// accidentally returned the artifact for source B, this assertion would
/// catch it because the fake compiler emits source-dependent bytes.
#[tokio::test]
async fn distinct_sources_have_distinct_cached_outputs() {
    let tmp = tempfile::tempdir().unwrap();
    let _guard = CacheDirEnvGuard::set(&tmp.path().join("zccache-cache"));
    let server = DaemonServer::bind(&crate::ipc::unique_test_endpoint()).unwrap();

    let cc = write_fake_cc(tmp.path());
    let src_a = tmp.path().join("a.c");
    let src_b = tmp.path().join("b.c");
    std::fs::write(&src_a, b"int a(void) { return 1; }\n").unwrap();
    std::fs::write(&src_b, b"int b(void) { return 2; }\n").unwrap();
    let out_a = tmp.path().join("a.o");
    let out_b = tmp.path().join("b.o");

    let args_a = vec![
        "-c".to_string(),
        src_a.to_string_lossy().into_owned(),
        "-o".to_string(),
        out_a.to_string_lossy().into_owned(),
    ];
    let args_b = vec![
        "-c".to_string(),
        src_b.to_string_lossy().into_owned(),
        "-o".to_string(),
        out_b.to_string_lossy().into_owned(),
    ];

    // Compile A, then B. Both are cold misses (distinct sources/keys).
    for args in [&args_a, &args_b] {
        let resp = handle_compile_ephemeral(
            &server.state,
            std::process::id(),
            tmp.path(),
            &cc,
            args,
            tmp.path(),
            None,
            Vec::new(),
        )
        .await;
        match resp {
            Response::CompileResult { exit_code, .. } => assert_eq!(exit_code, 0),
            other => panic!("expected CompileResult, got {other:?}"),
        }
    }

    let bytes_a = std::fs::read(&out_a).expect("a.o must exist");
    let bytes_b = std::fs::read(&out_b).expect("b.o must exist");
    assert_ne!(
        bytes_a, bytes_b,
        "distinct sources must produce distinct cached outputs — got identical bytes which would indicate cross-key cache aliasing"
    );

    // Now invoke each WARM and verify the materialized content still matches
    // the cold-miss content for that source. A defer-vs-lookup race could
    // surface as content from the wrong source appearing in the warm output.
    std::fs::remove_file(&out_a).ok();
    std::fs::remove_file(&out_b).ok();
    for (args, _expected) in [(&args_a, &bytes_a), (&args_b, &bytes_b)] {
        let resp = handle_compile_ephemeral(
            &server.state,
            std::process::id(),
            tmp.path(),
            &cc,
            args,
            tmp.path(),
            None,
            Vec::new(),
        )
        .await;
        match resp {
            Response::CompileResult { exit_code, .. } => assert_eq!(exit_code, 0),
            other => panic!("expected CompileResult, got {other:?}"),
        }
    }
    let warm_a = std::fs::read(&out_a).expect("warm a.o");
    let warm_b = std::fs::read(&out_b).expect("warm b.o");
    assert_eq!(
        warm_a, *bytes_a,
        "warm a.o must match its own cold-miss content (no cross-key contamination)"
    );
    assert_eq!(
        warm_b, *bytes_b,
        "warm b.o must match its own cold-miss content (no cross-key contamination)"
    );
}

/// Concurrent compile requests for the same source must each observe content
/// derived from that source — never content from a different source.
///
/// This exercises the race window between `store_miss_artifact` completing and
/// the in-memory cache becoming visible to subsequent lookups. Today
/// (synchronous-store path) every concurrent task either sees the entry
/// already inserted or recompiles from scratch — both produce the same bytes
/// because the fake cc is deterministic-per-source. When #610's
/// deferred-write path lands, the race window widens; this test catches any
/// wrong-hit returning bytes derived from a different cache key's source.
///
/// Each task uses its own output path so concurrent writes don't clobber
/// each other; the *content* assertion is the invariant under test.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_lookups_after_cold_miss_return_consistent_content() {
    let tmp = tempfile::tempdir().unwrap();
    let _guard = CacheDirEnvGuard::set(&tmp.path().join("zccache-cache"));
    let server = DaemonServer::bind(&crate::ipc::unique_test_endpoint()).unwrap();
    let state = std::sync::Arc::clone(&server.state);

    let cc = write_fake_cc(tmp.path());
    let src = tmp.path().join("shared.c");
    std::fs::write(&src, b"int shared(void) { return 7; }\n").unwrap();
    let cold_out = tmp.path().join("shared.o");

    // Cold miss seeds the cache.
    let cold_args = vec![
        "-c".to_string(),
        src.to_string_lossy().into_owned(),
        "-o".to_string(),
        cold_out.to_string_lossy().into_owned(),
    ];
    let cold_resp = handle_compile_ephemeral(
        &state,
        std::process::id(),
        tmp.path(),
        &cc,
        &cold_args,
        tmp.path(),
        None,
        Vec::new(),
    )
    .await;
    match cold_resp {
        Response::CompileResult { exit_code, .. } => assert_eq!(exit_code, 0),
        other => panic!("expected CompileResult on cold path, got {other:?}"),
    }
    let expected = std::fs::read(&cold_out).expect("cold output present");

    // 16 concurrent lookups for the SAME source, each writing to its own
    // output path so per-task disk writes don't collide. Every task should
    // either hit the cache or recompile to the same deterministic bytes.
    const N: usize = 16;
    let mut handles = Vec::with_capacity(N);
    for i in 0..N {
        let state = std::sync::Arc::clone(&state);
        let cc = cc.clone();
        let src = src.clone();
        let cwd = tmp.path().to_path_buf();
        let out_path = tmp.path().join(format!("shared_{i}.o"));
        let task_args = vec![
            "-c".to_string(),
            src.to_string_lossy().into_owned(),
            "-o".to_string(),
            out_path.to_string_lossy().into_owned(),
        ];
        handles.push(tokio::spawn(async move {
            let resp = handle_compile_ephemeral(
                &state,
                std::process::id(),
                &cwd,
                &cc,
                &task_args,
                &cwd,
                None,
                Vec::new(),
            )
            .await;
            match resp {
                Response::CompileResult { exit_code, .. } => {
                    assert_eq!(exit_code, 0, "task {i} must succeed");
                }
                other => panic!("task {i}: expected CompileResult, got {other:?}"),
            }
            std::fs::read(&out_path).unwrap_or_else(|e| panic!("task {i}: read {out_path:?}: {e}"))
        }));
    }

    // Note: per-task output path differs only by filename — the fake cc
    // produces bytes derived from the SOURCE path, not the output path,
    // so every task's bytes must match `expected` regardless of whether
    // it hit the cache or recompiled. A wrong-hit would surface as bytes
    // matching some other key's source.
    for (i, h) in handles.into_iter().enumerate() {
        let got = h.await.unwrap_or_else(|e| panic!("task {i} join: {e}"));
        assert_eq!(
            got, expected,
            "task {i}: content must match cold-miss output byte-for-byte (wrong-hit detected)"
        );
    }
}
