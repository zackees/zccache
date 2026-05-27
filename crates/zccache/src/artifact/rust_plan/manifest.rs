//! Bundle manifest model, protobuf IO, validation, and path safety.

use std::path::{Component, Path};

use crate::core::NormalizedPath;
use prost::Message;
use serde::{Deserialize, Serialize};

use super::local::rust_plan_identity_hash;
use super::proto::{manifest_from_proto, manifest_to_proto, rust_plan_proto};
use super::schema::{
    RustArtifactClass, RustArtifactPlanV1, RustPlanError, RustPlanMode,
    RUST_ARTIFACT_CACHE_SCHEMA_VERSION,
};
use super::summary::RustPlanSummary;

pub(super) const BUNDLE_MANIFEST_NAME: &str = "manifest.pb";
pub(super) const LEGACY_BUNDLE_MANIFEST_NAME: &str = "manifest.json";
pub(super) const BUNDLE_FILES_DIR: &str = "files";

/// File stored in a Rust artifact bundle.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RustBundledArtifact {
    pub relative_path: String,
    pub class: RustArtifactClass,
    pub size: u64,
    pub content_hash: String,
    #[serde(default)]
    pub mtime_unix_nanos: u64,
}

/// Layer kind for a Rust artifact bundle manifest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RustArtifactBundleLayerKind {
    Complete,
    Base,
    Delta,
}

/// Manifest for zccache-owned Rust artifact bundles.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RustArtifactBundleManifest {
    pub manifest_schema_version: u32,
    pub plan_schema_version: u32,
    pub cache_schema_version: u32,
    pub mode: RustPlanMode,
    pub cache_key: String,
    pub created_at_secs: u64,
    pub plan_identity_hash: String,
    pub artifacts: Vec<RustBundledArtifact>,
    #[serde(default = "default_bundle_layer_kind")]
    pub layer_kind: RustArtifactBundleLayerKind,
    #[serde(default)]
    pub base_cache_key: Option<String>,
    #[serde(default)]
    pub deleted_paths: Vec<String>,
}

fn default_bundle_layer_kind() -> RustArtifactBundleLayerKind {
    RustArtifactBundleLayerKind::Complete
}

pub(super) fn write_bundle_manifest(
    bundle_dir: &Path,
    manifest: &RustArtifactBundleManifest,
) -> Result<(), RustPlanError> {
    let mut bytes = Vec::new();
    manifest_to_proto(manifest).encode(&mut bytes)?;
    std::fs::write(bundle_dir.join(BUNDLE_MANIFEST_NAME), bytes)?;
    let legacy_manifest = bundle_dir.join(LEGACY_BUNDLE_MANIFEST_NAME);
    if legacy_manifest.exists() {
        std::fs::remove_file(legacy_manifest)?;
    }
    Ok(())
}

pub(super) fn read_bundle_manifest(
    bundle_dir: &Path,
) -> Result<RustArtifactBundleManifest, RustPlanError> {
    let manifest_path = bundle_dir.join(BUNDLE_MANIFEST_NAME);
    if manifest_path.exists() {
        let proto = rust_plan_proto::RustArtifactBundleManifest::decode(
            std::fs::read(manifest_path)?.as_slice(),
        )?;
        return manifest_from_proto(proto);
    }

    let legacy_manifest_path = bundle_dir.join(LEGACY_BUNDLE_MANIFEST_NAME);
    let manifest: RustArtifactBundleManifest =
        serde_json::from_slice(&std::fs::read(legacy_manifest_path)?)?;
    Ok(manifest)
}

/// Compute the stable cache key for a plan.
pub(super) fn validate_manifest(
    plan: &RustArtifactPlanV1,
    cache_key: &str,
    manifest: &RustArtifactBundleManifest,
    summary: &mut RustPlanSummary,
) -> Result<bool, RustPlanError> {
    if manifest.manifest_schema_version != RUST_ARTIFACT_CACHE_SCHEMA_VERSION {
        return Err(RustPlanError::UnsupportedCacheSchemaVersion {
            found: manifest.manifest_schema_version,
            supported: RUST_ARTIFACT_CACHE_SCHEMA_VERSION,
        });
    }
    let mut compatible = true;
    if manifest.cache_key != cache_key {
        summary
            .key_input_mismatches
            .push("bundle cache key does not match requested plan".to_string());
        compatible = false;
    }
    if manifest.mode != plan.mode {
        summary
            .key_input_mismatches
            .push("bundle mode does not match requested plan".to_string());
        compatible = false;
    }
    let plan_identity_hash = rust_plan_identity_hash(plan);
    if manifest.plan_identity_hash != plan_identity_hash {
        summary
            .key_input_mismatches
            .push("bundle input hash does not match requested plan".to_string());
        compatible = false;
    }
    if manifest.layer_kind == RustArtifactBundleLayerKind::Delta
        && manifest
            .base_cache_key
            .as_deref()
            .is_some_and(|base_cache_key| base_cache_key != cache_key)
    {
        summary
            .key_input_mismatches
            .push("delta base cache key does not match requested plan".to_string());
        compatible = false;
    }
    if compatible {
        Ok(true)
    } else {
        summary.compatibility.status = "warning".to_string();
        summary.compatibility.errors = summary.key_input_mismatches.clone();
        Ok(false)
    }
}

pub(super) fn safe_join(root: &Path, relative: &str) -> Result<NormalizedPath, RustPlanError> {
    let rel = Path::new(relative);
    if rel.as_os_str().is_empty() {
        return Err(RustPlanError::UnsafeRelativePath(relative.to_string()));
    }
    for component in rel.components() {
        match component {
            Component::Normal(_) | Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(RustPlanError::UnsafeRelativePath(relative.to_string()));
            }
        }
    }
    Ok(NormalizedPath::new(root.join(rel)))
}
