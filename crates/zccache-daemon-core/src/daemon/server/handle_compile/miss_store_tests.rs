//! Focused regressions for compile-miss artifact publication.

use super::{
    commit_rustc_artifact_index, enqueue_persisted_index, preserve_staged_depfile_for_persistence,
    remove_provisional_artifact, store_rustc_outputs, truncate_staged_publication_error,
    MissArtifactStoreStats, PersistOutcome, MAX_STAGED_PUBLICATION_ERROR_CHARS,
};
use crate::core::NormalizedPath;
use crate::daemon::server::compile_resource_gate::CompileResourceGate;
use crate::daemon::server::*;
use std::sync::Arc;
use std::time::{Duration, Instant};

#[tokio::test]
async fn rustc_index_commit_merges_verdicts_for_shared_artifact_bytes() {
    let temp = tempfile::tempdir().unwrap();
    let server = crate::daemon::server::tests::bind_isolated_server(temp.path());
    let mut meta = ArtifactIndex::new(vec![], vec![], vec![], vec![], 0);
    for key in ["plain", "dylint"] {
        meta.rustc_verdicts.insert(
            key.to_string(),
            ArtifactVerdict {
                stdout: Arc::new(Vec::new()),
                stderr: Arc::new(key.as_bytes().to_vec()),
                exit_code: 0,
            },
        );
        commit_rustc_artifact_index(&server.state, "artifact".to_string(), meta, false).unwrap();
        meta = ArtifactIndex::new(vec![], vec![], vec![], vec![], 0);
    }
    let cached = server.state.artifacts.get("artifact").unwrap();
    assert_eq!(cached.meta.rustc_verdicts.len(), 2);
}

#[tokio::test]
async fn perf_staged_rust_hit_uses_provisional_payload_before_durable_publication() {
    let temp = tempfile::tempdir().unwrap();
    let server = crate::daemon::server::tests::bind_isolated_server(temp.path());
    let state = Arc::clone(&server.state);
    let work = tempfile::tempdir().unwrap();
    let permits = state.persist_semaphore.available_permits() as u32;
    assert!(permits > 0, "test server must expose a persist permit");
    let blocked_publisher = Arc::clone(&state.persist_semaphore)
        .acquire_many_owned(permits)
        .await
        .unwrap();
    let private_root: NormalizedPath = work.path().join("private-rust-output").into();
    std::fs::create_dir_all(&private_root).unwrap();
    let staged: NormalizedPath = private_root.join("libworkspace.a");
    let requested: NormalizedPath = work.path().join("libworkspace.a").into();
    let expected = b"workspace staticlib";
    std::fs::write(&staged, expected).unwrap();
    let plan = StagedCompilePlan::for_test(
        private_root,
        vec![StagedOutputPlan {
            requested: requested.clone(),
            staged: staged.clone(),
            role: StagedOutputRole::Regular,
        }],
    );
    let outputs = vec![RustcOutputFile {
        name: "libworkspace.a".to_string(),
        path: staged.clone(),
        size: expected.len() as u64,
    }];
    let key = "8".repeat(64);
    let sid = crate::depgraph::SessionId::new();
    let source_path: NormalizedPath = work.path().join("lib.rs").into();
    let mut stats = MissArtifactStoreStats::default();
    let publication_guard = begin_artifact_publication(&state).await.unwrap();
    let resource_gate = CompileResourceGate::default();
    let resource_admission = resource_gate.acquire(false).await;

    store_rustc_outputs(
        &state,
        &sid,
        &source_path,
        &outputs,
        &key,
        None,
        &Arc::new(Vec::new()),
        &Arc::new(Vec::new()),
        0,
        Instant::now(),
        &mut stats,
        Instant::now(),
        false,
        Some(plan),
        publication_guard,
        resource_admission,
    );
    let exclusive_gate = resource_gate.clone();
    let mut exclusive = tokio::spawn(async move { exclusive_gate.acquire(true).await });
    assert!(
        tokio::time::timeout(Duration::from_millis(50), &mut exclusive)
            .await
            .is_err(),
        "exclusive compiler must wait while detached shared publication is blocked"
    );
    assert!(
        staged.is_file(),
        "publishing the provisional entry must retain its private source"
    );

    let verdict_key = crate::depgraph::compute_rustc_verdict_key(&key, None)
        .hash()
        .to_hex();
    let requested_outputs = vec![requested.clone()];
    let started = Instant::now();
    super::super::hit_branches::await_artifact_publication_if_needed(
        &state,
        &key,
        Some(&verdict_key),
        Some(&requested_outputs),
    )
    .await;
    assert!(
        staged.is_file(),
        "bypassing the pending wait must not release the private source"
    );
    let response = super::super::cached_hit::materialize_cached_compile_hit(
        super::super::cached_hit::CachedHitMaterializeRequest {
            state: &state,
            sid: &sid,
            artifact_key_hex: &key,
            verdict_key_hex: Some(&verdict_key),
            source_path: &source_path,
            output_path: &requested,
            secondary_output_dir: work.path().into(),
            current_depfile_dest: None,
            compile_start: Instant::now(),
            hit_label: "HIT_TEST",
            cached_error_label: "CACHED_ERROR_TEST",
            record_compilation: false,
            downgrade_output_metadata: true,
            mtime_floor_paths: Vec::new(),
            // Exercise the explicit emit-compat mapping used by the pipeline's
            // alternate rustc context hit branch while publication is blocked.
            rustc_metadata_compat_outputs: Some(requested_outputs),
            rustc_archive_hardlink_eligible: Some(false),
            phases: super::super::cached_hit::CachedHitPhases::request_cache(0, 0),
        },
    )
    .unwrap();
    assert!(
        started.elapsed() < Duration::from_millis(500),
        "a proven hit must not wait for the blocked durable publisher"
    );
    assert!(matches!(
        response,
        Response::CompileResult {
            exit_code: 0,
            cached: true,
            ..
        }
    ));
    assert_eq!(std::fs::read(&requested).unwrap(), expected);
    drop(blocked_publisher);
    assert!(
        pending_writes::await_all(&state.pending_cache_writes, Duration::from_secs(10)).await,
        "detached publisher did not complete after its permit was released"
    );
    tokio::time::timeout(Duration::from_secs(1), exclusive)
        .await
        .expect("exclusive compiler should acquire after detached publication")
        .expect("exclusive task");
}

#[tokio::test]
async fn failed_older_publisher_does_not_remove_replacement_artifact() {
    let temp = tempfile::tempdir().unwrap();
    let server = crate::daemon::server::tests::bind_isolated_server(temp.path());
    let artifact = |byte| {
        CachedArtifact::from_cached_payloads(
            ArtifactIndex::new(
                vec!["output.rmeta".to_string()],
                vec![1],
                Arc::new(Vec::new()),
                Arc::new(Vec::new()),
                0,
            ),
            vec![CachedPayload::Bytes(Arc::new(vec![byte]))],
        )
    };
    let key = "replacement-key";
    let provisional = artifact(1);
    let replacement = artifact(2);
    server
        .state
        .artifacts
        .insert(key.to_string(), replacement.clone());

    remove_provisional_artifact(&server.state, key, &provisional);

    let live = server.state.artifacts.get(key).unwrap();
    assert!(
        live.same_instance(&replacement),
        "a stale publisher failure must preserve the newer artifact"
    );
}

#[tokio::test]
async fn visible_artifact_waits_for_pending_sibling_verdict() {
    let temp = tempfile::tempdir().unwrap();
    let server = crate::daemon::server::tests::bind_isolated_server(temp.path());
    let state = Arc::clone(&server.state);
    let key = "shared-rust-artifact";
    let plain_verdict_key = "plain-verdict";
    let requested_verdict_key = "dylint-verdict";
    let requested_output: NormalizedPath = temp.path().join("output.rmeta").into();
    let requested_outputs = vec![requested_output];
    let verdict = |stderr: &[u8]| ArtifactVerdict {
        stdout: Arc::new(Vec::new()),
        stderr: Arc::new(stderr.to_vec()),
        exit_code: 0,
    };
    let artifact = |include_requested_verdict| {
        let mut meta = ArtifactIndex::new(
            vec!["output.rmeta".to_string()],
            vec![1],
            Arc::new(Vec::new()),
            Arc::new(Vec::new()),
            0,
        );
        meta.rustc_verdicts
            .insert(plain_verdict_key.to_string(), verdict(b"plain"));
        if include_requested_verdict {
            meta.rustc_verdicts
                .insert(requested_verdict_key.to_string(), verdict(b"dylint"));
        }
        CachedArtifact::from_cached_payloads(meta, vec![CachedPayload::Bytes(Arc::new(vec![1]))])
    };
    state.artifacts.insert(key.to_string(), artifact(false));
    let _plain_publisher = pending_writes::register(&state.pending_cache_writes, key);
    let _dylint_publisher = pending_writes::register(&state.pending_cache_writes, key);

    let state_for_waiter = Arc::clone(&state);
    let requested_outputs_for_waiter = requested_outputs.clone();
    let mut waiter = tokio::spawn(async move {
        super::super::hit_branches::await_artifact_publication_if_needed(
            &state_for_waiter,
            key,
            Some(requested_verdict_key),
            Some(&requested_outputs_for_waiter),
        )
        .await;
    });
    assert!(
        tokio::time::timeout(Duration::from_millis(20), &mut waiter)
            .await
            .is_err(),
        "an occupied row missing the requested sibling verdict must wait"
    );

    pending_writes::complete(&state.pending_cache_writes, key);
    assert!(
        tokio::time::timeout(Duration::from_millis(20), &mut waiter)
            .await
            .is_err(),
        "an unrelated publisher completing first must not end the wait"
    );

    state.artifacts.insert(key.to_string(), artifact(true));
    pending_writes::complete(&state.pending_cache_writes, key);
    tokio::time::timeout(Duration::from_secs(1), waiter)
        .await
        .expect("request-specific wait timed out")
        .expect("request-specific wait task panicked");
    assert!(
        super::super::hit_branches::artifact_ready_for_request(
            &state,
            key,
            Some(requested_verdict_key),
            Some(&requested_outputs),
        ),
        "the merged sibling verdict and output must satisfy the request"
    );
}

#[tokio::test]
async fn visible_metadata_waits_for_pending_payload_replacement() {
    let temp = tempfile::tempdir().unwrap();
    let server = crate::daemon::server::tests::bind_isolated_server(temp.path());
    let state = Arc::clone(&server.state);
    let key = "stale-rust-artifact";
    let verdict_key = "rustc-verdict";
    let requested_output: NormalizedPath = temp.path().join("output.rmeta").into();
    let requested_outputs = vec![requested_output];
    let metadata = || {
        let mut meta = ArtifactIndex::new(
            vec!["output.rmeta".to_string()],
            vec![1],
            Arc::new(Vec::new()),
            Arc::new(Vec::new()),
            0,
        );
        meta.rustc_verdicts.insert(
            verdict_key.to_string(),
            ArtifactVerdict {
                stdout: Arc::new(Vec::new()),
                stderr: Arc::new(Vec::new()),
                exit_code: 0,
            },
        );
        meta
    };
    state
        .artifacts
        .insert(key.to_string(), CachedArtifact::from_index(metadata()));
    let _publisher = pending_writes::register(&state.pending_cache_writes, key);

    let state_for_waiter = Arc::clone(&state);
    let requested_outputs_for_waiter = requested_outputs.clone();
    let mut waiter = tokio::spawn(async move {
        super::super::hit_branches::await_artifact_publication_if_needed(
            &state_for_waiter,
            key,
            Some(verdict_key),
            Some(&requested_outputs_for_waiter),
        )
        .await;
    });
    assert!(
        tokio::time::timeout(Duration::from_millis(20), &mut waiter)
            .await
            .is_err(),
        "visible metadata without a usable payload must wait for replacement"
    );

    state.artifacts.insert(
        key.to_string(),
        CachedArtifact::from_cached_payloads(
            metadata(),
            vec![CachedPayload::Bytes(Arc::new(vec![1]))],
        ),
    );
    pending_writes::complete(&state.pending_cache_writes, key);
    tokio::time::timeout(Duration::from_secs(1), waiter)
        .await
        .expect("payload replacement wait timed out")
        .expect("payload replacement wait task panicked");
    assert!(super::super::hit_branches::artifact_ready_for_request(
        &state,
        key,
        Some(verdict_key),
        Some(&requested_outputs),
    ));
}

#[test]
fn failed_async_persist_never_enqueues_an_index_insert() {
    let (index_tx, mut index_rx) = tokio::sync::mpsc::unbounded_channel();
    let queued = enqueue_persisted_index(
        PersistOutcome::failed(),
        &index_tx,
        "failed-persist-key".to_string(),
    );

    assert!(!queued, "a failed persist must not be indexed");
    assert!(
        index_rx.try_recv().is_err(),
        "failed persist must not send IndexWriterCommand::Insert"
    );
}

#[test]
fn publication_error_is_bounded_without_splitting_utf8() {
    let error = format!("{}suffix", "å".repeat(MAX_STAGED_PUBLICATION_ERROR_CHARS));
    let rendered = truncate_staged_publication_error(&error);

    assert!(rendered.ends_with('…'));
    assert_eq!(
        rendered.trim_end_matches('…').chars().count(),
        MAX_STAGED_PUBLICATION_ERROR_CHARS
    );
}

#[test]
fn staged_depfile_persistence_source_survives_plan_cleanup() {
    let temp = tempfile::tempdir().expect("tempdir");
    let private_root = temp.path().join("private");
    let staged_depfile: NormalizedPath = private_root.join("output-1").into();
    let requested_depfile: NormalizedPath = temp.path().join("build/custom.mk").into();
    std::fs::create_dir_all(&private_root).expect("private root");
    std::fs::write(&staged_depfile, b"staged object: logical source\n").expect("staged depfile");
    let mut capture = Some((staged_depfile, b"staged object: logical source\n".to_vec()));

    let guard = preserve_staged_depfile_for_persistence(
        &mut capture,
        Some(&requested_depfile),
        temp.path(),
    )
    .expect("preserve depfile")
    .expect("persistence guard");
    let preserved = capture.as_ref().expect("capture").0.clone();
    assert_eq!(
        preserved.file_name().and_then(|name| name.to_str()),
        Some("custom.mk")
    );

    std::fs::remove_dir_all(&private_root).expect("simulate staged-plan cleanup");
    assert_eq!(
        std::fs::read(&preserved).expect("preserved canonical bytes"),
        b"staged object: logical source\n"
    );
    drop(guard);
    assert!(!preserved.as_path().exists());
}
