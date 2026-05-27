//! Protobuf structs and conversions for Rust plans and bundle manifests.

use crate::core::NormalizedPath;

use super::manifest::{
    RustArtifactBundleLayerKind, RustArtifactBundleManifest, RustBundledArtifact,
};
use super::schema::{
    ensure_supported_cache_schema_version, RustArtifactClass, RustArtifactPlanV1, RustPlanError,
    RustPlanInputs, RustPlanMode, RustPlanPackages, RustToolchainIdentity,
    RUST_ARTIFACT_PLAN_SCHEMA_VERSION,
};

pub(super) mod rust_plan_proto {
    #[derive(Clone, PartialEq, ::prost::Message)]
    pub struct RustArtifactPlanV1 {
        #[prost(uint32, tag = "1")]
        pub schema_version: u32,
        #[prost(uint32, tag = "2")]
        pub mode: u32,
        #[prost(string, tag = "3")]
        pub workspace_root: String,
        #[prost(string, tag = "4")]
        pub target_dir: String,
        #[prost(message, optional, tag = "5")]
        pub toolchain: Option<RustToolchainIdentity>,
        #[prost(string, tag = "6")]
        pub target_triple: String,
        #[prost(string, tag = "7")]
        pub profile: String,
        #[prost(message, optional, tag = "8")]
        pub inputs: Option<RustPlanInputs>,
        #[prost(message, optional, tag = "9")]
        pub packages: Option<RustPlanPackages>,
        #[prost(uint32, repeated, tag = "10")]
        pub allowed_artifact_classes: Vec<u32>,
        #[prost(uint32, tag = "11")]
        pub cache_schema_version: u32,
        #[prost(string, tag = "12")]
        pub journal_log_path: String,
        #[prost(string, tag = "13")]
        pub cache_profile: String,
        #[prost(uint32, repeated, tag = "14")]
        pub dropped_artifact_classes: Vec<u32>,
    }

    #[derive(Clone, PartialEq, ::prost::Message)]
    pub struct RustToolchainIdentity {
        #[prost(string, tag = "1")]
        pub rustc: String,
        #[prost(string, tag = "2")]
        pub cargo: String,
        #[prost(string, tag = "3")]
        pub channel: String,
        #[prost(string, tag = "4")]
        pub host: String,
    }

    #[derive(Clone, PartialEq, ::prost::Message)]
    pub struct RustPlanInputs {
        #[prost(string, tag = "1")]
        pub features_hash: String,
        #[prost(string, tag = "2")]
        pub rustflags_hash: String,
        #[prost(string, tag = "3")]
        pub env_hash: String,
        #[prost(string, tag = "4")]
        pub lockfile_hash: String,
        #[prost(string, tag = "5")]
        pub cargo_config_hash: String,
        #[prost(string, repeated, tag = "6")]
        pub manifest_hashes: Vec<String>,
    }

    #[derive(Clone, PartialEq, ::prost::Message)]
    pub struct RustPlanPackages {
        #[prost(string, repeated, tag = "1")]
        pub selected_package_ids: Vec<String>,
        #[prost(string, repeated, tag = "2")]
        pub workspace_package_ids: Vec<String>,
        #[prost(string, repeated, tag = "3")]
        pub excluded_path_package_ids: Vec<String>,
    }

    #[derive(Clone, PartialEq, ::prost::Message)]
    pub struct RustArtifactBundleManifest {
        #[prost(uint32, tag = "1")]
        pub manifest_schema_version: u32,
        #[prost(uint32, tag = "2")]
        pub plan_schema_version: u32,
        #[prost(uint32, tag = "3")]
        pub cache_schema_version: u32,
        #[prost(uint32, tag = "4")]
        pub mode: u32,
        #[prost(string, tag = "5")]
        pub cache_key: String,
        #[prost(uint64, tag = "6")]
        pub created_at_secs: u64,
        #[prost(string, tag = "7")]
        pub plan_identity_hash: String,
        #[prost(message, repeated, tag = "8")]
        pub artifacts: Vec<RustBundledArtifact>,
        #[prost(uint32, tag = "9")]
        pub layer_kind: u32,
        #[prost(string, tag = "10")]
        pub base_cache_key: String,
        #[prost(string, repeated, tag = "11")]
        pub deleted_paths: Vec<String>,
    }

    #[derive(Clone, PartialEq, ::prost::Message)]
    pub struct RustBundledArtifact {
        #[prost(string, tag = "1")]
        pub relative_path: String,
        #[prost(uint32, tag = "2")]
        pub class: u32,
        #[prost(uint64, tag = "3")]
        pub size: u64,
        #[prost(string, tag = "4")]
        pub content_hash: String,
        #[prost(uint64, tag = "5")]
        pub mtime_unix_nanos: u64,
    }
}

fn plan_mode_to_proto(mode: RustPlanMode) -> u32 {
    match mode {
        RustPlanMode::Thin => 1,
        RustPlanMode::Full => 2,
    }
}

fn plan_mode_from_proto(value: u32) -> Result<RustPlanMode, RustPlanError> {
    match value {
        1 => Ok(RustPlanMode::Thin),
        2 => Ok(RustPlanMode::Full),
        other => Err(RustPlanError::InvalidManifest(format!(
            "unknown plan mode code {other}"
        ))),
    }
}

fn artifact_class_to_proto(class: RustArtifactClass) -> u32 {
    match class {
        RustArtifactClass::Rlib => 1,
        RustArtifactClass::Rmeta => 2,
        RustArtifactClass::DepInfo => 3,
        RustArtifactClass::ProcMacro => 4,
        RustArtifactClass::SharedLib => 5,
        RustArtifactClass::CargoFingerprint => 6,
        RustArtifactClass::CargoFingerprintMeta => 7,
        RustArtifactClass::CargoFingerprintOutputs => 8,
        RustArtifactClass::BuildScriptMetadata => 9,
        RustArtifactClass::BuildScriptOutput => 10,
        RustArtifactClass::BuildScriptBuild => 11,
        RustArtifactClass::Incremental => 12,
        RustArtifactClass::Dwo => 13,
        RustArtifactClass::Pdb => 14,
        RustArtifactClass::Dsym => 15,
        RustArtifactClass::FullTarget => 16,
    }
}

fn artifact_class_from_proto(value: u32) -> Result<RustArtifactClass, RustPlanError> {
    match value {
        1 => Ok(RustArtifactClass::Rlib),
        2 => Ok(RustArtifactClass::Rmeta),
        3 => Ok(RustArtifactClass::DepInfo),
        4 => Ok(RustArtifactClass::ProcMacro),
        5 => Ok(RustArtifactClass::SharedLib),
        6 => Ok(RustArtifactClass::CargoFingerprint),
        7 => Ok(RustArtifactClass::CargoFingerprintMeta),
        8 => Ok(RustArtifactClass::CargoFingerprintOutputs),
        9 => Ok(RustArtifactClass::BuildScriptMetadata),
        10 => Ok(RustArtifactClass::BuildScriptOutput),
        11 => Ok(RustArtifactClass::BuildScriptBuild),
        12 => Ok(RustArtifactClass::Incremental),
        13 => Ok(RustArtifactClass::Dwo),
        14 => Ok(RustArtifactClass::Pdb),
        15 => Ok(RustArtifactClass::Dsym),
        16 => Ok(RustArtifactClass::FullTarget),
        other => Err(RustPlanError::InvalidManifest(format!(
            "unknown artifact class code {other}"
        ))),
    }
}

fn layer_kind_to_proto(kind: RustArtifactBundleLayerKind) -> u32 {
    match kind {
        RustArtifactBundleLayerKind::Complete => 0,
        RustArtifactBundleLayerKind::Base => 1,
        RustArtifactBundleLayerKind::Delta => 2,
    }
}

fn layer_kind_from_proto(value: u32) -> Result<RustArtifactBundleLayerKind, RustPlanError> {
    match value {
        0 => Ok(RustArtifactBundleLayerKind::Complete),
        1 => Ok(RustArtifactBundleLayerKind::Base),
        2 => Ok(RustArtifactBundleLayerKind::Delta),
        other => Err(RustPlanError::InvalidManifest(format!(
            "unknown layer kind code {other}"
        ))),
    }
}

fn normalized_path_to_proto(path: &NormalizedPath) -> String {
    path.as_path().to_string_lossy().into_owned()
}

fn optional_normalized_path_to_proto(path: &Option<NormalizedPath>) -> String {
    path.as_ref()
        .map(normalized_path_to_proto)
        .unwrap_or_default()
}

fn optional_normalized_path_from_proto(raw: String) -> Option<NormalizedPath> {
    if raw.is_empty() {
        None
    } else {
        Some(NormalizedPath::new(raw))
    }
}

fn optional_string_from_proto(raw: String) -> Option<String> {
    if raw.is_empty() {
        None
    } else {
        Some(raw)
    }
}

pub(super) fn plan_to_proto(plan: &RustArtifactPlanV1) -> rust_plan_proto::RustArtifactPlanV1 {
    rust_plan_proto::RustArtifactPlanV1 {
        schema_version: plan.schema_version,
        mode: plan_mode_to_proto(plan.mode),
        workspace_root: normalized_path_to_proto(&plan.workspace_root),
        target_dir: normalized_path_to_proto(&plan.target_dir),
        toolchain: Some(rust_plan_proto::RustToolchainIdentity {
            rustc: plan.toolchain.rustc.clone(),
            cargo: plan.toolchain.cargo.clone(),
            channel: plan.toolchain.channel.clone(),
            host: plan.toolchain.host.clone(),
        }),
        target_triple: plan.target_triple.clone(),
        profile: plan.profile.clone(),
        inputs: Some(rust_plan_proto::RustPlanInputs {
            features_hash: plan.inputs.features_hash.clone(),
            rustflags_hash: plan.inputs.rustflags_hash.clone(),
            env_hash: plan.inputs.env_hash.clone(),
            lockfile_hash: plan.inputs.lockfile_hash.clone(),
            cargo_config_hash: plan.inputs.cargo_config_hash.clone(),
            manifest_hashes: plan.inputs.manifest_hashes.clone(),
        }),
        packages: Some(rust_plan_proto::RustPlanPackages {
            selected_package_ids: plan.packages.selected_package_ids.clone(),
            workspace_package_ids: plan.packages.workspace_package_ids.clone(),
            excluded_path_package_ids: plan.packages.excluded_path_package_ids.clone(),
        }),
        allowed_artifact_classes: plan
            .allowed_artifact_classes
            .iter()
            .copied()
            .map(artifact_class_to_proto)
            .collect(),
        cache_schema_version: plan.cache_schema_version,
        journal_log_path: optional_normalized_path_to_proto(&plan.journal_log_path),
        cache_profile: plan.cache_profile.clone().unwrap_or_default(),
        dropped_artifact_classes: plan
            .dropped_artifact_classes
            .iter()
            .copied()
            .map(artifact_class_to_proto)
            .collect(),
    }
}

pub(super) fn plan_from_proto(
    proto: rust_plan_proto::RustArtifactPlanV1,
) -> Result<RustArtifactPlanV1, RustPlanError> {
    if proto.schema_version != RUST_ARTIFACT_PLAN_SCHEMA_VERSION {
        return Err(RustPlanError::UnsupportedSchemaVersion {
            found: proto.schema_version,
            supported: RUST_ARTIFACT_PLAN_SCHEMA_VERSION,
        });
    }
    ensure_supported_cache_schema_version(proto.cache_schema_version)?;

    let toolchain = proto
        .toolchain
        .ok_or_else(|| RustPlanError::InvalidPlan("toolchain is required".to_string()))?;
    let inputs = proto
        .inputs
        .ok_or_else(|| RustPlanError::InvalidPlan("inputs are required".to_string()))?;
    let packages = proto.packages.unwrap_or_default();
    let plan = RustArtifactPlanV1 {
        schema_version: proto.schema_version,
        mode: plan_mode_from_proto(proto.mode)?,
        workspace_root: NormalizedPath::new(proto.workspace_root),
        target_dir: NormalizedPath::new(proto.target_dir),
        toolchain: RustToolchainIdentity {
            rustc: toolchain.rustc,
            cargo: toolchain.cargo,
            channel: toolchain.channel,
            host: toolchain.host,
        },
        target_triple: proto.target_triple,
        profile: proto.profile,
        inputs: RustPlanInputs {
            features_hash: inputs.features_hash,
            rustflags_hash: inputs.rustflags_hash,
            env_hash: inputs.env_hash,
            lockfile_hash: inputs.lockfile_hash,
            cargo_config_hash: inputs.cargo_config_hash,
            manifest_hashes: inputs.manifest_hashes,
        },
        packages: RustPlanPackages {
            selected_package_ids: packages.selected_package_ids,
            workspace_package_ids: packages.workspace_package_ids,
            excluded_path_package_ids: packages.excluded_path_package_ids,
        },
        allowed_artifact_classes: proto
            .allowed_artifact_classes
            .into_iter()
            .map(artifact_class_from_proto)
            .collect::<Result<Vec<_>, RustPlanError>>()?,
        cache_schema_version: proto.cache_schema_version,
        journal_log_path: optional_normalized_path_from_proto(proto.journal_log_path),
        cache_profile: optional_string_from_proto(proto.cache_profile),
        dropped_artifact_classes: proto
            .dropped_artifact_classes
            .into_iter()
            .map(artifact_class_from_proto)
            .collect::<Result<Vec<_>, RustPlanError>>()?,
    };
    plan.validate()?;
    Ok(plan)
}

pub(super) fn manifest_to_proto(
    manifest: &RustArtifactBundleManifest,
) -> rust_plan_proto::RustArtifactBundleManifest {
    rust_plan_proto::RustArtifactBundleManifest {
        manifest_schema_version: manifest.manifest_schema_version,
        plan_schema_version: manifest.plan_schema_version,
        cache_schema_version: manifest.cache_schema_version,
        mode: plan_mode_to_proto(manifest.mode),
        cache_key: manifest.cache_key.clone(),
        created_at_secs: manifest.created_at_secs,
        plan_identity_hash: manifest.plan_identity_hash.clone(),
        artifacts: manifest
            .artifacts
            .iter()
            .map(|artifact| rust_plan_proto::RustBundledArtifact {
                relative_path: artifact.relative_path.clone(),
                class: artifact_class_to_proto(artifact.class),
                size: artifact.size,
                content_hash: artifact.content_hash.clone(),
                mtime_unix_nanos: artifact.mtime_unix_nanos,
            })
            .collect(),
        layer_kind: layer_kind_to_proto(manifest.layer_kind),
        base_cache_key: manifest.base_cache_key.clone().unwrap_or_default(),
        deleted_paths: manifest.deleted_paths.clone(),
    }
}

pub(super) fn manifest_from_proto(
    proto: rust_plan_proto::RustArtifactBundleManifest,
) -> Result<RustArtifactBundleManifest, RustPlanError> {
    Ok(RustArtifactBundleManifest {
        manifest_schema_version: proto.manifest_schema_version,
        plan_schema_version: proto.plan_schema_version,
        cache_schema_version: proto.cache_schema_version,
        mode: plan_mode_from_proto(proto.mode)?,
        cache_key: proto.cache_key,
        created_at_secs: proto.created_at_secs,
        plan_identity_hash: proto.plan_identity_hash,
        artifacts: proto
            .artifacts
            .into_iter()
            .map(|artifact| {
                Ok(RustBundledArtifact {
                    relative_path: artifact.relative_path,
                    class: artifact_class_from_proto(artifact.class)?,
                    size: artifact.size,
                    content_hash: artifact.content_hash,
                    mtime_unix_nanos: artifact.mtime_unix_nanos,
                })
            })
            .collect::<Result<Vec<_>, RustPlanError>>()?,
        layer_kind: layer_kind_from_proto(proto.layer_kind)?,
        base_cache_key: if proto.base_cache_key.is_empty() {
            None
        } else {
            Some(proto.base_cache_key)
        },
        deleted_paths: proto.deleted_paths,
    })
}
