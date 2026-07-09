//! Spawn-lineage env propagation.
//!
//! Every child process spawned by the daemon — compilers, linkers, deploy
//! hooks, system-include probes — is tagged with a small set of env vars that
//! identify zccache as the spawn owner and record the chain of ancestor PIDs.
//! This lets external tools attribute orphaned descendants back to zccache and
//! reconstruct the full spawn lineage even after the daemon has exited.
//!
//! The originator marker uses the `TOOL:PID` shape from
//! [`running-process`](https://github.com/zackees/running-process), so any tool
//! built around running-process can scan for orphaned descendants without
//! caring whether the spawn went through running-process directly or through
//! zccache. See issue #7 for the design rationale.

/// Compatibility marker used by `running-process` for crash-resilient orphan
/// discovery. Format: `TOOL:PID`. Preserved across the chain — the outermost
/// owner that already set this value remains the originator.
pub const ENV_ORIGINATOR: &str = "RUNNING_PROCESS_ORIGINATOR";

/// `>`-separated chain of ancestor PIDs (oldest first), terminating in the
/// PID that just spawned this child. Each spawn boundary appends.
pub const ENV_LINEAGE: &str = "ZCCACHE_LINEAGE";

/// PID of the immediate parent that spawned this child (the daemon).
pub const ENV_PARENT_PID: &str = "ZCCACHE_PARENT_PID";

/// PID of the zccache daemon that owns this spawn.
pub const ENV_DAEMON_PID: &str = "ZCCACHE_DAEMON_PID";

/// PID of the CLI client that issued the request (when known).
pub const ENV_CLIENT_PID: &str = "ZCCACHE_CLIENT_PID";

/// Session ID associated with the spawn (when known).
pub const ENV_SESSION_ID: &str = "ZCCACHE_SESSION_ID";

/// All lineage env var names this module sets. Useful for tests and tooling
/// that wants to inspect the full set of markers in one place.
pub const ALL: &[&str] = &[
    ENV_ORIGINATOR,
    ENV_LINEAGE,
    ENV_PARENT_PID,
    ENV_DAEMON_PID,
    ENV_CLIENT_PID,
    ENV_SESSION_ID,
];

/// Spawn-lineage context for a single child process.
#[derive(Clone, Debug)]
pub struct Lineage {
    pub daemon_pid: u32,
    pub client_pid: Option<u32>,
    pub session_id: Option<String>,
}

impl Lineage {
    /// Build a lineage for the running daemon. `client_pid` and `session_id`
    /// are `None` when the spawn is not associated with a client request
    /// (e.g., daemon-internal probes).
    #[must_use]
    pub fn current(client_pid: Option<u32>, session_id: Option<String>) -> Self {
        Self {
            daemon_pid: std::process::id(),
            client_pid,
            session_id,
        }
    }

    /// Compute the lineage env vars to overlay on a child's env.
    ///
    /// `incoming_env` is the env the child will receive. We read any existing
    /// lineage chain or originator tag from it so we can extend rather than
    /// overwrite — preserving outer ancestry the CLI inherited from a build
    /// tool wrapped by `running-process`.
    #[must_use]
    pub fn env_for_child(
        &self,
        incoming_env: Option<&[(String, String)]>,
    ) -> Vec<(String, String)> {
        fn lookup<'a>(env: Option<&'a [(String, String)]>, key: &str) -> Option<&'a str> {
            env.and_then(|e| e.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str()))
        }

        // Originator: the outermost owner gets to keep the tag. Only set ours
        // if no upstream tool has already claimed it.
        let originator = lookup(incoming_env, ENV_ORIGINATOR)
            .map(str::to_owned)
            .unwrap_or_else(|| format!("zccache:{}", self.daemon_pid));

        // Build the chain: existing > client_pid > daemon_pid.
        // We flatten the incoming chain into its individual segments so the
        // de-dup check (don't repeat the trailing PID) inspects the actual
        // last hop, not whatever combined string happened to be stored.
        let mut chain: Vec<String> = match lookup(incoming_env, ENV_LINEAGE) {
            Some(existing) if !existing.is_empty() => {
                existing.split('>').map(str::to_owned).collect()
            }
            _ => Vec::new(),
        };
        let push_unique = |chain: &mut Vec<String>, pid: u32| {
            let s = pid.to_string();
            if chain.last().map(String::as_str) != Some(s.as_str()) {
                chain.push(s);
            }
        };
        if let Some(pid) = self.client_pid {
            push_unique(&mut chain, pid);
        }
        push_unique(&mut chain, self.daemon_pid);
        let lineage = chain.join(">");

        let mut out = vec![
            (ENV_ORIGINATOR.into(), originator),
            (ENV_LINEAGE.into(), lineage),
            (ENV_DAEMON_PID.into(), self.daemon_pid.to_string()),
            (ENV_PARENT_PID.into(), self.daemon_pid.to_string()),
        ];
        if let Some(pid) = self.client_pid {
            out.push((ENV_CLIENT_PID.into(), pid.to_string()));
        }
        if let Some(ref sid) = self.session_id {
            out.push((ENV_SESSION_ID.into(), sid.clone()));
        }
        out
    }

    /// Apply the lineage to a `tokio::process::Command` after the caller has
    /// already populated the child's primary env.
    pub fn apply_to_tokio(
        &self,
        cmd: &mut tokio::process::Command,
        incoming_env: Option<&[(String, String)]>,
    ) {
        for (k, v) in self.env_for_child(incoming_env) {
            cmd.env(k, v);
        }
    }

    /// Apply the lineage to a `std::process::Command` after the caller has
    /// already populated the child's primary env.
    pub fn apply_to_sync(
        &self,
        cmd: &mut std::process::Command,
        incoming_env: Option<&[(String, String)]>,
    ) {
        for (k, v) in self.env_for_child(incoming_env) {
            cmd.env(k, v);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pairs(items: &[(&str, &str)]) -> Vec<(String, String)> {
        items
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    fn get<'a>(vars: &'a [(String, String)], key: &str) -> Option<&'a str> {
        vars.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
    }

    #[test]
    fn empty_incoming_starts_chain_with_daemon_only() {
        let l = Lineage {
            daemon_pid: 100,
            client_pid: None,
            session_id: None,
        };
        let out = l.env_for_child(None);
        assert_eq!(get(&out, ENV_LINEAGE), Some("100"));
        assert_eq!(get(&out, ENV_ORIGINATOR), Some("zccache:100"));
        assert_eq!(get(&out, ENV_DAEMON_PID), Some("100"));
        assert_eq!(get(&out, ENV_PARENT_PID), Some("100"));
        assert_eq!(get(&out, ENV_CLIENT_PID), None);
        assert_eq!(get(&out, ENV_SESSION_ID), None);
    }

    #[test]
    fn client_pid_appears_in_chain_before_daemon() {
        let l = Lineage {
            daemon_pid: 200,
            client_pid: Some(150),
            session_id: None,
        };
        let out = l.env_for_child(None);
        assert_eq!(get(&out, ENV_LINEAGE), Some("150>200"));
        assert_eq!(get(&out, ENV_CLIENT_PID), Some("150"));
    }

    #[test]
    fn existing_lineage_is_preserved_and_extended() {
        let incoming = pairs(&[(ENV_LINEAGE, "10>20"), (ENV_ORIGINATOR, "build:10")]);
        let l = Lineage {
            daemon_pid: 200,
            client_pid: Some(150),
            session_id: None,
        };
        let out = l.env_for_child(Some(&incoming));
        assert_eq!(get(&out, ENV_LINEAGE), Some("10>20>150>200"));
        // Outer originator keeps its claim — we are not the outermost owner.
        assert_eq!(get(&out, ENV_ORIGINATOR), Some("build:10"));
    }

    #[test]
    fn session_id_propagated_when_present() {
        let l = Lineage {
            daemon_pid: 100,
            client_pid: None,
            session_id: Some("abc-123".into()),
        };
        let out = l.env_for_child(None);
        assert_eq!(get(&out, ENV_SESSION_ID), Some("abc-123"));
    }

    #[test]
    fn duplicate_trailing_pid_is_collapsed() {
        // Incoming chain already ends with the client's PID — re-applying must
        // not produce `...>150>150`.
        let incoming = pairs(&[(ENV_LINEAGE, "10>150")]);
        let l = Lineage {
            daemon_pid: 200,
            client_pid: Some(150),
            session_id: None,
        };
        let out = l.env_for_child(Some(&incoming));
        assert_eq!(get(&out, ENV_LINEAGE), Some("10>150>200"));
    }

    #[test]
    fn current_uses_running_daemon_pid() {
        let l = Lineage::current(Some(42), Some("sid".into()));
        assert_eq!(l.daemon_pid, std::process::id());
        assert_eq!(l.client_pid, Some(42));
        assert_eq!(l.session_id.as_deref(), Some("sid"));
    }

    #[test]
    fn apply_to_tokio_sets_lineage_env() {
        let l = Lineage {
            daemon_pid: 100,
            client_pid: Some(50),
            session_id: None,
        };
        let mut cmd = tokio::process::Command::new("echo");
        l.apply_to_tokio(&mut cmd, None);
        let envs: Vec<(String, String)> = cmd
            .as_std()
            .get_envs()
            .filter_map(|(k, v)| {
                Some((
                    k.to_string_lossy().into_owned(),
                    v?.to_string_lossy().into_owned(),
                ))
            })
            .collect();
        assert_eq!(get(&envs, ENV_LINEAGE), Some("50>100"));
        assert_eq!(get(&envs, ENV_ORIGINATOR), Some("zccache:100"));
    }

    #[test]
    fn apply_to_sync_sets_lineage_env() {
        let l = Lineage {
            daemon_pid: 100,
            client_pid: None,
            session_id: None,
        };
        let mut cmd = std::process::Command::new("echo");
        l.apply_to_sync(&mut cmd, None);
        let envs: Vec<(String, String)> = cmd
            .get_envs()
            .filter_map(|(k, v)| {
                Some((
                    k.to_string_lossy().into_owned(),
                    v?.to_string_lossy().into_owned(),
                ))
            })
            .collect();
        assert_eq!(get(&envs, ENV_DAEMON_PID), Some("100"));
    }

    #[test]
    fn empty_existing_lineage_treated_as_unset() {
        // A literal empty `ZCCACHE_LINEAGE=` should not produce a leading `>`
        // when the chain is built — guard against `> > 100` style malformations.
        let incoming = pairs(&[(ENV_LINEAGE, "")]);
        let l = Lineage {
            daemon_pid: 100,
            client_pid: None,
            session_id: None,
        };
        let out = l.env_for_child(Some(&incoming));
        assert_eq!(get(&out, ENV_LINEAGE), Some("100"));
    }

    #[test]
    fn no_client_pid_starts_chain_with_daemon_only_even_when_session_set() {
        // `client_pid` and `session_id` are independent: a daemon-internal probe
        // can have a session_id without a client_pid (no IPC client to attribute).
        let l = Lineage {
            daemon_pid: 99,
            client_pid: None,
            session_id: Some("probe".into()),
        };
        let out = l.env_for_child(None);
        assert_eq!(get(&out, ENV_LINEAGE), Some("99"));
        assert_eq!(get(&out, ENV_CLIENT_PID), None);
        assert_eq!(get(&out, ENV_SESSION_ID), Some("probe"));
    }

    #[test]
    fn all_constants_are_unique_and_present() {
        // Cheap regression check: if someone adds a new constant they must
        // also list it in `ALL`. Catches drift between the two.
        let mut seen = std::collections::HashSet::new();
        for var in ALL {
            assert!(seen.insert(*var), "duplicate var in ALL: {var}");
        }
        assert!(ALL.contains(&ENV_ORIGINATOR));
        assert!(ALL.contains(&ENV_LINEAGE));
        assert!(ALL.contains(&ENV_DAEMON_PID));
        assert!(ALL.contains(&ENV_PARENT_PID));
        assert!(ALL.contains(&ENV_CLIENT_PID));
        assert!(ALL.contains(&ENV_SESSION_ID));
    }

    #[test]
    fn deeply_nested_chain_is_extended() {
        // Stress: a chain that's already 5 hops deep should grow to 7 after
        // we add the daemon's two fresh hops. No segments dropped.
        let incoming = pairs(&[(ENV_LINEAGE, "1>2>3>4>5")]);
        let l = Lineage {
            daemon_pid: 700,
            client_pid: Some(600),
            session_id: None,
        };
        let out = l.env_for_child(Some(&incoming));
        assert_eq!(get(&out, ENV_LINEAGE), Some("1>2>3>4>5>600>700"));
    }

    #[test]
    fn originator_is_independent_of_lineage_chain() {
        // Originator is preserved even when the chain's outer entry doesn't
        // match the originator's PID — the two pieces of info are independent
        // (originator is a *tool name*, the chain is just PIDs).
        let incoming = pairs(&[(ENV_ORIGINATOR, "myrunner:99999"), (ENV_LINEAGE, "100>200")]);
        let l = Lineage {
            daemon_pid: 400,
            client_pid: None,
            session_id: None,
        };
        let out = l.env_for_child(Some(&incoming));
        assert_eq!(get(&out, ENV_ORIGINATOR), Some("myrunner:99999"));
        assert_eq!(get(&out, ENV_LINEAGE), Some("100>200>400"));
    }

    #[test]
    fn overlay_after_replay_extends_chain_and_preserves_unrelated_env() {
        // Mirrors `apply_client_env_sync`: the caller first replays client_env
        // (which contains an outer chain plus unrelated vars like PATH), then
        // overlays the lineage. We assert the overlay extends — not duplicates —
        // the lineage and leaves unrelated env untouched.
        let incoming = pairs(&[(ENV_LINEAGE, "10>20"), ("PATH", "/usr/bin")]);
        let l = Lineage {
            daemon_pid: 300,
            client_pid: Some(200),
            session_id: None,
        };
        let mut cmd = std::process::Command::new("echo");
        for (k, v) in &incoming {
            cmd.env(k, v);
        }
        l.apply_to_sync(&mut cmd, Some(&incoming));
        let envs: Vec<(String, String)> = cmd
            .get_envs()
            .filter_map(|(k, v)| {
                Some((
                    k.to_string_lossy().into_owned(),
                    v?.to_string_lossy().into_owned(),
                ))
            })
            .collect();
        assert_eq!(get(&envs, ENV_LINEAGE), Some("10>20>200>300"));
        assert_eq!(get(&envs, "PATH"), Some("/usr/bin"));
    }

    #[test]
    fn lineage_is_clone_and_debug() {
        // Lineage is passed by reference into spawn helpers but tests/builders
        // sometimes need to clone or print it. Compile-time check.
        let l = Lineage {
            daemon_pid: 1,
            client_pid: Some(2),
            session_id: Some("s".into()),
        };
        let cloned = l.clone();
        let _ = format!("{cloned:?}");
    }
}
