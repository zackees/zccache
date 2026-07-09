//! Private daemon owner/ref-count state for soldr development isolation.

use crate::protocol::{PrivateDaemonOwnerStatus, PrivateDaemonStatus};
use std::collections::{BTreeSet, HashMap};
use tokio::sync::Mutex;

#[derive(Default)]
pub(super) struct PrivateDaemonLifecycle {
    state: Mutex<PrivateDaemonState>,
}

#[derive(Default)]
struct PrivateDaemonState {
    enabled: bool,
    owner_refs: HashMap<u32, u64>,
    private_env_keys: BTreeSet<String>,
}

pub(super) struct OwnerPruneResult {
    pub(super) removed_pids: Vec<u32>,
    pub(super) should_shutdown: bool,
}

impl PrivateDaemonLifecycle {
    pub(super) fn new() -> Self {
        Self::default()
    }

    pub(super) async fn register_session(
        &self,
        owner_pids: &[u32],
        private_env: &[(String, String)],
    ) {
        let mut state = self.state.lock().await;
        state.enabled = true;
        for pid in owner_pids {
            *state.owner_refs.entry(*pid).or_insert(0) += 1;
        }
        for (key, _) in private_env {
            state.private_env_keys.insert(key.clone());
        }
    }

    pub(super) async fn release_session(&self, owner_pids: &[u32]) {
        let mut state = self.state.lock().await;
        for pid in owner_pids {
            let Some(count) = state.owner_refs.get_mut(pid) else {
                continue;
            };
            *count = count.saturating_sub(1);
            if *count == 0 {
                state.owner_refs.remove(pid);
            }
        }
    }

    pub(super) async fn prune_dead_owner_pids<F>(&self, is_alive: F) -> OwnerPruneResult
    where
        F: Fn(u32) -> bool,
    {
        let mut state = self.state.lock().await;
        if !state.enabled {
            return OwnerPruneResult {
                removed_pids: Vec::new(),
                should_shutdown: false,
            };
        }

        let dead: Vec<u32> = state
            .owner_refs
            .keys()
            .copied()
            .filter(|pid| !is_alive(*pid))
            .collect();
        for pid in &dead {
            state.owner_refs.remove(pid);
        }
        OwnerPruneResult {
            removed_pids: dead,
            should_shutdown: state.owner_refs.is_empty(),
        }
    }

    pub(super) async fn is_enabled(&self) -> bool {
        self.state.lock().await.enabled
    }

    pub(super) async fn snapshot(&self) -> PrivateDaemonStatus {
        let state = self.state.lock().await;
        if !state.enabled {
            return PrivateDaemonStatus::shared();
        }
        let mut owners: Vec<PrivateDaemonOwnerStatus> = state
            .owner_refs
            .iter()
            .map(|(pid, ref_count)| PrivateDaemonOwnerStatus {
                pid: *pid,
                ref_count: *ref_count,
            })
            .collect();
        owners.sort_by_key(|owner| owner.pid);
        PrivateDaemonStatus {
            enabled: true,
            owners,
            private_env_keys: state.private_env_keys.iter().cloned().collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn private_daemon_ref_counts_owner_pids() {
        let lifecycle = PrivateDaemonLifecycle::new();
        lifecycle
            .register_session(&[10], &[("ZCCACHE_PRIVATE".into(), "secret".into())])
            .await;
        lifecycle.register_session(&[10, 20], &[]).await;

        let status = lifecycle.snapshot().await;
        assert!(status.enabled);
        assert_eq!(
            status.owners,
            vec![
                PrivateDaemonOwnerStatus {
                    pid: 10,
                    ref_count: 2
                },
                PrivateDaemonOwnerStatus {
                    pid: 20,
                    ref_count: 1
                }
            ]
        );
        assert_eq!(status.private_env_keys, vec!["ZCCACHE_PRIVATE"]);

        lifecycle.release_session(&[10]).await;
        let status = lifecycle.snapshot().await;
        assert_eq!(status.owners[0].ref_count, 1);
    }

    #[tokio::test]
    async fn private_daemon_prune_requests_shutdown_after_last_owner_dies() {
        let lifecycle = PrivateDaemonLifecycle::new();
        lifecycle.register_session(&[10, 20], &[]).await;

        let first = lifecycle.prune_dead_owner_pids(|pid| pid == 20).await;
        assert_eq!(first.removed_pids, vec![10]);
        assert!(!first.should_shutdown);

        let second = lifecycle.prune_dead_owner_pids(|_pid| false).await;
        assert_eq!(second.removed_pids, vec![20]);
        assert!(second.should_shutdown);
    }
}
