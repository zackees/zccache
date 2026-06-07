//! Tests for `handle_release_worktree_handles` (issue #690).
//!
//! Drives the handler in-process via `DaemonServer::test_state()` — the same
//! seam `handle_clear`'s integration test uses. Avoids the IPC roundtrip so
//! we can observe `SessionManager` state directly after the call.

use super::super::*;
use super::CacheDirEnvGuard;
use crate::depgraph::SessionConfig;

fn session_under(worktree: &std::path::Path, journal: Option<&std::path::Path>) -> SessionConfig {
    SessionConfig {
        client_pid: std::process::id(),
        working_dir: NormalizedPath::from(worktree),
        log_file: None,
        track_stats: false,
        journal_path: journal.map(NormalizedPath::from),
        profile: false,
        private_env: Vec::new(),
        owner_pids: Vec::new(),
    }
}

/// Happy path: a session whose `working_dir` is under the released path is
/// dropped; a session under a *different* worktree is preserved. The dropped
/// session ID appears in the response, and counts match.
#[tokio::test]
#[ignore] // integration-level: instantiates a real DaemonServer
async fn release_drops_sessions_under_path_and_leaves_siblings() {
    crate::test_support::test_timeout(async {
        let cache_tmp = tempfile::tempdir().unwrap();
        let _env = CacheDirEnvGuard::set(cache_tmp.path());
        let endpoint = crate::ipc::unique_test_endpoint();
        let cache_dir = NormalizedPath::new(cache_tmp.path());
        let server = DaemonServer::bind_with_cache_dir(&endpoint, &cache_dir).unwrap();

        let worktree_a = tempfile::tempdir().unwrap();
        let worktree_b = tempfile::tempdir().unwrap();
        let inside_a = worktree_a.path().join("target/debug");
        std::fs::create_dir_all(&inside_a).unwrap();

        let state = server.test_state();
        let sid_root_a = state
            .sessions
            .create(session_under(worktree_a.path(), None));
        let sid_inside_a = state.sessions.create(session_under(&inside_a, None));
        let sid_b = state
            .sessions
            .create(session_under(worktree_b.path(), None));
        assert_eq!(state.sessions.active_count(), 3);

        let target = NormalizedPath::new(worktree_a.path());
        let resp = super::super::handle_release_worktree_handles::handle_release_worktree_handles(
            state, &target,
        )
        .await;

        let Response::ReleaseWorktreeHandlesResult {
            inspected,
            released,
            sessions_dropped,
            unreleased,
        } = resp
        else {
            panic!("expected ReleaseWorktreeHandlesResult, got: {resp:?}");
        };

        assert_eq!(inspected, 3, "should have looked at every active session");
        assert_eq!(released, 2, "both sessions under worktree_a must drop");
        assert!(
            unreleased.is_empty(),
            "no long-lived mmaps means nothing should fail to release; got {unreleased:?}"
        );
        assert_eq!(state.sessions.active_count(), 1, "sibling session survives");
        assert!(
            state.sessions.get(&sid_b).is_some(),
            "session under worktree_b must still exist"
        );
        assert!(
            state.sessions.get(&sid_root_a).is_none()
                && state.sessions.get(&sid_inside_a).is_none(),
            "sessions under worktree_a must be gone"
        );

        let dropped: std::collections::HashSet<String> = sessions_dropped.into_iter().collect();
        assert!(dropped.contains(&sid_root_a.to_string()));
        assert!(dropped.contains(&sid_inside_a.to_string()));
    })
    .await;
}

/// Safety guard: the daemon must refuse to release its own cache root.
/// soldr's worktree paths are always disjoint from the cache root, so a
/// well-behaved caller never hits this — but a buggy caller passing the
/// cache root would corrupt every concurrent session if we honored it.
#[tokio::test]
#[ignore] // integration-level: instantiates a real DaemonServer
async fn release_refuses_to_release_cache_root() {
    crate::test_support::test_timeout(async {
        let cache_tmp = tempfile::tempdir().unwrap();
        let _env = CacheDirEnvGuard::set(cache_tmp.path());
        let endpoint = crate::ipc::unique_test_endpoint();
        let cache_dir = NormalizedPath::new(cache_tmp.path());
        let server = DaemonServer::bind_with_cache_dir(&endpoint, &cache_dir).unwrap();
        let state = server.test_state();

        // Seed a session so the handler has something to potentially touch.
        let unrelated = tempfile::tempdir().unwrap();
        let sid = state.sessions.create(session_under(unrelated.path(), None));
        assert!(state.sessions.get(&sid).is_some());

        let resp = super::super::handle_release_worktree_handles::handle_release_worktree_handles(
            state, &cache_dir,
        )
        .await;

        assert!(
            matches!(resp, Response::Error { ref message } if message.contains("cache root")),
            "expected refusal naming the cache root, got: {resp:?}"
        );
        assert!(
            state.sessions.get(&sid).is_some(),
            "refusal must not have torn down any sessions"
        );
    })
    .await;
}

/// Empty case: the daemon has no sessions under the requested path. The
/// response counts are zero and no session state is disturbed.
#[tokio::test]
#[ignore] // integration-level: instantiates a real DaemonServer
async fn release_with_no_matches_returns_zero_counts() {
    crate::test_support::test_timeout(async {
        let cache_tmp = tempfile::tempdir().unwrap();
        let _env = CacheDirEnvGuard::set(cache_tmp.path());
        let endpoint = crate::ipc::unique_test_endpoint();
        let cache_dir = NormalizedPath::new(cache_tmp.path());
        let server = DaemonServer::bind_with_cache_dir(&endpoint, &cache_dir).unwrap();
        let state = server.test_state();

        let other_worktree = tempfile::tempdir().unwrap();
        let sid = state
            .sessions
            .create(session_under(other_worktree.path(), None));

        // Target a fresh, unrelated tmp dir with no sessions under it.
        let target_tmp = tempfile::tempdir().unwrap();
        let target = NormalizedPath::new(target_tmp.path());
        let resp = super::super::handle_release_worktree_handles::handle_release_worktree_handles(
            state, &target,
        )
        .await;

        let Response::ReleaseWorktreeHandlesResult {
            inspected,
            released,
            sessions_dropped,
            unreleased,
        } = resp
        else {
            panic!("expected ReleaseWorktreeHandlesResult, got: {resp:?}");
        };
        assert_eq!(inspected, 1);
        assert_eq!(released, 0);
        assert!(sessions_dropped.is_empty());
        assert!(unreleased.is_empty());
        assert!(state.sessions.get(&sid).is_some());
    })
    .await;
}
