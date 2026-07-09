//! `Request::SessionStart` handling, session log writing, and per-session
//! stats tracking.

use super::*;

pub(super) struct SessionStartArgs<'a> {
    pub(super) client_pid: u32,
    pub(super) working_dir: &'a Path,
    pub(super) log_file: Option<NormalizedPath>,
    pub(super) track_stats: bool,
    pub(super) journal_path: Option<NormalizedPath>,
    pub(super) profile: bool,
    pub(super) private_daemon: Option<crate::protocol::PrivateDaemonSessionOptions>,
}

/// Handle a SessionStart request: create session, watch working directory.
pub(super) async fn handle_session_start(
    state: &SharedState,
    args: SessionStartArgs<'_>,
) -> Response {
    let (private_env, owner_pids) = match args.private_daemon {
        Some(options) => {
            if let Some(endpoint) = options.endpoint.as_deref() {
                if endpoint != state.endpoint {
                    return Response::Error {
                        message: format!(
                            "private daemon endpoint mismatch: connected to {}, requested {endpoint}",
                            state.endpoint
                        ),
                    };
                }
            }
            if let Some(cache_dir) = options.cache_dir.as_ref() {
                let requested_effective =
                    crate::core::config::effective_cache_root_from_top_level(cache_dir);
                if requested_effective != state.cache_dir {
                    return Response::Error {
                        message: format!(
                            "private daemon cache dir mismatch: connected effective root {}, requested root {} (effective {})",
                            state.cache_dir.display(),
                            cache_dir.display(),
                            requested_effective.display()
                        ),
                    };
                }
            }
            let owner_pids = if options.owner_pids.is_empty() {
                vec![args.client_pid]
            } else {
                options.owner_pids
            };
            state
                .private_daemon
                .register_session(&owner_pids, &options.env)
                .await;
            (options.env, owner_pids)
        }
        None => (Vec::new(), Vec::new()),
    };

    let session_config = crate::depgraph::SessionConfig {
        client_pid: args.client_pid,
        working_dir: args.working_dir.into(),
        log_file: args.log_file,
        track_stats: args.track_stats,
        journal_path: args.journal_path,
        profile: args.profile,
        private_env,
        owner_pids,
    };

    let session_id = state.sessions.create(session_config);
    state.ended_sessions.remove(&session_id);
    state.session_worktree_roots.insert(
        session_id,
        SessionWorktreeRoot {
            working_dir: args.working_dir.into(),
            root: resolve_worktree_root(args.working_dir, None),
        },
    );

    // Mirror any depgraph load-time warning into this session's log so
    // the cold fallback after a version-mismatch / corrupt depgraph.bin
    // is visible to operators reading `last-session.log`. Issue #320.
    {
        let warning_opt = {
            let guard = state
                .depgraph_load_warning
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            guard.clone()
        };
        if let Some(warning) = warning_opt {
            write_session_log(&state.sessions, &session_id, &warning);
        }
    }

    // Watch the working directory for file changes.
    watch_directory(state, args.working_dir).await;

    let journal_path = state
        .sessions
        .get(&session_id)
        .and_then(|s| s.journal_path.clone());

    Response::SessionStarted {
        session_id: session_id.to_string(),
        journal_path,
    }
}

pub(super) fn merge_session_private_env(
    sessions: &SessionManager,
    session_id: &SessionId,
    client_env: Option<Vec<(String, String)>>,
) -> Option<Vec<(String, String)>> {
    let Some(session) = sessions.get(session_id) else {
        return client_env;
    };
    if session.private_env.is_empty() {
        return client_env;
    }

    let mut merged = client_env.unwrap_or_default();
    for (private_key, private_value) in session.private_env {
        if let Some((_, value)) = merged.iter_mut().find(|(key, _)| key == &private_key) {
            *value = private_value;
        } else {
            merged.push((private_key, private_value));
        }
    }
    Some(merged)
}

pub(super) fn redact_session_private_env_for_journal(
    sessions: &SessionManager,
    session_id: &SessionId,
    env: &Option<Vec<(String, String)>>,
) -> Option<Vec<(String, String)>> {
    let Some(session) = sessions.get(session_id) else {
        return env.clone();
    };
    if session.private_env.is_empty() {
        return env.clone();
    }
    let private_keys: std::collections::HashSet<String> = session
        .private_env
        .iter()
        .map(|(key, _)| key.clone())
        .collect();
    env.as_ref().map(|vars| {
        vars.iter()
            .map(|(key, value)| {
                if private_keys.contains(key) {
                    (key.clone(), "<redacted>".to_string())
                } else {
                    (key.clone(), value.clone())
                }
            })
            .collect()
    })
}

/// Apply a mutation to the session's stats tracker (if tracking is enabled).
pub(super) fn record_session_stat(
    sessions: &SessionManager,
    session_id: &SessionId,
    f: impl FnOnce(&mut crate::depgraph::SessionStatsTracker),
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn merge_session_private_env_overlays_client_env() {
        let sessions = SessionManager::new(Duration::from_secs(300));
        let session_id = sessions.create(crate::depgraph::SessionConfig {
            client_pid: 1,
            working_dir: "/tmp/work".into(),
            log_file: None,
            track_stats: false,
            journal_path: None,
            profile: false,
            private_env: vec![
                ("ZCCACHE_PATH_REMAP".to_string(), "auto".to_string()),
                ("ZCCACHE_PRIVATE_ONLY".to_string(), "1".to_string()),
            ],
            owner_pids: vec![1],
        });

        let merged = merge_session_private_env(
            &sessions,
            &session_id,
            Some(vec![
                ("ZCCACHE_PATH_REMAP".to_string(), "off".to_string()),
                ("OTHER".to_string(), "value".to_string()),
            ]),
        )
        .unwrap();

        assert!(merged.contains(&("ZCCACHE_PATH_REMAP".to_string(), "auto".to_string())));
        assert!(merged.contains(&("ZCCACHE_PRIVATE_ONLY".to_string(), "1".to_string())));
        assert!(merged.contains(&("OTHER".to_string(), "value".to_string())));
        assert!(!merged.contains(&("ZCCACHE_PATH_REMAP".to_string(), "off".to_string())));
    }

    #[test]
    fn redact_session_private_env_for_journal_hides_values() {
        let sessions = SessionManager::new(Duration::from_secs(300));
        let session_id = sessions.create(crate::depgraph::SessionConfig {
            client_pid: 1,
            working_dir: "/tmp/work".into(),
            log_file: None,
            track_stats: false,
            journal_path: None,
            profile: false,
            private_env: vec![("ZCCACHE_PRIVATE".to_string(), "secret".to_string())],
            owner_pids: vec![1],
        });

        let redacted = redact_session_private_env_for_journal(
            &sessions,
            &session_id,
            &Some(vec![
                ("ZCCACHE_PRIVATE".to_string(), "secret".to_string()),
                ("OTHER".to_string(), "value".to_string()),
            ]),
        )
        .unwrap();

        assert!(redacted.contains(&("ZCCACHE_PRIVATE".to_string(), "<redacted>".to_string())));
        assert!(redacted.contains(&("OTHER".to_string(), "value".to_string())));
        assert!(!redacted.contains(&("ZCCACHE_PRIVATE".to_string(), "secret".to_string())));
    }
}
