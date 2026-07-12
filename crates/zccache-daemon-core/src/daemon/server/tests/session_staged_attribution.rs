//! Cross-session isolation for staged pipeline telemetry (#1071).

use super::super::*;
use crate::protocol::StagedProfileSummary;

#[cfg(unix)]
type TestClientConnection = crate::ipc::IpcConnection;
#[cfg(windows)]
type TestClientConnection = crate::ipc::IpcClientConnection;

async fn start_tracked_session(client: &mut TestClientConnection, cwd: &Path) -> SessionId {
    client
        .send(&Request::SessionStart {
            client_pid: std::process::id(),
            working_dir: cwd.into(),
            log_file: None,
            track_stats: true,
            journal_path: None,
            profile: false,
            private_daemon: None,
        })
        .await
        .unwrap();
    match client.recv().await.unwrap() {
        Some(Response::SessionStarted { session_id, .. }) => session_id.parse().unwrap(),
        other => panic!("expected SessionStarted, got {other:?}"),
    }
}

async fn session_staged(
    client: &mut TestClientConnection,
    session_id: SessionId,
) -> StagedProfileSummary {
    client
        .send(&Request::SessionStats {
            session_id: session_id.to_string(),
        })
        .await
        .unwrap();
    match client.recv().await.unwrap() {
        Some(Response::SessionStatsResult { stats: Some(stats) }) => {
            stats.phase_profile.unwrap().staged
        }
        other => panic!("expected SessionStatsResult, got {other:?}"),
    }
}

#[tokio::test]
async fn concurrent_failed_requests_are_attributed_only_to_their_session() {
    crate::test_support::test_timeout(async {
        use crate::daemon::staged_stats::{
            scope_request_profile, StagedCounter, StagedFailure, StagedTiming,
        };

        let temp = tempfile::tempdir().unwrap();
        let endpoint = crate::ipc::unique_test_endpoint();
        let mut server = DaemonServer::bind(&endpoint).unwrap();
        let state = server.test_state_arc();
        let shutdown = server.shutdown_handle();
        let server_task = tokio::spawn(async move { server.run(0).await.unwrap() });
        let mut first_client = crate::ipc::connect(&endpoint).await.unwrap();
        let mut second_client = crate::ipc::connect(&endpoint).await.unwrap();
        let first_id = start_tracked_session(&mut first_client, temp.path()).await;
        let second_id = start_tracked_session(&mut second_client, temp.path()).await;
        let first_profile = Arc::clone(
            state
                .session_staged_profiles
                .get(&first_id)
                .unwrap()
                .value(),
        );
        let second_profile = Arc::clone(
            state
                .session_staged_profiles
                .get(&second_id)
                .unwrap()
                .value(),
        );
        let barrier = Arc::new(tokio::sync::Barrier::new(3));

        let first_task = {
            let state = Arc::clone(&state);
            let barrier = Arc::clone(&barrier);
            tokio::spawn(scope_request_profile(first_profile, async move {
                barrier.wait().await;
                state
                    .profiler
                    .staged
                    .count(StagedCounter::PublicationFailure);
                state.profiler.staged.failure(StagedFailure::PointerCommit);
                state.profiler.staged.timing(StagedTiming::Publication, 7);
            }))
        };
        let second_task = {
            let state = Arc::clone(&state);
            let barrier = Arc::clone(&barrier);
            tokio::spawn(scope_request_profile(second_profile, async move {
                barrier.wait().await;
                state
                    .profiler
                    .staged
                    .count(StagedCounter::MaterializeFailure);
                state
                    .profiler
                    .staged
                    .failure(StagedFailure::RequestedMaterialization);
                state
                    .profiler
                    .staged
                    .timing(StagedTiming::MissMaterialization, 11);
            }))
        };

        barrier.wait().await;
        first_task.await.unwrap();
        second_task.await.unwrap();

        // Unscoped work models an ephemeral link/exec request. It belongs only
        // to daemon totals and must not appear in either tracked session.
        state.profiler.staged.count(StagedCounter::PlanError);
        state.profiler.staged.failure(StagedFailure::Planning);

        let first = session_staged(&mut first_client, first_id).await;
        assert_eq!(first.counters["publication_failure"], 1);
        assert_eq!(first.counters["materialize_failure"], 0);
        assert_eq!(first.counters["plan_error"], 0);
        assert_eq!(first.failures["pointer_commit"], 1);
        assert_eq!(first.failures["requested_materialization"], 0);
        assert_eq!(first.timings_ns["publication"], 7);
        assert_eq!(first.timings_ns["miss_materialization"], 0);

        let second = session_staged(&mut second_client, second_id).await;
        assert_eq!(second.counters["publication_failure"], 0);
        assert_eq!(second.counters["materialize_failure"], 1);
        assert_eq!(second.counters["plan_error"], 0);
        assert_eq!(second.failures["pointer_commit"], 0);
        assert_eq!(second.failures["requested_materialization"], 1);
        assert_eq!(second.timings_ns["publication"], 0);
        assert_eq!(second.timings_ns["miss_materialization"], 11);

        let aggregate = state.profiler.staged.snapshot();
        assert_eq!(aggregate.counters["publication_failure"], 1);
        assert_eq!(aggregate.counters["materialize_failure"], 1);
        assert_eq!(aggregate.counters["plan_error"], 1);

        first_client
            .send(&Request::SessionEnd {
                session_id: first_id.to_string(),
            })
            .await
            .unwrap();
        match first_client.recv().await.unwrap() {
            Some(Response::SessionEnded { stats: Some(stats) }) => {
                let ended = stats.phase_profile.unwrap().staged;
                assert_eq!(ended.counters["publication_failure"], 1);
                assert_eq!(ended.counters["materialize_failure"], 0);
                assert_eq!(ended.counters["plan_error"], 0);
            }
            other => panic!("expected SessionEnded stats, got {other:?}"),
        }
        assert!(!state.session_staged_profiles.contains_key(&first_id));

        first_client.send(&Request::Clear).await.unwrap();
        assert!(matches!(
            first_client.recv().await.unwrap(),
            Some(Response::Cleared { .. })
        ));
        let second = session_staged(&mut second_client, second_id).await;
        assert!(second
            .counters
            .values()
            .chain(second.timings_ns.values())
            .chain(second.bytes.values())
            .chain(second.failures.values())
            .all(|value| *value == 0));

        shutdown.notify_one();
        server_task.await.unwrap();
    })
    .await;
}
