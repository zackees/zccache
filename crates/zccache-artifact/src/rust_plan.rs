//! Plan-driven Rust target artifact save/restore.

#[cfg(feature = "gha")]
mod gha;
mod local;
mod manifest;
mod proto;
mod schema;
mod selection;
mod summary;
#[cfg(any(feature = "cli", feature = "gha"))]
mod targz;
mod threads;

#[cfg(test)]
mod tests;

#[cfg(feature = "gha")]
pub use gha::{restore_rust_plan_gha, rust_plan_gha_version, save_rust_plan_gha, RustPlanGhaError};
pub use local::{
    restore_rust_plan_layered_local, restore_rust_plan_local, rust_plan_bundle_dir,
    rust_plan_cache_key, save_rust_plan_delta_local, save_rust_plan_local,
};
pub use manifest::{RustArtifactBundleLayerKind, RustArtifactBundleManifest, RustBundledArtifact};
pub use schema::{
    RustArtifactClass, RustArtifactPlanV1, RustPlanError, RustPlanInputs, RustPlanMode,
    RustPlanPackages, RustToolchainIdentity, RUST_ARTIFACT_CACHE_SCHEMA_VERSION,
    RUST_ARTIFACT_PLAN_SCHEMA_VERSION,
};
pub use summary::{
    RustPlanArtifactEffectiveness, RustPlanCompatibility, RustPlanOperation, RustPlanSkippedSample,
    RustPlanSummary,
};
#[cfg(feature = "cli")]
pub use targz::{tar_gz_decode, tar_gz_encode};

#[cfg(test)]
use local::{
    bundle_selected_artifacts_with_threads, rust_plan_identity_hash, system_time_to_unix_nanos,
    unix_nanos_to_system_time,
};
#[cfg(test)]
use manifest::{
    read_bundle_manifest, safe_join, write_bundle_manifest, BUNDLE_FILES_DIR, BUNDLE_MANIFEST_NAME,
    LEGACY_BUNDLE_MANIFEST_NAME,
};
#[cfg(test)]
use schema::default_thin_classes;
#[cfg(test)]
use selection::{classify_artifact, collect_files, package_name_from_id, select_artifacts};
#[cfg(test)]
use threads::{
    default_rust_plan_tar_threads, parse_rust_plan_tar_threads, DEFAULT_RUST_PLAN_TAR_THREADS_CAP,
    MAX_RUST_PLAN_TAR_THREADS,
};
