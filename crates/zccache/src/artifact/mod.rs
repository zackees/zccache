//! Disk artifact cache for zccache.
//!
//! Provides content-addressed storage for compilation artifacts, backed by
//! the filesystem with an in-memory `ArtifactIndex` snapshotted to a bincode
//! blob (`index.bin`) for metadata and eviction.

#![allow(clippy::missing_errors_doc)] // TODO: add error docs

pub mod kv;
mod rust_plan;
mod store;

pub use kv::{
    is_valid_namespace, Key, KvError, KvResult, KvStore, INLINE_THRESHOLD, MAX_VALUE_BYTES,
};
#[cfg(feature = "gha")]
pub use rust_plan::{
    restore_rust_plan_gha, restore_rust_plan_layered_local, restore_rust_plan_local,
    rust_plan_bundle_dir, rust_plan_cache_key, rust_plan_gha_version, save_rust_plan_delta_local,
    save_rust_plan_gha, save_rust_plan_local, RustArtifactBundleLayerKind,
    RustArtifactBundleManifest, RustArtifactClass, RustArtifactPlanV1, RustBundledArtifact,
    RustPlanArtifactEffectiveness, RustPlanCompatibility, RustPlanError, RustPlanGhaError,
    RustPlanInputs, RustPlanMode, RustPlanOperation, RustPlanPackages, RustPlanSkippedSample,
    RustPlanSummary, RustToolchainIdentity, RUST_ARTIFACT_CACHE_SCHEMA_VERSION,
    RUST_ARTIFACT_PLAN_SCHEMA_VERSION,
};
#[cfg(not(feature = "gha"))]
pub use rust_plan::{
    restore_rust_plan_layered_local, restore_rust_plan_local, rust_plan_bundle_dir,
    rust_plan_cache_key, save_rust_plan_delta_local, save_rust_plan_local,
    RustArtifactBundleLayerKind, RustArtifactBundleManifest, RustArtifactClass, RustArtifactPlanV1,
    RustBundledArtifact, RustPlanArtifactEffectiveness, RustPlanCompatibility, RustPlanError,
    RustPlanInputs, RustPlanMode, RustPlanOperation, RustPlanPackages, RustPlanSkippedSample,
    RustPlanSummary, RustToolchainIdentity, RUST_ARTIFACT_CACHE_SCHEMA_VERSION,
    RUST_ARTIFACT_PLAN_SCHEMA_VERSION,
};
#[cfg(feature = "cli")]
pub(crate) use rust_plan::{tar_gz_decode, tar_gz_encode};
pub use store::{ArtifactIndex, ArtifactStore};

use crate::core::NormalizedPath;
use std::path::Path;

/// Configuration for the artifact store.
#[derive(Debug, Clone)]
pub struct ArtifactStoreConfig {
    /// Root directory for artifact storage.
    pub cache_dir: NormalizedPath,
    /// Maximum total cache size in bytes.
    pub max_size: u64,
}

/// The artifact store manages cached compilation outputs on disk.
///
/// Artifacts are stored in a content-addressed directory layout:
/// `{cache_dir}/artifacts/{hash[0..2]}/{hash[2..4]}/{hash}`
///
/// A bincode blob at `{cache_dir}/index.bin` tracks metadata and access
/// times for eviction; see `store.rs` for the in-memory + flush design.
pub struct ArtifactStoreLegacy {
    config: ArtifactStoreConfig,
}

impl ArtifactStoreLegacy {
    /// Open or create an artifact store at the given configuration.
    ///
    /// Creates the cache directory if it does not exist.
    pub fn open(config: ArtifactStoreConfig) -> crate::core::Result<Self> {
        std::fs::create_dir_all(&config.cache_dir)?;
        Ok(Self { config })
    }

    /// Returns the path where an artifact with the given key would be stored.
    #[must_use]
    pub fn artifact_path(&self, key: &crate::hash::ContentHash) -> NormalizedPath {
        let shards = key.shard_prefix(2, 1);
        self.config
            .cache_dir
            .join("artifacts")
            .join(&shards[0])
            .join(&shards[1])
            .join(key.to_hex())
    }

    /// Check if an artifact exists for the given cache key.
    #[must_use]
    pub fn contains(&self, key: &crate::hash::ContentHash) -> bool {
        self.artifact_path(key).exists()
    }

    /// Returns the configured cache directory.
    #[must_use]
    pub fn cache_dir(&self) -> &Path {
        &self.config.cache_dir
    }
}
