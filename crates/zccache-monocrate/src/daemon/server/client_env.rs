//! Replay client environment variables into compiler child processes.
//!
//! When a client (e.g. `cc` invoked by `make`) is replayed by the daemon, we
//! clear the daemon's inherited env and substitute the client's vars — minus
//! a few that name process-local file descriptors. Lineage markers are then
//! layered on top so orphan trackers attribute the compiler to zccache.

use super::*;

/// Apply client environment variables to a compiler command, then overlay
/// spawn-lineage markers so orphan trackers can attribute the child to
/// zccache (see `super::super::lineage`).
///
/// If `client_env` is `Some`, the inherited env is cleared and replaced with
/// the client's vars. Lineage env vars are layered on top in either case so
/// the child always carries the chain.
pub(super) fn apply_client_env(
    cmd: &mut tokio::process::Command,
    client_env: &Option<Vec<(String, String)>>,
    lineage: &super::super::lineage::Lineage,
) {
    if let Some(vars) = client_env {
        cmd.env_clear();
        for (key, val) in vars {
            if client_env_var_is_safe_to_replay(key) {
                cmd.env(key, val);
            }
        }
    }
    lineage.apply_to_tokio(cmd, client_env.as_deref());
}

/// Cargo jobserver env vars name process-local file descriptors. The daemon
/// receives those names through IPC, not the fds themselves, so replaying them
/// into daemon-spawned compilers produces Cargo's stale-jobserver warning.
pub(super) fn client_env_var_is_safe_to_replay(key: &str) -> bool {
    !matches!(key, "MAKEFLAGS" | "CARGO_MAKEFLAGS")
}

/// Sync-command counterpart of [`apply_client_env`].
pub(super) fn apply_client_env_sync(
    cmd: &mut std::process::Command,
    client_env: Option<&[(String, String)]>,
    lineage: &super::super::lineage::Lineage,
) {
    if let Some(vars) = client_env {
        cmd.env_clear();
        for (key, val) in vars {
            if client_env_var_is_safe_to_replay(key) {
                cmd.env(key, val);
            }
        }
    }
    lineage.apply_to_sync(cmd, client_env);
}

/// Look up the client PID for a session. Returns `None` if the session is
/// unknown (already ended) — callers should still emit lineage with whatever
/// they know.
pub(super) fn session_client_pid(state: &SharedState, sid: &SessionId) -> Option<u32> {
    state.sessions.get(sid).map(|s| s.client_pid)
}
