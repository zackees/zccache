//! `DaemonStatus` bincode roundtrip tests.

use super::*;

#[test]
fn daemon_status_expanded_roundtrip() {
    let status = DaemonStatus {
        version: env!("CARGO_PKG_VERSION").to_string(),
        daemon_namespace: "soldr-dev".to_string(),
        endpoint: "test://soldr-dev".to_string(),
        private_daemon: PrivateDaemonStatus {
            enabled: true,
            owners: vec![PrivateDaemonOwnerStatus {
                pid: 1234,
                ref_count: 2,
            }],
            private_env_keys: vec!["ZCCACHE_PATH_REMAP".to_string()],
        },
        artifact_count: 892,
        cache_size_bytes: 147_000_000,
        metadata_entries: 5430,
        uptime_secs: 8040,
        cache_hits: 1089,
        cache_misses: 143,
        total_compilations: 1247,
        non_cacheable: 15,
        compile_errors: 3,
        compile_errors_cached: 2,
        time_saved_ms: 750_000,
        total_links: 50,
        link_hits: 38,
        link_misses: 10,
        link_non_cacheable: 2,
        dep_graph_contexts: 892,
        dep_graph_files: 4201,
        sessions_total: 41,
        sessions_active: 3,
        cache_dir: "/home/user/.zccache".into(),
        dep_graph_version: 1,
        dep_graph_disk_size: 2_500_000,
        dep_graph_persisted: true,
    };
    roundtrip(&status);
}

#[test]
fn daemon_status_version_field_roundtrips() {
    let with_version = DaemonStatus {
        version: "1.2.3".to_string(),
        daemon_namespace: zccache_core::config::DEFAULT_DAEMON_NAMESPACE.to_string(),
        endpoint: String::new(),
        private_daemon: PrivateDaemonStatus::shared(),
        artifact_count: 0,
        cache_size_bytes: 0,
        metadata_entries: 0,
        uptime_secs: 0,
        cache_hits: 0,
        cache_misses: 0,
        total_compilations: 0,
        non_cacheable: 0,
        compile_errors: 0,
        compile_errors_cached: 0,
        time_saved_ms: 0,
        total_links: 0,
        link_hits: 0,
        link_misses: 0,
        link_non_cacheable: 0,
        dep_graph_contexts: 0,
        dep_graph_files: 0,
        sessions_total: 0,
        sessions_active: 0,
        cache_dir: "".into(),
        dep_graph_version: 0,
        dep_graph_disk_size: 0,
        dep_graph_persisted: false,
    };
    roundtrip(&with_version);
}
