//! `Request::SessionStart` handling, session log writing, and per-session
//! stats tracking.

use super::*;

/// Handle a SessionStart request: create session, watch working directory.
pub(super) async fn handle_session_start(
    state: &SharedState,
    client_pid: u32,
    working_dir: &Path,
    log_file: Option<NormalizedPath>,
    track_stats: bool,
    journal_path: Option<NormalizedPath>,
    profile: bool,
) -> Response {
    let session_config = zccache_monocrate::depgraph::SessionConfig {
        client_pid,
        working_dir: working_dir.into(),
        log_file,
        track_stats,
        journal_path,
        profile,
    };

    let session_id = state.sessions.create(session_config);
    state.session_worktree_roots.insert(
        session_id,
        SessionWorktreeRoot {
            working_dir: working_dir.into(),
            root: resolve_worktree_root(working_dir, None),
        },
    );

    // Mirror any depgraph load-time warning into this session's log so
    // the cold fallback after a version-mismatch / corrupt depgraph.bin
    // is visible to operators reading `last-session.log`. Issue #320.
    {
        let warning_opt = {
            let guard = state.depgraph_load_warning.lock().await;
            guard.clone()
        };
        if let Some(warning) = warning_opt {
            write_session_log(&state.sessions, &session_id, &warning);
        }
    }

    // Watch the working directory for file changes.
    watch_directory(state, working_dir).await;

    let journal_path = state
        .sessions
        .get(&session_id)
        .and_then(|s| s.journal_path.clone());

    Response::SessionStarted {
        session_id: session_id.to_string(),
        journal_path,
    }
}

/// Apply a mutation to the session's stats tracker (if tracking is enabled).
pub(super) fn record_session_stat(
    sessions: &SessionManager,
    session_id: &SessionId,
    f: impl FnOnce(&mut zccache_monocrate::depgraph::SessionStatsTracker),
) {
    sessions.mutate(session_id, |session| {
        if let Some(ref mut tracker) = session.stats_tracker {
            f(tracker);
        }
    });
}

/// Write a log line to the session's log file (if configured).
pub(super) fn write_session_log(sessions: &SessionManager, session_id: &SessionId, message: &str) {
    if let Some(session) = sessions.get(session_id) {
        if let Some(ref log_path) = session.log_file {
            use std::io::Write;
            if let Ok(mut f) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(log_path)
            {
                let _ = writeln!(f, "{message}");
            }
        }
    }
}
