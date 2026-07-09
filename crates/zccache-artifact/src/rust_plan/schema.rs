//! Rust artifact plan schema, validation, and wire-facing types.

use std::collections::BTreeSet;
use std::path::Path;

use prost::Message;
use serde::{Deserialize, Serialize};
use zccache_core::NormalizedPath;

use super::proto::{plan_from_proto, plan_to_proto, rust_plan_proto};

/// Supported Rust artifact plan schema version.
pub const RUST_ARTIFACT_PLAN_SCHEMA_VERSION: u32 = 1;
/// Supported cache bundle schema versions soldr may send. v1 is the legacy
/// shape (thin-v1 / full). v2 is the `thin-v2` opt-in described in soldr#461:
/// it adds the `cache_profile` and `dropped_artifact_classes` fields and
/// splits the legacy `cargo_fingerprint` class into `cargo_fingerprint_meta`
/// (kept) and `cargo_fingerprint_outputs` (dropped). zccache accepts both so
/// older soldr builds keep working unchanged.
pub const SUPPORTED_RUST_ARTIFACT_CACHE_SCHEMA_VERSIONS: &[u32] = &[1, 2];
/// Cache schema version zccache writes into bundle manifests it creates.
/// Pinned at 1 so the on-disk manifest format stays stable across the
/// thin-v2 opt-in â€” the v2 wire fields are inputs to the save walker, not
/// outputs in the manifest itself.
pub const RUST_ARTIFACT_CACHE_SCHEMA_VERSION: u32 = 1;

/// Errors returned by plan loading and execution.
#[derive(Debug, thiserror::Error)]
pub enum RustPlanError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("protobuf encode error: {0}")]
    ProtobufEncode(#[from] prost::EncodeError),
    #[error("protobuf decode error: {0}")]
    ProtobufDecode(#[from] prost::DecodeError),
    #[error(
        "unsupported Rust artifact plan schema version {found}; supported version is {supported}"
    )]
    UnsupportedSchemaVersion { found: u32, supported: u32 },
    #[error(
        "unsupported Rust artifact cache schema version {found}; supported version is {supported}"
    )]
    UnsupportedCacheSchemaVersion { found: u32, supported: u32 },
    #[error("invalid Rust artifact plan: {0}")]
    InvalidPlan(String),
    #[error("Rust artifact bundle is missing: {0}")]
    BundleMissing(NormalizedPath),
    #[error("invalid Rust artifact bundle manifest: {0}")]
    InvalidManifest(String),
    #[error("unsafe relative artifact path in bundle: {0}")]
    UnsafeRelativePath(String),
}

/// Rust artifact plan mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RustPlanMode {
    /// Restore/save bounded dependency artifacts and Cargo freshness metadata.
    Thin,
    /// Restore/save the target tree explicitly, except transient state.
    Full,
}

impl std::fmt::Display for RustPlanMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Thin => write!(f, "thin"),
            Self::Full => write!(f, "full"),
        }
    }
}

/// Artifact classes that a plan may allow.
///
/// New variants added for the soldr `thin-v2` profile (soldr#461):
/// `CargoFingerprintMeta` / `CargoFingerprintOutputs` split the legacy
/// `CargoFingerprint` umbrella into freshness inputs vs. outputs;
/// `Incremental` / `BuildScriptBuild` / `Dwo` / `Pdb` / `Dsym` enumerate
/// categories that thin-v2 explicitly drops so the wire-format
/// `dropped_artifact_classes` list can name them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RustArtifactClass {
    Rlib,
    Rmeta,
    DepInfo,
    ProcMacro,
    SharedLib,
    /// Legacy thin-v1 umbrella over the `.fingerprint/<crate>/` directory.
    /// Kept for backwards compatibility; thin-v2 splits this into
    /// `CargoFingerprintMeta` (freshness inputs cargo reads) and
    /// `CargoFingerprintOutputs` (everything else in that directory).
    CargoFingerprint,
    /// Freshness-input files inside `<profile>/.fingerprint/<crate>-<hash>/`:
    /// `invoked.timestamp`, `dep-*`, `output-*`, `lib-*`, `bin-*`. Cargo
    /// reads these to decide skip-vs-rebuild, so thin-v2 keeps them.
    CargoFingerprintMeta,
    /// Non-meta files inside `<profile>/.fingerprint/<crate>-<hash>/`. Dropped
    /// by thin-v2 â€” they are outputs of past compilations, not inputs to the
    /// next freshness decision.
    CargoFingerprintOutputs,
    BuildScriptMetadata,
    BuildScriptOutput,
    /// Compiled build-script binary at `target/<profile>/build/*/build-script-build*`.
    /// thin-v2 drops these â€” they are cheap to regenerate from cached deps.
    BuildScriptBuild,
    /// Anything under `target/<profile>/incremental/`. Always transient state;
    /// thin-v2 names it explicitly so the drop list is exhaustive.
    Incremental,
    /// `.dwo` split-DWARF files under `deps/`. Dropped by thin-v2.
    Dwo,
    /// `.pdb` Windows debug files under `deps/`. Dropped by thin-v2.
    Pdb,
    /// `.dSYM/` macOS debug bundles under `deps/` (directory bundles â€” every
    /// file inside the bundle is classified as `Dsym`). Dropped by thin-v2.
    Dsym,
    FullTarget,
}

/// Toolchain identity supplied by soldr.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RustToolchainIdentity {
    pub rustc: String,
    pub cargo: String,
    pub channel: String,
    pub host: String,
}

/// Input hashes that affect Cargo build outputs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RustPlanInputs {
    pub features_hash: String,
    pub rustflags_hash: String,
    pub env_hash: String,
    pub lockfile_hash: String,
    pub cargo_config_hash: String,
    #[serde(default)]
    pub manifest_hashes: Vec<String>,
}

/// Package IDs selected or excluded by the planner.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RustPlanPackages {
    #[serde(default)]
    pub selected_package_ids: Vec<String>,
    #[serde(default)]
    pub workspace_package_ids: Vec<String>,
    #[serde(default)]
    pub excluded_path_package_ids: Vec<String>,
}

/// Versioned v1 Rust artifact cache plan.
///
/// Wire-compat note (soldr#461): the previous version used
/// `#[serde(deny_unknown_fields)]`, which made every future soldr addition
/// a coordinated breaking change. The `thin-v2` rollout added
/// `cache_profile` and `dropped_artifact_classes` as new top-level fields,
/// so we now accept (and ignore) unknown fields here. The explicit fields
/// below cover everything zccache acts on; anything else soldr ships in a
/// future plan is silently dropped during deserialization rather than
/// crashing the save/restore.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RustArtifactPlanV1 {
    pub schema_version: u32,
    pub mode: RustPlanMode,
    pub workspace_root: NormalizedPath,
    pub target_dir: NormalizedPath,
    pub toolchain: RustToolchainIdentity,
    pub target_triple: String,
    pub profile: String,
    pub inputs: RustPlanInputs,
    pub packages: RustPlanPackages,
    #[serde(default)]
    pub allowed_artifact_classes: Vec<RustArtifactClass>,
    pub cache_schema_version: u32,
    #[serde(default)]
    pub journal_log_path: Option<NormalizedPath>,
    /// Thin-slice pruning profile selected by soldr. `None` for legacy plans
    /// (thin-v1 / full) that predate soldr#461; `Some("thin-v2")` opts in to
    /// the fingerprint-aware prune.
    #[serde(default)]
    pub cache_profile: Option<String>,
    /// Artifact classes soldr explicitly wants dropped from the saved bundle.
    /// Honored regardless of what `allowed_artifact_classes` says â€” a file
    /// whose class appears here is skipped even if its class is also in the
    /// allow-list. Empty for legacy plans.
    #[serde(default)]
    pub dropped_artifact_classes: Vec<RustArtifactClass>,
}

impl RustArtifactPlanV1 {
    /// Load, version-check, and validate a plan from a protobuf or legacy JSON file.
    pub fn load(path: &Path) -> Result<Self, RustPlanError> {
        let raw = std::fs::read(path)?;
        if is_probably_json_plan(&raw) {
            let raw = std::str::from_utf8(&raw).map_err(|err| {
                RustPlanError::InvalidPlan(format!("JSON plan is not valid UTF-8: {err}"))
            })?;
            return Self::from_json_str(raw);
        }
        Self::from_proto_bytes(&raw)
    }

    /// Load, version-check, and validate a plan from a JSON string.
    pub fn from_json_str(raw: &str) -> Result<Self, RustPlanError> {
        let value: serde_json::Value = serde_json::from_str(raw.trim_start_matches('\u{feff}'))?;
        Self::from_json_value(value)
    }

    /// Load, version-check, and validate a plan from protobuf bytes.
    pub fn from_proto_bytes(raw: &[u8]) -> Result<Self, RustPlanError> {
        let proto = rust_plan_proto::RustArtifactPlanV1::decode(raw)?;
        plan_from_proto(proto)
    }

    /// Encode this plan as compact protobuf bytes.
    pub fn to_proto_bytes(&self) -> Result<Vec<u8>, RustPlanError> {
        let mut bytes = Vec::new();
        plan_to_proto(self).encode(&mut bytes)?;
        Ok(bytes)
    }

    /// Load, version-check, and validate a plan from a JSON value.
    pub fn from_json_value(value: serde_json::Value) -> Result<Self, RustPlanError> {
        let schema_version = json_u32_field(&value, "schema_version")?;
        if schema_version != RUST_ARTIFACT_PLAN_SCHEMA_VERSION {
            return Err(RustPlanError::UnsupportedSchemaVersion {
                found: schema_version,
                supported: RUST_ARTIFACT_PLAN_SCHEMA_VERSION,
            });
        }

        let cache_schema_version = json_u32_field(&value, "cache_schema_version")?;
        ensure_supported_cache_schema_version(cache_schema_version)?;

        let plan: Self = serde_json::from_value(value)?;
        plan.validate()?;
        Ok(plan)
    }

    /// Validate fields whose constraints are outside serde's type checks.
    pub fn validate(&self) -> Result<(), RustPlanError> {
        let mut errors = Vec::new();
        if self.profile.trim().is_empty() {
            errors.push("profile must not be empty");
        }
        if self.target_triple.trim().is_empty() {
            errors.push("target_triple must not be empty");
        }
        if self.toolchain.rustc.trim().is_empty() {
            errors.push("toolchain.rustc must not be empty");
        }
        if self.toolchain.cargo.trim().is_empty() {
            errors.push("toolchain.cargo must not be empty");
        }
        if self.toolchain.channel.trim().is_empty() {
            errors.push("toolchain.channel must not be empty");
        }
        if self.toolchain.host.trim().is_empty() {
            errors.push("toolchain.host must not be empty");
        }
        if self.workspace_root.as_os_str().is_empty() {
            errors.push("workspace_root must not be empty");
        }
        if self.target_dir.as_os_str().is_empty() {
            errors.push("target_dir must not be empty");
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(RustPlanError::InvalidPlan(errors.join("; ")))
        }
    }

    /// Effective allowed classes for thin mode.
    #[must_use]
    pub fn effective_allowed_classes(&self) -> BTreeSet<RustArtifactClass> {
        if self.allowed_artifact_classes.is_empty() {
            default_thin_classes()
        } else {
            self.allowed_artifact_classes.iter().copied().collect()
        }
    }
}

pub(super) fn is_probably_json_plan(raw: &[u8]) -> bool {
    let without_bom = raw.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(raw);
    without_bom
        .iter()
        .copied()
        .find(|byte| !byte.is_ascii_whitespace())
        .is_some_and(|byte| byte == b'{')
}

pub(super) fn default_thin_classes() -> BTreeSet<RustArtifactClass> {
    [
        RustArtifactClass::Rlib,
        RustArtifactClass::Rmeta,
        RustArtifactClass::DepInfo,
        RustArtifactClass::ProcMacro,
        RustArtifactClass::SharedLib,
        RustArtifactClass::CargoFingerprint,
        RustArtifactClass::BuildScriptMetadata,
        RustArtifactClass::BuildScriptOutput,
    ]
    .into_iter()
    .collect()
}

pub(super) fn json_u32_field(
    value: &serde_json::Value,
    field: &'static str,
) -> Result<u32, RustPlanError> {
    let Some(raw) = value.get(field) else {
        return Err(RustPlanError::InvalidPlan(format!("{field} is required")));
    };
    let Some(n) = raw.as_u64() else {
        return Err(RustPlanError::InvalidPlan(format!(
            "{field} must be an unsigned integer"
        )));
    };
    u32::try_from(n).map_err(|_| RustPlanError::InvalidPlan(format!("{field} is too large")))
}

pub(super) fn ensure_supported_cache_schema_version(
    cache_schema_version: u32,
) -> Result<(), RustPlanError> {
    if SUPPORTED_RUST_ARTIFACT_CACHE_SCHEMA_VERSIONS.contains(&cache_schema_version) {
        Ok(())
    } else {
        // Report the most recent supported version in the error message so
        // operators see "expected 2" once thin-v2 lands instead of always
        // seeing the legacy "expected 1". The error type still carries the
        // single canonical version field for compat with existing
        // pattern-matching consumers.
        let supported = SUPPORTED_RUST_ARTIFACT_CACHE_SCHEMA_VERSIONS
            .iter()
            .copied()
            .max()
            .unwrap_or(RUST_ARTIFACT_CACHE_SCHEMA_VERSION);
        Err(RustPlanError::UnsupportedCacheSchemaVersion {
            found: cache_schema_version,
            supported,
        })
    }
}
