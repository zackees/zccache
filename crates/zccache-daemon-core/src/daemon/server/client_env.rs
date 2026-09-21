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
pub(super) fn apply_client_env_builder(
    mut builder: kernal_api::SpawnSpec,
    client_env: &Option<Vec<(String, String)>>,
    lineage: &super::super::lineage::Lineage,
    is_dylint_driver: bool,
) -> kernal_api::SpawnSpec {
    if let Some(vars) = client_env {
        builder = builder.clear_env(true);
        for (key, val) in vars {
            if client_env_var_is_safe_to_replay(key) {
                builder = builder.env(key, val);
            }
        }
    }
    if let Some(ld_library_path) =
        dylint_nix_ld_library_path(client_env.as_deref(), is_dylint_driver)
    {
        builder = builder.env("LD_LIBRARY_PATH", ld_library_path);
    }
    lineage.apply_to_async_builder(builder, client_env.as_deref())
}

/// Return the `LD_LIBRARY_PATH` a Linux Dylint-driver child needs, if the
/// daemon has a Nix loader path to contribute. Client entries remain first so
/// their library selection wins; without a replayed client environment, the
/// daemon's inherited `LD_LIBRARY_PATH` remains first for the same reason.
fn dylint_nix_ld_library_path(
    client_env: Option<&[(String, String)]>,
    is_dylint_driver: bool,
) -> Option<std::ffi::OsString> {
    if !crate::platform::host::is_linux() || !is_dylint_driver {
        return None;
    }
    let nix_ld_library_path = std::env::var_os("NIX_LD_LIBRARY_PATH")?;
    if nix_ld_library_path.is_empty() {
        return None;
    }

    let ld_library_path = client_env
        .and_then(|vars| {
            vars.iter()
                .rev()
                .find_map(|(key, value)| (key == "LD_LIBRARY_PATH").then_some(value))
                .map(std::ffi::OsString::from)
        })
        .or_else(|| {
            client_env
                .is_none()
                .then(|| std::env::var_os("LD_LIBRARY_PATH"))
                .flatten()
        });
    Some(match ld_library_path {
        Some(ld_library_path) => {
            let mut combined = ld_library_path;
            combined.push(":");
            combined.push(nix_ld_library_path);
            combined
        }
        None => nix_ld_library_path,
    })
}

/// Cargo jobserver env vars name process-local file descriptors. The daemon
/// receives those names through IPC, not the fds themselves, so replaying them
/// into daemon-spawned compilers produces Cargo's stale-jobserver warning.
pub(super) fn client_env_var_is_safe_to_replay(key: &str) -> bool {
    !matches!(
        key,
        "MAKEFLAGS" | "CARGO_MAKEFLAGS" | crate::compiler::DYLINT_CACHE_INPUT_HASH_ENV
    )
}

/// Sync-command counterpart of [`apply_client_env_builder`].
#[cfg(test)]
pub(super) fn apply_client_env_sync(
    cmd: &mut std::process::Command,
    client_env: Option<&[(String, String)]>,
    lineage: &super::super::lineage::Lineage,
    is_dylint_driver: bool,
) {
    if let Some(vars) = client_env {
        cmd.env_clear();
        for (key, val) in vars {
            if client_env_var_is_safe_to_replay(key) {
                cmd.env(key, val);
            }
        }
    }
    if let Some(ld_library_path) = dylint_nix_ld_library_path(client_env, is_dylint_driver) {
        cmd.env("LD_LIBRARY_PATH", ld_library_path);
    }
    lineage.apply_to_sync(cmd, client_env);
}

/// Look up the client PID for a session. Returns `None` if the session is
/// unknown (already ended) — callers should still emit lineage with whatever
/// they know.
pub(super) fn session_client_pid(state: &SharedState, sid: &SessionId) -> Option<u32> {
    state.sessions.get(sid).map(|s| s.client_pid)
}
