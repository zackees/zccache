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
//! - Loom + thread-sanitizer harness (separate test target)
//!
//! Notify-timeout fall-through (the prior item on this list) landed
//! once the `PendingCacheWrite` registry scaffold went in — see
//! `daemon/server/pending_writes.rs` for the unit-level test and
//! `tests/pending_cache_writes.rs` for the `SharedState`-integration
//! variant.

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

/// After a source is modified, the next compile must reflect the NEW content,
/// even if a deferred-write task for the OLD content is still in flight.
///
/// This guards the worst-case shape of a defer-vs-invalidation race: the
/// daemon takes a cold-miss for foo.c@v1, returns to the wrapper, and starts
/// publishing the artifact in the background. Before the publish lands, the
/// source is edited (foo.c@v2). A subsequent lookup must either (a) miss
/// because v1's publish hasn't completed and v2's content hash differs from
/// v1's cache key (so v1's entry, even if visible, would not match v2's
/// lookup key), or (b) hit v2 after v2 is compiled and published. In no
/// circumstance may the lookup return v1's bytes when v2 is the source.
///
/// The fake cc shim emits source-content-derived bytes via the source path,
/// but the cache key is derived from source CONTENT hash. Editing the source
/// changes the content hash → new cache key → no collision with the v1
/// entry. So the v1 artifact, even if still published in the background,
/// is unreachable via the v2 lookup key.
#[tokio::test]
async fn source_edit_invalidates_cached_artifact() {
    let tmp = tempfile::tempdir().unwrap();
    let _guard = CacheDirEnvGuard::set(&tmp.path().join("zccache-cache"));
    let server = DaemonServer::bind(&crate::ipc::unique_test_endpoint()).unwrap();

    // Custom fake cc that echoes source CONTENT into the output, so we can
    // verify the cached bytes reflect the current source content. The
    // default `write_fake_cc` echoes the source PATH; here we want content.
    let cc = tmp.path().join("cc-echo-content");
    std::fs::write(
        &cc,
        r#"#!/bin/sh
src=
out=
while [ "$#" -gt 0 ]; do
    case "$1" in
        -c) shift; src=$1 ;;
        -o) shift; out=$1 ;;
    esac
    shift || true
done
if [ -z "$src" ] || [ -z "$out" ]; then exit 2; fi
# Echo the SOURCE CONTENT into the object — distinct source contents
# produce distinct object bytes.
printf 'src-content:' > "$out"
cat "$src" >> "$out"
"#,
    )
    .unwrap();
    let mut perms = std::fs::metadata(&cc).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&cc, perms).unwrap();

    let src = tmp.path().join("evolving.c");
    let out = tmp.path().join("evolving.o");
    let args = vec![
        "-c".to_string(),
        src.to_string_lossy().into_owned(),
        "-o".to_string(),
        out.to_string_lossy().into_owned(),
    ];

    // v1: write source, compile, capture bytes.
    std::fs::write(&src, b"int v(void) { return 1; }\n").unwrap();
    let v1_resp = handle_compile_ephemeral(
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
    match v1_resp {
        Response::CompileResult { exit_code, .. } => assert_eq!(exit_code, 0),
        other => panic!("expected CompileResult, got {other:?}"),
    }
    let v1_bytes = std::fs::read(&out).expect("v1 output");
    assert!(
        v1_bytes.starts_with(b"src-content:int v(void) { return 1; }"),
        "v1 bytes must encode v1 source content, got: {:?}",
        String::from_utf8_lossy(&v1_bytes[..v1_bytes.len().min(80)])
    );

    // Modify source. Bump mtime to defeat any stat-based fast paths.
    std::fs::write(&src, b"int v(void) { return 2; }\n").unwrap();
    let later = filetime::FileTime::from_unix_time(filetime::FileTime::now().unix_seconds() + 5, 0);
    filetime::set_file_mtime(&src, later).expect("set mtime forward");

    std::fs::remove_file(&out).ok();
    let v2_resp = handle_compile_ephemeral(
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
    match v2_resp {
        Response::CompileResult { exit_code, .. } => assert_eq!(exit_code, 0),
        other => panic!("expected CompileResult, got {other:?}"),
    }
    let v2_bytes = std::fs::read(&out).expect("v2 output");

    // Critical invariant: the v2 compile must reflect v2's source content,
    // not v1's. Whether the daemon's defer for v1 is still in flight or not,
    // the v2 lookup key is different (source-hash-based) so v1's entry
    // cannot satisfy v2.
    assert!(
        v2_bytes.starts_with(b"src-content:int v(void) { return 2; }"),
        "v2 bytes must encode v2 source content (cache invalidation broken), got: {:?}",
        String::from_utf8_lossy(&v2_bytes[..v2_bytes.len().min(80)])
    );
    assert_ne!(
        v1_bytes, v2_bytes,
        "v1 and v2 bytes must differ — source edit between compiles must invalidate the cache key"
    );
}

/// Daemon crash mid-flight must never surface wrong content to a post-restart
/// lookup. The lookup either hits with correct content (artifact + index were
/// already durable before the crash) or misses and recompiles to the same
/// deterministic bytes (artifact and/or index were lost). It never returns
/// content from a different cache key.
///
/// This is the canonical crash-recovery invariant from #610: under the
/// deferred-write path, the response leaves the daemon BEFORE the in-memory
/// cache becomes visible and BEFORE the WAL flush. If a crash lands inside
/// that window, the next daemon's `load_all()` from the on-disk index plus
/// content-addressed artifact directory must recover any committed entry,
/// and reject any uncommitted one without surfacing wrong content.
///
/// The test simulates the crash by **dropping** the `DaemonServer` (no
/// graceful shutdown — no shutdown notify, no final WAL flush, no in-flight
/// task drain) and binding a fresh server to the same cache root. Both
/// daemons are explicitly bound to the same cache root, so the second daemon
/// reads whatever the first managed to persist before the abrupt drop.
#[tokio::test]
async fn crash_mid_flight_recovery_never_surfaces_wrong_content() {
    let tmp = tempfile::tempdir().unwrap();
    let cache_root = tmp.path().join("zccache-cache");
    let cc = write_fake_cc(tmp.path());
    let src = tmp.path().join("crashy.c");
    std::fs::write(&src, b"int crashy(void) { return 9; }\n").unwrap();
    let out = tmp.path().join("crashy.o");
    let args = vec![
        "-c".to_string(),
        src.to_string_lossy().into_owned(),
        "-o".to_string(),
        out.to_string_lossy().into_owned(),
    ];

    // First daemon: do the cold-miss compile and capture the canonical bytes
    // for `crashy.c`. The fake cc shim derives bytes from the source path —
    // so any post-restart lookup that returns different bytes would be
    // surfacing content from a different cache key, the wrong-hit we must
    // never see.
    let expected = {
        let server = DaemonServer::bind_with_cache_dir(
            &crate::ipc::unique_test_endpoint(),
            &cache_root.clone().into(),
        )
        .unwrap();
        let resp = handle_compile_ephemeral(
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
        match resp {
            Response::CompileResult { exit_code, .. } => assert_eq!(exit_code, 0),
            other => panic!("expected CompileResult on cold path, got {other:?}"),
        }
        std::fs::read(&out).expect("cold output present")
        // `server` drops here — no graceful shutdown, no final WAL flush.
        // Any uncommitted entries are lost on purpose. This is the "crash".
    };

    // Drop the cold-miss output file so any post-restart compile must
    // materialize fresh bytes (either from cache or by recompiling).
    std::fs::remove_file(&out).ok();

    // Second daemon: same cache root, fresh process state. `load_all()` may
    // or may not have seen the first daemon's writes depending on whether
    // the artifact persist + WAL flush completed before the drop.
    let server2 = DaemonServer::bind_with_cache_dir(
        &crate::ipc::unique_test_endpoint(),
        &cache_root.clone().into(),
    )
    .unwrap();
    let resp = handle_compile_ephemeral(
        &server2.state,
        std::process::id(),
        tmp.path(),
        &cc,
        &args,
        tmp.path(),
        None,
        Vec::new(),
    )
    .await;
    match resp {
        Response::CompileResult { exit_code, .. } => assert_eq!(exit_code, 0),
        other => panic!("expected CompileResult on post-crash path, got {other:?}"),
    }
    let got = std::fs::read(&out).expect("post-crash output present");

    // Core invariant: the post-restart compile, whether it hit recovered
    // cache state or recompiled from scratch, must produce content that
    // matches the original cold-miss bytes. A wrong-hit returning content
    // from a different cache key fails here.
    assert_eq!(
        got, expected,
        "post-restart content must match the original cold-miss bytes — \
         whether the cache was recovered or rebuilt is implementation \
         detail; the only invariant is correctness"
    );
}

/// Cross-key contention: N distinct cold-misses run concurrently and each must
/// receive its own bytes — never another key's bytes.
///
/// This is the cross-key counterpart to `concurrent_lookups_after_cold_miss_*`
/// (which races same-key lookups). Here every task seeds its own previously
/// unseen artifact key. The DashMap/redb/dep_graph state is therefore being
/// inserted into from N tasks at once with no shared keys.
///
/// Wrong-hit shapes this guards against once #610's deferred-write path lands:
/// - Pending-write registry keyed wrongly (e.g. by `compiler` instead of by
///   `(context, source-hash)`) — a lookup for key `K_Y` would block on or
///   reuse a registry entry left by `K_X`'s in-flight publish, then read
///   `K_X`'s artifact bytes.
/// - DashMap shard collision causing two distinct keys to share a shard lock
///   and one observer reading a half-written entry from the other.
///
/// The fake cc shim emits `printf 'object-for:%s\n' "$src"` — bytes encode the
/// source PATH, so cross-contamination surfaces as `object-for:<other-src>` in
/// the assert. The assertion is exact-bytes per task, not "any of the N".
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn distinct_cold_misses_never_cross_contaminate() {
    let tmp = tempfile::tempdir().unwrap();
    let _guard = CacheDirEnvGuard::set(&tmp.path().join("zccache-cache"));
    let server = DaemonServer::bind(&crate::ipc::unique_test_endpoint()).unwrap();
    let state = std::sync::Arc::clone(&server.state);
    let cc = write_fake_cc(tmp.path());

    // Per-task: distinct source file, distinct (deterministic) source bytes,
    // distinct output path. Source content varies so the daemon's content-hash
    // cache key varies; source path varies so the fake cc's emitted bytes vary
    // and any cross-contamination is observable.
    const N: usize = 10;
    let inputs: Vec<(PathBuf, PathBuf)> = (0..N)
        .map(|i| {
            let src = tmp.path().join(format!("cross_{i}.c"));
            let out = tmp.path().join(format!("cross_{i}.o"));
            std::fs::write(&src, format!("int cross_{i}(void) {{ return {i}; }}\n")).unwrap();
            (src, out)
        })
        .collect();

    // Pre-compute the expected bytes per task — exactly what the fake cc emits
    // for that source path. Computing this up-front (no cache involvement)
    // pins the invariant: every task's output MUST equal this exact value.
    let expected: Vec<Vec<u8>> = inputs
        .iter()
        .map(|(src, _)| format!("object-for:{}\n", src.display()).into_bytes())
        .collect();

    // Spawn N concurrent cold-misses with distinct keys. Each is the first
    // time the daemon has seen its cache key, so all N race through the
    // miss-store path simultaneously.
    let started = std::time::Instant::now();
    let mut handles = Vec::with_capacity(N);
    for (i, (src, out)) in inputs.iter().enumerate() {
        let state = std::sync::Arc::clone(&state);
        let cc = cc.clone();
        let cwd = tmp.path().to_path_buf();
        let src = src.clone();
        let out = out.clone();
        let args = vec![
            "-c".to_string(),
            src.to_string_lossy().into_owned(),
            "-o".to_string(),
            out.to_string_lossy().into_owned(),
        ];
        handles.push(tokio::spawn(async move {
            let resp = handle_compile_ephemeral(
                &state,
                std::process::id(),
                &cwd,
                &cc,
                &args,
                &cwd,
                None,
                Vec::new(),
            )
            .await;
            match resp {
                Response::CompileResult { exit_code, .. } => {
                    assert_eq!(exit_code, 0, "task {i} cold-miss must succeed");
                }
                other => panic!("task {i}: expected CompileResult, got {other:?}"),
            }
            std::fs::read(&out).unwrap_or_else(|e| panic!("task {i}: read {out:?}: {e}"))
        }));
    }

    let mut results: Vec<Vec<u8>> = Vec::with_capacity(N);
    for (i, h) in handles.into_iter().enumerate() {
        results.push(h.await.unwrap_or_else(|e| panic!("task {i} join: {e}")));
    }

    // Liveness floor — no task may block forever waiting on another task's
    // shard lock or pending-write entry. Generous bound; the assertion exists
    // to catch deadlock under the deferred-write path, not to gate perf.
    let elapsed = started.elapsed();
    assert!(
        elapsed < std::time::Duration::from_secs(30),
        "concurrent cold-misses took {elapsed:?} — possible deadlock between tasks"
    );

    // Wrong-hit invariant: every task's bytes MUST equal that task's expected
    // bytes. Cross-contamination would surface as `object-for:<other-src>`.
    for (i, got) in results.iter().enumerate() {
        assert_eq!(
            *got, expected[i],
            "task {i}: content must encode this task's source path, not another's — \
             cross-key wrong-hit detected (saw bytes derived from a different cache key)"
        );
    }
}
