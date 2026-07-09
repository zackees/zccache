//! Publish the zccache `CacheManifest` into the running-process central
//! registry (zackees/running-process#435).
//!
//! The manifest lets broker peers discover this daemon's cache roots without
//! probing the daemon. It is built with the frozen `CacheManifestBuilder`
//! (#433), which stamps the broker-owned boilerplate (media type, schema
//! version, host identity, timestamps) and seals the `self_sha256` digest on
//! `publish`. The daemon records the manifest at startup, beside writing its
//! `BackendHandle` identity (see [`crate::write_backend_identity`]).
//!
//! The manifest records the five cache roots named in the zccache adoption
//! guide — artifact, index, log, lock, and temp — mapped onto the v1
//! `CacheRootKind` taxonomy:
//!
//! | zccache root | `CacheRootKind` | path source |
//! |---|---|---|
//! | artifact | `CacheData` | [`zccache_core::config::artifacts_dir_from_cache_dir`] |
//! | index | `CacheIndex` | depgraph dir under the cache root |
//! | log | `CacheLogs` | log dir under the cache root |
//! | lock | `CacheLocks` | the cache root itself (lock/socket/identity live here) |
//! | temp | `CacheTmp` | [`zccache_core::config::tmp_dir_from_cache_dir`] |
//!
//! Publishing is best-effort and never blocks daemon startup: a registry write
//! failure is logged and ignored, exactly like the `BackendHandle` identity
//! write. `RUNNING_PROCESS_DISABLE=1` skips publishing entirely so the direct
//! bincode path stays byte-for-byte the pre-adoption behavior.
//!
//! ## v2 dual-write (slice 23 of zccache#782)
//!
//! As of running-process PR #525 + #526 a v2 `CacheManifestBuilder` +
//! `write_to_central_v2` surface exists upstream
//! (`broker::protocol_v2::manifest_io`) with wire values that mirror
//! v1's `CacheRootKind` exactly. To stay reachable from both v1 and v2
//! brokers during the rollout, [`publish_manifest`] now writes BOTH
//! `zccache-<ver>.pb` and `zccache-<ver>.v2.pb` into the central
//! registry. Each carries the same service identity + cache-root list;
//! the v2 file is read by the v2 broker scaffold (loader still in
//! flight). v2 failures are logged but do NOT abort the v1 write —
//! v1 stays primary during rollout. zccache#782 slice 25 collapses
//! this to v2-only once v1 retires.

use std::path::{Path, PathBuf};

use running_process::broker::builders::CacheManifestBuilder;
use running_process::broker::protocol::CacheRootKind;
use running_process::broker::protocol_v2;

use zccache_core::NormalizedPath;

/// Service name advertised by the manifest — must match the `ServiceDefinition`
/// and the `BackendHandle` probe (`crate::probe_backend_handle`).
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
    CacheManifestBuilder::new(ZCCACHE_SERVICE_NAME, zccache_core::VERSION)
        .broker_instance(SHARED_BROKER_INSTANCE)
        .root(
            CacheRootKind::CacheData,
            path_string(&zccache_core::config::artifacts_dir_from_cache_dir(cache_dir)),
        )
        .root(CacheRootKind::CacheIndex, path_string(&index_dir))
        .root(CacheRootKind::CacheLogs, path_string(&log_dir))
        // Lock files, the daemon socket, and the running-process identity JSON
        // all live directly under the cache root.
        .root(CacheRootKind::CacheLocks, path_string(cache_dir))
        .root(
            CacheRootKind::CacheTmp,
            path_string(&zccache_core::config::tmp_dir_from_cache_dir(cache_dir)),
        )
}

/// Seal and write the zccache cache manifest into the running-process central
/// registry. Best-effort: returns the written path on success.
///
/// Slice 23 of zccache#782: also dual-writes a v2 manifest
/// (`zccache-<ver>.v2.pb`) into the same registry. The v2 write is
/// best-effort and never blocks the v1 path — a v2 failure is logged
/// and ignored.
///
/// Honors `RUNNING_PROCESS_DISABLE=1` (returns `None` without writing).
pub fn publish_manifest(cache_dir: &NormalizedPath) -> Option<PathBuf> {
    if super::running_process_disabled() {
        return None;
    }
    let v1_path = match build_manifest_builder(cache_dir).publish() {
        Ok(path) => Some(path),
        Err(err) => {
            tracing::warn!(error = %err, "failed to publish running-process cache manifest");
            None
        }
    };

    // Dual-write v2 alongside v1. The v2 write reuses v1's
    // `central_registry_dir()` (mirrored upstream as
    // `central_registry_dir_v2()`), so a single directory carries
    // both. v2 failures are logged + ignored.
    if let Err(err) = build_manifest_builder_v2(cache_dir).publish() {
        tracing::warn!(
            error = %err,
            "v2 manifest dual-write failed (non-fatal during rollout)"
        );
    }

    v1_path
}

/// Seal and write the manifest into an explicit registry dir (tests, custom
/// layouts). Bypasses the disable hatch so tests stay deterministic.
///
/// Returns the v1 path; the v2 file is written alongside as a side
/// effect (callers that need both can locate the v2 file at
/// `<v1_path.with_extension("v2.pb")>`).
pub fn publish_manifest_in(
    registry_dir: &Path,
    cache_dir: &NormalizedPath,
) -> Result<PathBuf, running_process::broker::manifest::ManifestError> {
    let v1_path = build_manifest_builder(cache_dir).publish_in(registry_dir)?;
    // v2 dual-write — surface the v2 error here so tests can pin
    // both write paths. Production [`publish_manifest`] swallows the
    // v2 error; tests want the louder failure mode.
    let _v2_path = build_manifest_builder_v2(cache_dir).publish_in(registry_dir)?;
    Ok(v1_path)
}

/// Slice 23 of zccache#782: build a v2 CacheManifest mirroring the v1
/// shape from [`build_manifest_builder`]. Shares service identity,
/// version, broker_instance, and the full cache-root list — every
/// `CacheRootKind` value is identical to v1 (the upstream `as i32`
/// cast is wire-compatible per #526).
#[must_use]
pub fn build_manifest_builder_v2(cache_dir: &NormalizedPath) -> protocol_v2::CacheManifestBuilder {
    use running_process::broker::protocol_v2::CacheRootKind as V2;
    let index_dir = cache_dir.join("depgraph");
    let log_dir = cache_dir.join("logs");
    protocol_v2::CacheManifestBuilder::new(ZCCACHE_SERVICE_NAME, zccache_core::VERSION)
        .broker_instance(SHARED_BROKER_INSTANCE)
        .root(
            V2::CacheData,
            path_string(&zccache_core::config::artifacts_dir_from_cache_dir(
                cache_dir,
            )),
        )
        .root(V2::CacheIndex, path_string(&index_dir))
        .root(V2::CacheLogs, path_string(&log_dir))
        .root(V2::CacheLocks, path_string(cache_dir))
        .root(
            V2::CacheTmp,
            path_string(&zccache_core::config::tmp_dir_from_cache_dir(cache_dir)),
        )
}

/// Install the zccache `ServiceDefinition` into the running-process service-
/// definition directory at daemon startup (#720 Phase 2).
///
/// Best-effort and idempotent — `ServiceDefinitionBuilder::install_in` writes
/// atomically and overwrites a stale definition of the same service name with
/// the new one. A registry write failure is logged and ignored, exactly like
/// the [`publish_manifest`] / `write_backend_identity` siblings.
/// `RUNNING_PROCESS_DISABLE=1` skips installation entirely so the direct
/// bincode path stays byte-for-byte the pre-adoption behavior.
///
/// Phase 0 of #720 is the version-policy refinement that turns the current
/// exact-version pin (`min_version = allow_version = CARGO_PKG_VERSION`) into
/// a real compatibility floor + range; until that decision lands this
/// function preserves the existing exact-version policy already shipped by
/// the `zccache install-servicedef` CLI subcommand.
pub fn publish_service_definition(daemon_binary: &Path) -> Option<PathBuf> {
    use running_process::broker::builders::ServiceDefinitionBuilder;
    use running_process::broker::server::service_definition_dir;

    if super::running_process_disabled() {
        return None;
    }

    let binary = match std::fs::canonicalize(daemon_binary) {
        Ok(p) => p,
        Err(err) => {
            tracing::warn!(
                daemon_binary = %daemon_binary.display(),
                error = %err,
                "failed to canonicalize zccache-daemon binary for service-definition install"
            );
            return None;
        }
    };
    let Some(binary_dir) = binary.parent() else {
        tracing::warn!(
            binary = %binary.display(),
            "zccache-daemon binary has no parent directory; skipping service-definition install"
        );
        return None;
    };

    match ServiceDefinitionBuilder::shared_broker(
        ZCCACHE_SERVICE_NAME,
        binary.display().to_string(),
    )
    .per_version_binary_dir(binary_dir.display().to_string())
    .min_version(zccache_core::VERSION)
    .allow_version(zccache_core::VERSION)
    .label("vendor", "zackees")
    .label("package", "zccache")
    .label("consumer", "zccache")
    .label("running-process-tracker", "zackees/running-process#435")
    .install_in(&service_definition_dir())
    {
        Ok(path) => {
            tracing::debug!(servicedef = %path.display(), "installed running-process service definition");
            Some(path)
        }
        Err(err) => {
            tracing::warn!(error = %err, "failed to install running-process service definition");
            None
        }
    }
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
        assert_eq!(manifest.service_version, zccache_core::VERSION);
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

    /// Slice 23 of zccache#782: dual-write also produces a v2
    /// `zccache-<ver>.v2.pb` file alongside the v1 file, carrying the
    /// same identity + cache-root list. Pins that a v2 broker would
    /// discover the same zccache cache layout once it has a loader.
    #[test]
    fn publish_also_writes_v2_manifest() {
        use prost::Message;

        let registry = tempfile::tempdir().expect("tempdir");
        let cache_dir = NormalizedPath::from("/tmp/zccache-manifest-v2-roundtrip");

        publish_manifest_in(registry.path(), &cache_dir).expect("publish manifest");

        // The v2 file lives at the same stem but with .v2.pb extension.
        let v2_path = registry
            .path()
            .join(format!("zccache-{}.v2.pb", zccache_core::VERSION));
        assert!(
            v2_path.exists(),
            "v2 manifest must exist at {}",
            v2_path.display()
        );

        let bytes = std::fs::read(&v2_path).expect("read v2 file");
        let decoded =
            protocol_v2::CacheManifest::decode(bytes.as_slice()).expect("v2 CacheManifest decodes");

        assert_eq!(decoded.service_name, "zccache");
        assert_eq!(decoded.service_version, zccache_core::VERSION);
        assert_eq!(decoded.broker_envelope_version, "v2");
        assert_eq!(decoded.broker_instance, "shared");
        assert_eq!(decoded.roots.len(), 5, "all 5 cache roots present in v2");

        // v2 wire values mirror v1's per upstream PR #526, so an
        // `as i32` cast from the v1 enum equals the v2 enum value.
        use running_process::broker::protocol_v2::CacheRootKind as V2;
        let v2_kinds: Vec<i32> = decoded.roots.iter().map(|r| r.kind).collect();
        assert!(v2_kinds.contains(&(V2::CacheData as i32)));
        assert!(v2_kinds.contains(&(V2::CacheIndex as i32)));
        assert!(v2_kinds.contains(&(V2::CacheLogs as i32)));
        assert!(v2_kinds.contains(&(V2::CacheLocks as i32)));
        assert!(v2_kinds.contains(&(V2::CacheTmp as i32)));
    }

    /// v1↔v2 wire alignment: each cache root in the v2 manifest must
    /// match the corresponding root in the v1 manifest at the same
    /// `kind as i32` value (per upstream PR #526). Pins that the two
    /// generations agree on the role classification for every root
    /// zccache writes.
    #[test]
    fn v1_and_v2_manifest_roots_agree_on_wire_values() {
        let cache_dir = NormalizedPath::from("/tmp/zccache-manifest-alignment");
        let v1 = build_manifest_builder(&cache_dir).build().expect("seal v1");
        let v2 = build_manifest_builder_v2(&cache_dir).build();
        assert_eq!(v1.roots.len(), v2.roots.len(), "same root count");
        for (a, b) in v1.roots.iter().zip(v2.roots.iter()) {
            assert_eq!(a.kind, b.kind, "kind mismatch for path={}", a.path);
            assert_eq!(a.path, b.path, "path mismatch for kind={}", a.kind);
        }
    }
}
