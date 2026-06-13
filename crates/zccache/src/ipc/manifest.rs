//! Publish the zccache `CacheManifest` into the running-process central
//! registry (zackees/running-process#435).
//!
//! The manifest lets broker peers discover this daemon's cache roots without
//! probing the daemon. It is built with the frozen `CacheManifestBuilder`
//! (#433), which stamps the broker-owned boilerplate (media type, schema
//! version, host identity, timestamps) and seals the `self_sha256` digest on
//! `publish`. The daemon records the manifest at startup, beside writing its
//! `BackendHandle` identity (see [`crate::ipc::write_backend_identity`]).
//!
//! The manifest records the five cache roots named in the zccache adoption
//! guide — artifact, index, log, lock, and temp — mapped onto the v1
//! `CacheRootKind` taxonomy:
//!
//! | zccache root | `CacheRootKind` | path source |
//! |---|---|---|
//! | artifact | `CacheData` | [`crate::core::config::artifacts_dir_from_cache_dir`] |
//! | index | `CacheIndex` | depgraph dir under the cache root |
//! | log | `CacheLogs` | log dir under the cache root |
//! | lock | `CacheLocks` | the cache root itself (lock/socket/identity live here) |
//! | temp | `CacheTmp` | [`crate::core::config::tmp_dir_from_cache_dir`] |
//!
//! Publishing is best-effort and never blocks daemon startup: a registry write
//! failure is logged and ignored, exactly like the `BackendHandle` identity
//! write. `RUNNING_PROCESS_DISABLE=1` skips publishing entirely so the direct
//! bincode path stays byte-for-byte the pre-adoption behavior.

use std::path::{Path, PathBuf};

use running_process::broker::builders::CacheManifestBuilder;
use running_process::broker::protocol::CacheRootKind;

use crate::core::NormalizedPath;

/// Service name advertised by the manifest — must match the `ServiceDefinition`
/// and the `BackendHandle` probe (`crate::ipc::probe_backend_handle`).
pub const ZCCACHE_SERVICE_NAME: &str = "zccache";

/// Broker instance label for the per-user shared broker.
const SHARED_BROKER_INSTANCE: &str = "shared";

/// Build the zccache `CacheManifest` for `cache_dir` without persisting it.
///
/// Exposed for tests; production code calls [`publish_manifest`].
#[must_use]
pub fn build_manifest_builder(cache_dir: &NormalizedPath) -> CacheManifestBuilder {
    let index_dir = cache_dir.join("depgraph");
    let log_dir = cache_dir.join("logs");
    CacheManifestBuilder::new(ZCCACHE_SERVICE_NAME, crate::core::VERSION)
        .broker_instance(SHARED_BROKER_INSTANCE)
        .root(
            CacheRootKind::CacheData,
            path_string(&crate::core::config::artifacts_dir_from_cache_dir(cache_dir)),
        )
        .root(CacheRootKind::CacheIndex, path_string(&index_dir))
        .root(CacheRootKind::CacheLogs, path_string(&log_dir))
        // Lock files, the daemon socket, and the running-process identity JSON
        // all live directly under the cache root.
        .root(CacheRootKind::CacheLocks, path_string(cache_dir))
        .root(
            CacheRootKind::CacheTmp,
            path_string(&crate::core::config::tmp_dir_from_cache_dir(cache_dir)),
        )
}

/// Seal and write the zccache cache manifest into the running-process central
/// registry. Best-effort: returns the written path on success.
///
/// Honors `RUNNING_PROCESS_DISABLE=1` (returns `None` without writing).
pub fn publish_manifest(cache_dir: &NormalizedPath) -> Option<PathBuf> {
    if super::running_process_disabled() {
        return None;
    }
    match build_manifest_builder(cache_dir).publish() {
        Ok(path) => Some(path),
        Err(err) => {
            tracing::warn!(error = %err, "failed to publish running-process cache manifest");
            None
        }
    }
}

/// Seal and write the manifest into an explicit registry dir (tests, custom
/// layouts). Bypasses the disable hatch so tests stay deterministic.
pub fn publish_manifest_in(
    registry_dir: &Path,
    cache_dir: &NormalizedPath,
) -> Result<PathBuf, running_process::broker::manifest::ManifestError> {
    build_manifest_builder(cache_dir).publish_in(registry_dir)
}

fn path_string(path: &NormalizedPath) -> String {
    path.as_path().display().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use running_process::broker::manifest::read_manifest;
    use running_process::broker::protocol::CacheRoot;

    fn roots_by_kind(roots: &[CacheRoot]) -> Vec<(i32, String)> {
        roots.iter().map(|r| (r.kind, r.path.clone())).collect()
    }

    #[test]
    fn manifest_records_all_five_cache_roots() {
        let cache_dir = NormalizedPath::from("/tmp/zccache-manifest-test");
        let manifest = build_manifest_builder(&cache_dir)
            .build()
            .expect("seal manifest");

        assert_eq!(manifest.service_name, "zccache");
        assert_eq!(manifest.service_version, crate::core::VERSION);
        assert_eq!(manifest.broker_instance, "shared");

        let kinds: Vec<i32> = manifest.roots.iter().map(|r| r.kind).collect();
        assert!(
            kinds.contains(&(CacheRootKind::CacheData as i32)),
            "artifact"
        );
        assert!(kinds.contains(&(CacheRootKind::CacheIndex as i32)), "index");
        assert!(kinds.contains(&(CacheRootKind::CacheLogs as i32)), "log");
        assert!(kinds.contains(&(CacheRootKind::CacheLocks as i32)), "lock");
        assert!(kinds.contains(&(CacheRootKind::CacheTmp as i32)), "temp");
        assert_eq!(manifest.roots.len(), 5);
    }

    #[test]
    fn publish_round_trips_through_central_registry() {
        let registry = tempfile::tempdir().expect("tempdir");
        let cache_dir = NormalizedPath::from("/tmp/zccache-manifest-roundtrip");

        let written = publish_manifest_in(registry.path(), &cache_dir).expect("publish manifest");
        assert!(written.exists(), "manifest file should exist on disk");

        // read_manifest recomputes the self_sha256 digest, so a successful
        // load proves the CacheManifestBuilder sealed the manifest correctly.
        let loaded = read_manifest(&written).expect("read + verify sealed manifest");
        assert_eq!(loaded.service_name, "zccache");

        let original = roots_by_kind(
            &build_manifest_builder(&cache_dir)
                .build()
                .expect("seal manifest")
                .roots,
        );
        assert_eq!(roots_by_kind(&loaded.roots), original);
    }
}
