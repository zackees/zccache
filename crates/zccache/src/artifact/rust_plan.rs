//! Plan-driven Rust target artifact save/restore.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::path::{Component, Path};
use std::time::{SystemTime, UNIX_EPOCH};

use rayon::prelude::*;
use serde::ser::SerializeStruct;
use serde::{Deserialize, Serialize};
use zccache::core::{normalize_for_key, NormalizedPath};

/// Upper bound on save-time worker threads. Beyond this Windows filter-driver
/// serialization dominates and extra threads stop helping (see issue #177 and
/// the linked soldr#272 analysis).
const DEFAULT_RUST_PLAN_TAR_THREADS_CAP: usize = 8;
/// Hard upper bound regardless of caller request — protects small runners from
/// per-thread buffer blowup if someone passes a huge value.
const MAX_RUST_PLAN_TAR_THREADS: usize = 64;

/// Supported Rust artifact plan schema version.
pub const RUST_ARTIFACT_PLAN_SCHEMA_VERSION: u32 = 1;
/// Supported cache bundle schema version.
pub const RUST_ARTIFACT_CACHE_SCHEMA_VERSION: u32 = 1;

const BUNDLE_MANIFEST_NAME: &str = "manifest.json";
const BUNDLE_FILES_DIR: &str = "files";

/// Errors returned by plan loading and execution.
#[derive(Debug, thiserror::Error)]
pub enum RustPlanError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RustArtifactClass {
    Rlib,
    Rmeta,
    DepInfo,
    ProcMacro,
    SharedLib,
    CargoFingerprint,
    BuildScriptMetadata,
    BuildScriptOutput,
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
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
}

impl RustArtifactPlanV1 {
    /// Load, version-check, and validate a plan from a JSON file.
    pub fn load(path: &Path) -> Result<Self, RustPlanError> {
        let raw = std::fs::read_to_string(path)?;
        Self::from_json_str(&raw)
    }

    /// Load, version-check, and validate a plan from a JSON string.
    pub fn from_json_str(raw: &str) -> Result<Self, RustPlanError> {
        let value: serde_json::Value = serde_json::from_str(raw.trim_start_matches('\u{feff}'))?;
        Self::from_json_value(value)
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
        if cache_schema_version != RUST_ARTIFACT_CACHE_SCHEMA_VERSION {
            return Err(RustPlanError::UnsupportedCacheSchemaVersion {
                found: cache_schema_version,
                supported: RUST_ARTIFACT_CACHE_SCHEMA_VERSION,
            });
        }

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

/// Backend operation represented in summaries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RustPlanOperation {
    Validate,
    Restore,
    Save,
}

/// Compatibility section in a machine-readable summary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RustPlanCompatibility {
    pub status: String,
    #[serde(default)]
    pub errors: Vec<String>,
}

/// Representative skipped artifact.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RustPlanSkippedSample {
    pub path: String,
    pub reason: String,
}

/// Artifact restore effectiveness, independent from compile-cache hit rate.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RustPlanArtifactEffectiveness {
    pub eligible_file_count: u64,
    pub restored_file_count: u64,
    pub reuse_ratio: f64,
}

/// Machine-readable operation summary for soldr/setup-soldr.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct RustPlanSummary {
    pub operation: RustPlanOperation,
    pub mode: RustPlanMode,
    pub plan_schema_version: u32,
    pub cache_schema_version: u32,
    pub compatibility: RustPlanCompatibility,
    pub restored_file_count: u64,
    pub restored_bytes: u64,
    pub saved_file_count: u64,
    pub saved_bytes: u64,
    pub skipped_count: u64,
    #[serde(default)]
    pub skipped_reasons: BTreeMap<String, u64>,
    #[serde(default)]
    pub skipped_samples: Vec<RustPlanSkippedSample>,
    #[serde(default)]
    pub key_input_mismatches: Vec<String>,
    #[serde(default)]
    pub miss_classifications: BTreeMap<String, u64>,
    pub backend: String,
    pub cache_key: String,
    pub backend_cache_key: Option<String>,
    pub backend_cache_version: Option<String>,
    pub archive_path: Option<NormalizedPath>,
    pub journal_log_path: Option<NormalizedPath>,
    pub target_artifact_effectiveness: RustPlanArtifactEffectiveness,
    pub compile_cache_stats: Option<serde_json::Value>,
}

impl Serialize for RustPlanSummary {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let miss_classifications = self.computed_miss_classifications();
        let mut state = serializer.serialize_struct("RustPlanSummary", 22)?;
        state.serialize_field("operation", &self.operation)?;
        state.serialize_field("mode", &self.mode)?;
        state.serialize_field("plan_schema_version", &self.plan_schema_version)?;
        state.serialize_field("cache_schema_version", &self.cache_schema_version)?;
        state.serialize_field("compatibility", &self.compatibility)?;
        state.serialize_field("restored_file_count", &self.restored_file_count)?;
        state.serialize_field("restored_bytes", &self.restored_bytes)?;
        state.serialize_field("saved_file_count", &self.saved_file_count)?;
        state.serialize_field("saved_bytes", &self.saved_bytes)?;
        state.serialize_field("skipped_count", &self.skipped_count)?;
        state.serialize_field("skipped_reasons", &self.skipped_reasons)?;
        state.serialize_field("skipped_samples", &self.skipped_samples)?;
        state.serialize_field("key_input_mismatches", &self.key_input_mismatches)?;
        state.serialize_field("miss_classifications", &miss_classifications)?;
        state.serialize_field("backend", &self.backend)?;
        state.serialize_field("cache_key", &self.cache_key)?;
        state.serialize_field("backend_cache_key", &self.backend_cache_key)?;
        state.serialize_field("backend_cache_version", &self.backend_cache_version)?;
        state.serialize_field("archive_path", &self.archive_path)?;
        state.serialize_field("journal_log_path", &self.journal_log_path)?;
        state.serialize_field(
            "target_artifact_effectiveness",
            &self.target_artifact_effectiveness,
        )?;
        state.serialize_field("compile_cache_stats", &self.compile_cache_stats)?;
        state.end()
    }
}

impl RustPlanSummary {
    /// Create a validation-only success summary.
    #[must_use]
    pub fn validation_success(plan: &RustArtifactPlanV1, cache_dir: &Path) -> Self {
        let cache_key = rust_plan_cache_key(plan);
        let archive_path = rust_plan_bundle_dir(cache_dir, &cache_key);
        Self::new(
            RustPlanOperation::Validate,
            plan.mode,
            plan.schema_version,
            plan.cache_schema_version,
            cache_key,
            Some(archive_path),
            plan.journal_log_path.clone(),
        )
    }

    /// Create a compatibility failure summary for JSON CLI output.
    #[must_use]
    pub fn compatibility_failure(operation: RustPlanOperation, err: &RustPlanError) -> Self {
        Self {
            operation,
            mode: RustPlanMode::Thin,
            plan_schema_version: 0,
            cache_schema_version: 0,
            compatibility: RustPlanCompatibility {
                status: "error".to_string(),
                errors: vec![err.to_string()],
            },
            restored_file_count: 0,
            restored_bytes: 0,
            saved_file_count: 0,
            saved_bytes: 0,
            skipped_count: 0,
            skipped_reasons: BTreeMap::new(),
            skipped_samples: Vec::new(),
            key_input_mismatches: Vec::new(),
            miss_classifications: BTreeMap::new(),
            backend: "unknown".to_string(),
            cache_key: String::new(),
            backend_cache_key: None,
            backend_cache_version: None,
            archive_path: None,
            journal_log_path: None,
            target_artifact_effectiveness: RustPlanArtifactEffectiveness {
                eligible_file_count: 0,
                restored_file_count: 0,
                reuse_ratio: 0.0,
            },
            compile_cache_stats: None,
        }
    }

    fn new(
        operation: RustPlanOperation,
        mode: RustPlanMode,
        plan_schema_version: u32,
        cache_schema_version: u32,
        cache_key: String,
        archive_path: Option<NormalizedPath>,
        journal_log_path: Option<NormalizedPath>,
    ) -> Self {
        Self {
            operation,
            mode,
            plan_schema_version,
            cache_schema_version,
            compatibility: RustPlanCompatibility {
                status: "ok".to_string(),
                errors: Vec::new(),
            },
            restored_file_count: 0,
            restored_bytes: 0,
            saved_file_count: 0,
            saved_bytes: 0,
            skipped_count: 0,
            skipped_reasons: BTreeMap::new(),
            skipped_samples: Vec::new(),
            key_input_mismatches: Vec::new(),
            miss_classifications: BTreeMap::new(),
            backend: "local".to_string(),
            cache_key,
            backend_cache_key: None,
            backend_cache_version: None,
            archive_path,
            journal_log_path,
            target_artifact_effectiveness: RustPlanArtifactEffectiveness {
                eligible_file_count: 0,
                restored_file_count: 0,
                reuse_ratio: 0.0,
            },
            compile_cache_stats: None,
        }
    }

    fn skip(&mut self, path: impl Into<String>, reason: &'static str) {
        self.skipped_count += 1;
        *self.skipped_reasons.entry(reason.to_string()).or_insert(0) += 1;
        if self.skipped_samples.len() < 16 {
            self.skipped_samples.push(RustPlanSkippedSample {
                path: path.into(),
                reason: reason.to_string(),
            });
        }
        self.refresh_miss_classifications();
    }

    /// Record a skipped artifact or backend miss in an operation summary.
    pub fn record_skip(&mut self, path: impl Into<String>, reason: &'static str) {
        self.skip(path, reason);
    }

    /// Set the backend identity fields in an operation summary.
    pub fn set_backend(
        &mut self,
        backend: impl Into<String>,
        backend_cache_key: Option<String>,
        backend_cache_version: Option<String>,
    ) {
        self.backend = backend.into();
        self.backend_cache_key = backend_cache_key;
        self.backend_cache_version = backend_cache_version;
    }

    fn refresh_effectiveness(&mut self, eligible: u64) {
        self.target_artifact_effectiveness.eligible_file_count = eligible;
        self.target_artifact_effectiveness.restored_file_count = self.restored_file_count;
        self.target_artifact_effectiveness.reuse_ratio = if eligible == 0 {
            0.0
        } else {
            self.restored_file_count as f64 / eligible as f64
        };
        self.refresh_miss_classifications();
    }

    /// Recompute low-reuse classifications from already-recorded diagnostics.
    pub fn refresh_miss_classifications(&mut self) {
        self.miss_classifications = self.computed_miss_classifications();
    }

    #[must_use]
    pub fn computed_miss_classifications(&self) -> BTreeMap<String, u64> {
        let mut classifications = BTreeMap::new();

        for (reason, count) in &self.skipped_reasons {
            if let Some(classification) = skip_reason_miss_classification(reason) {
                add_miss_classification(&mut classifications, classification, *count);
            }
        }

        for mismatch in &self.key_input_mismatches {
            for classification in key_mismatch_classifications(mismatch) {
                add_miss_classification(&mut classifications, classification, 1);
            }
        }

        if let Some(stats) = &self.compile_cache_stats {
            let misses = compile_cache_misses(stats);
            if misses > 0 {
                add_miss_classification(
                    &mut classifications,
                    "zccache_compile_cache_miss_despite_equivalent_rustc_command",
                    misses,
                );
            }
        }

        classifications
    }
}

fn add_miss_classification(
    classifications: &mut BTreeMap<String, u64>,
    classification: &'static str,
    count: u64,
) {
    *classifications
        .entry(classification.to_string())
        .or_insert(0) += count;
}

fn skip_reason_miss_classification(reason: &str) -> Option<&'static str> {
    match reason {
        "artifact_absent_from_restored_plan" => Some("artifact_absent_from_restored_plan"),
        "artifact_class_disallowed_by_plan" => Some("artifact_class_disallowed_by_plan"),
        "workspace_or_path_dependency_excluded_by_plan" => {
            Some("workspace_or_path_dependency_excluded_by_plan")
        }
        "restored_payload_missing_or_corrupt" => Some("restored_payload_missing_or_corrupt"),
        "backend_cache_miss" => Some("backend_cache_miss"),
        _ => None,
    }
}

fn key_mismatch_classifications(mismatch: &str) -> Vec<&'static str> {
    let lower = mismatch.to_ascii_lowercase();
    let mut classifications = Vec::new();

    if lower.contains("cache key")
        || lower.contains("mode")
        || lower.contains("toolchain")
        || lower.contains("profile")
        || lower.contains("rustflags")
        || lower.contains("target")
    {
        classifications.push("toolchain_profile_rustflags_target_mismatch");
    }

    if lower.contains("cache key")
        || lower.contains("input hash")
        || lower.contains("lockfile")
        || lower.contains("config")
        || lower.contains("manifest")
    {
        classifications.push("lockfile_config_manifest_hash_mismatch");
    }

    classifications
}

fn compile_cache_misses(stats: &serde_json::Value) -> u64 {
    stats
        .get("misses")
        .and_then(serde_json::Value::as_u64)
        .or_else(|| {
            stats
                .get("cache_misses")
                .and_then(serde_json::Value::as_u64)
        })
        .unwrap_or(0)
}

/// File stored in a Rust artifact bundle.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RustBundledArtifact {
    pub relative_path: String,
    pub class: RustArtifactClass,
    pub size: u64,
    pub content_hash: String,
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
}

/// Compute the stable cache key for a plan.
#[must_use]
pub fn rust_plan_cache_key(plan: &RustArtifactPlanV1) -> String {
    let identity = rust_plan_identity_hash(plan);
    format!("rust-plan-v1-{}", &identity[..32])
}

/// Compute the stable identity hash used by manifests.
#[must_use]
pub fn rust_plan_identity_hash(plan: &RustArtifactPlanV1) -> String {
    let payload = serde_json::json!({
        "schema_version": plan.schema_version,
        "mode": plan.mode,
        "workspace_root": normalize_for_key(plan.workspace_root.as_path()),
        "target_dir": normalize_for_key(plan.target_dir.as_path()),
        "toolchain": plan.toolchain,
        "target_triple": plan.target_triple,
        "profile": plan.profile,
        "inputs": plan.inputs,
        "packages": plan.packages,
        "allowed_artifact_classes": plan.effective_allowed_classes().into_iter().collect::<Vec<_>>(),
        "cache_schema_version": plan.cache_schema_version,
    });
    let bytes = serde_json::to_vec(&payload).unwrap_or_default();
    zccache::hash::hash_bytes(&bytes).to_hex()
}

/// Bundle directory for a plan cache key.
#[must_use]
pub fn rust_plan_bundle_dir(cache_dir: &Path, cache_key: &str) -> NormalizedPath {
    NormalizedPath::new(cache_dir.join("rust-plan").join(cache_key))
}

/// Execute local bundle save for a validated plan.
pub fn save_rust_plan_local(
    plan: &RustArtifactPlanV1,
    cache_dir: &Path,
) -> Result<RustPlanSummary, RustPlanError> {
    plan.validate()?;
    ensure_supported_cache_schema_version(plan.cache_schema_version)?;
    let cache_key = rust_plan_cache_key(plan);
    let bundle_dir = rust_plan_bundle_dir(cache_dir, &cache_key);
    let files_dir = bundle_dir.join(BUNDLE_FILES_DIR);
    let mut summary = RustPlanSummary::new(
        RustPlanOperation::Save,
        plan.mode,
        plan.schema_version,
        plan.cache_schema_version,
        cache_key.clone(),
        Some(bundle_dir.clone()),
        plan.journal_log_path.clone(),
    );

    let mut candidates = Vec::new();
    collect_files(plan.target_dir.as_path(), &mut candidates)?;
    candidates.sort();

    let selected = select_artifacts(plan, candidates, &mut summary);

    if bundle_dir.exists() {
        std::fs::remove_dir_all(&bundle_dir)?;
    }
    std::fs::create_dir_all(&files_dir)?;

    let artifacts = bundle_selected_artifacts(&selected, &files_dir)?;
    summary.saved_file_count += artifacts.len() as u64;
    summary.saved_bytes += artifacts.iter().map(|a| a.size).sum::<u64>();

    let manifest = RustArtifactBundleManifest {
        manifest_schema_version: RUST_ARTIFACT_CACHE_SCHEMA_VERSION,
        plan_schema_version: plan.schema_version,
        cache_schema_version: plan.cache_schema_version,
        mode: plan.mode,
        cache_key,
        created_at_secs: now_secs(),
        plan_identity_hash: rust_plan_identity_hash(plan),
        artifacts,
    };
    let manifest_bytes = serde_json::to_vec_pretty(&manifest)?;
    std::fs::write(bundle_dir.join(BUNDLE_MANIFEST_NAME), manifest_bytes)?;
    Ok(summary)
}

/// Copy + stat + hash every selected artifact into `files_dir`, returning the
/// manifest entries in input order (which `select_artifacts` has already sorted
/// by `relative_path` for determinism). Reads `resolve_rust_plan_tar_threads()`
/// for the parallelism setting — see issue #177 for the Windows-CI motivation.
fn bundle_selected_artifacts(
    selected: &[SelectedArtifact],
    files_dir: &Path,
) -> Result<Vec<RustBundledArtifact>, RustPlanError> {
    bundle_selected_artifacts_with_threads(selected, files_dir, resolve_rust_plan_tar_threads())
}

/// Same as `bundle_selected_artifacts`, but with `threads` injected so tests
/// can exercise the parallel path without racing on process-global env vars.
fn bundle_selected_artifacts_with_threads(
    selected: &[SelectedArtifact],
    files_dir: &Path,
    threads: usize,
) -> Result<Vec<RustBundledArtifact>, RustPlanError> {
    if threads <= 1 || selected.len() < 2 {
        return selected
            .iter()
            .map(|sel| bundle_one_artifact(sel, files_dir))
            .collect();
    }

    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(threads)
        .thread_name(|idx| format!("zccache-rust-plan-{idx}"))
        .build()
        .map_err(|err| {
            RustPlanError::Io(std::io::Error::other(format!(
                "failed to build rust-plan thread pool: {err}"
            )))
        })?;

    pool.install(|| {
        selected
            .par_iter()
            .map(|sel| bundle_one_artifact(sel, files_dir))
            .collect()
    })
}

fn bundle_one_artifact(
    sel: &SelectedArtifact,
    files_dir: &Path,
) -> Result<RustBundledArtifact, RustPlanError> {
    let dst = safe_join(files_dir, &sel.relative_path)?;
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::copy(&sel.source_path, &dst)?;
    let size = std::fs::metadata(&sel.source_path)?.len();
    let content_hash = zccache::hash::hash_file(&sel.source_path)?.to_hex();
    Ok(RustBundledArtifact {
        relative_path: sel.relative_path.clone(),
        class: sel.class,
        size,
        content_hash,
    })
}

/// Decide how many worker threads to use for the rust-plan save copy+hash loop.
///
/// Grammar (mirrors `SOLDR_TARGET_CACHE_TAR_THREADS` validated upstream by
/// soldr#273):
/// - unset / `auto` / empty / unparseable → vCPU-bounded, capped at 8
/// - `1` → sequential (regression escape hatch)
/// - positive integer N → `min(N, MAX_RUST_PLAN_TAR_THREADS)`
///
/// `ZCCACHE_RUST_PLAN_TAR_THREADS` takes precedence over the soldr-side var so
/// direct zccache invocations can override without touching the soldr env.
pub fn resolve_rust_plan_tar_threads() -> usize {
    let raw = std::env::var("ZCCACHE_RUST_PLAN_TAR_THREADS")
        .ok()
        .or_else(|| std::env::var("SOLDR_TARGET_CACHE_TAR_THREADS").ok());
    parse_rust_plan_tar_threads(raw.as_deref())
}

fn parse_rust_plan_tar_threads(raw: Option<&str>) -> usize {
    let trimmed = raw.map(str::trim).filter(|s| !s.is_empty());
    match trimmed {
        None => default_rust_plan_tar_threads(),
        Some(s) if s.eq_ignore_ascii_case("auto") => default_rust_plan_tar_threads(),
        Some(s) => match s.parse::<usize>() {
            Ok(0) => default_rust_plan_tar_threads(),
            Ok(n) => n.min(MAX_RUST_PLAN_TAR_THREADS),
            Err(_) => default_rust_plan_tar_threads(),
        },
    }
}

fn default_rust_plan_tar_threads() -> usize {
    std::thread::available_parallelism()
        .map(std::num::NonZeroUsize::get)
        .unwrap_or(1)
        .min(DEFAULT_RUST_PLAN_TAR_THREADS_CAP)
}

/// Execute local bundle restore for a validated plan.
pub fn restore_rust_plan_local(
    plan: &RustArtifactPlanV1,
    cache_dir: &Path,
) -> Result<RustPlanSummary, RustPlanError> {
    plan.validate()?;
    ensure_supported_cache_schema_version(plan.cache_schema_version)?;
    let cache_key = rust_plan_cache_key(plan);
    let bundle_dir = rust_plan_bundle_dir(cache_dir, &cache_key);
    let files_dir = bundle_dir.join(BUNDLE_FILES_DIR);
    let mut summary = RustPlanSummary::new(
        RustPlanOperation::Restore,
        plan.mode,
        plan.schema_version,
        plan.cache_schema_version,
        cache_key.clone(),
        Some(bundle_dir.clone()),
        plan.journal_log_path.clone(),
    );

    if !bundle_dir.exists() {
        summary.skip("<bundle>", "artifact_absent_from_restored_plan");
        summary.refresh_effectiveness(0);
        return Ok(summary);
    }

    let manifest_path = bundle_dir.join(BUNDLE_MANIFEST_NAME);
    let manifest: RustArtifactBundleManifest =
        serde_json::from_slice(&std::fs::read(&manifest_path)?)?;
    if !validate_manifest(plan, &cache_key, &manifest, &mut summary)? {
        summary.refresh_effectiveness(0);
        return Ok(summary);
    }

    let now = SystemTime::now();
    let file_times = std::fs::FileTimes::new()
        .set_accessed(now)
        .set_modified(now);
    let eligible = manifest.artifacts.len() as u64;

    for artifact in &manifest.artifacts {
        let src = match safe_join(&files_dir, &artifact.relative_path) {
            Ok(path) => path,
            Err(err) => {
                summary.skip(&artifact.relative_path, "path_traversal");
                summary.compatibility.errors.push(err.to_string());
                continue;
            }
        };
        let dst = match safe_join(plan.target_dir.as_path(), &artifact.relative_path) {
            Ok(path) => path,
            Err(err) => {
                summary.skip(&artifact.relative_path, "path_traversal");
                summary.compatibility.errors.push(err.to_string());
                continue;
            }
        };
        let Ok(metadata) = std::fs::metadata(&src) else {
            summary.skip(
                &artifact.relative_path,
                "restored_payload_missing_or_corrupt",
            );
            continue;
        };
        if metadata.len() != artifact.size {
            summary.skip(
                &artifact.relative_path,
                "restored_payload_missing_or_corrupt",
            );
            continue;
        }
        let Ok(content_hash) = zccache::hash::hash_file(&src).map(|hash| hash.to_hex()) else {
            summary.skip(
                &artifact.relative_path,
                "restored_payload_missing_or_corrupt",
            );
            continue;
        };
        if content_hash != artifact.content_hash {
            summary.skip(
                &artifact.relative_path,
                "restored_payload_missing_or_corrupt",
            );
            continue;
        }
        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent)?;
        }
        if dst.exists() {
            std::fs::remove_file(&dst)?;
        }
        if std::fs::hard_link(&src, &dst).is_err() {
            std::fs::copy(&src, &dst)?;
        }
        if let Ok(file) = std::fs::File::open(&dst) {
            let _ = file.set_times(file_times);
        }
        summary.restored_file_count += 1;
        summary.restored_bytes += artifact.size;
    }

    summary.refresh_effectiveness(eligible);
    Ok(summary)
}

fn validate_manifest(
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
    if compatible {
        Ok(true)
    } else {
        summary.compatibility.status = "warning".to_string();
        summary.compatibility.errors = summary.key_input_mismatches.clone();
        Ok(false)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SelectedArtifact {
    source_path: NormalizedPath,
    relative_path: String,
    class: RustArtifactClass,
}

fn select_artifacts(
    plan: &RustArtifactPlanV1,
    candidates: Vec<NormalizedPath>,
    summary: &mut RustPlanSummary,
) -> Vec<SelectedArtifact> {
    let allowed = plan.effective_allowed_classes();
    let excluded_names = excluded_package_names(&plan.packages);
    let mut selected = Vec::new();

    for path in candidates {
        let rel_path = match path.strip_prefix(plan.target_dir.as_path()) {
            Ok(rel) => rel,
            Err(_) => {
                summary.skip(path.display().to_string(), "outside_target_dir");
                continue;
            }
        };
        let rel = relative_path_string(rel_path);

        if has_component(rel_path, "incremental") {
            summary.skip(rel, "transient_state");
            continue;
        }

        let class = classify_artifact(rel_path, plan.mode);

        if plan.mode == RustPlanMode::Thin {
            let Some(class) = class else {
                summary.skip(rel, "artifact_class_disallowed_by_plan");
                continue;
            };
            if !allowed.contains(&class) {
                summary.skip(rel, "artifact_class_disallowed_by_plan");
                continue;
            }
            if artifact_matches_excluded_package(rel_path, &excluded_names) {
                summary.skip(rel, "workspace_or_path_dependency_excluded_by_plan");
                continue;
            }
            selected.push(SelectedArtifact {
                source_path: path,
                relative_path: rel,
                class,
            });
            continue;
        }

        selected.push(SelectedArtifact {
            source_path: path,
            relative_path: rel,
            class: class.unwrap_or(RustArtifactClass::FullTarget),
        });
    }

    selected.sort_by(|a, b| a.relative_path.cmp(&b.relative_path));
    selected
}

fn classify_artifact(rel: &Path, mode: RustPlanMode) -> Option<RustArtifactClass> {
    if has_component(rel, ".fingerprint") {
        return Some(RustArtifactClass::CargoFingerprint);
    }
    if has_component(rel, "build") {
        if has_component(rel, "out") {
            return Some(RustArtifactClass::BuildScriptOutput);
        }
        if let Some(name) = rel.file_name().and_then(OsStr::to_str) {
            if matches!(name, "output" | "invoked.timestamp" | "root-output") {
                return Some(RustArtifactClass::BuildScriptMetadata);
            }
        }
    }

    match rel.extension().and_then(OsStr::to_str) {
        Some("rlib") => Some(RustArtifactClass::Rlib),
        Some("rmeta") => Some(RustArtifactClass::Rmeta),
        Some("d") => Some(RustArtifactClass::DepInfo),
        Some("so" | "dylib" | "dll") if is_likely_proc_macro_dylib(rel) => {
            Some(RustArtifactClass::ProcMacro)
        }
        Some("so" | "dylib" | "dll") => Some(RustArtifactClass::SharedLib),
        _ if mode == RustPlanMode::Full => Some(RustArtifactClass::FullTarget),
        _ => None,
    }
}

fn is_likely_proc_macro_dylib(rel: &Path) -> bool {
    if !has_component(rel, "deps") {
        return false;
    }

    rel.file_stem()
        .and_then(OsStr::to_str)
        .map(|stem| {
            let stem = stem.to_ascii_lowercase();
            stem.contains("proc_macro") || stem.contains("proc-macro")
        })
        .unwrap_or(false)
}

fn collect_files(root: &Path, files: &mut Vec<NormalizedPath>) -> Result<(), RustPlanError> {
    if !root.exists() {
        return Ok(());
    }

    let mut entries = Vec::new();
    for entry in std::fs::read_dir(root)? {
        entries.push(entry?);
    }
    entries.sort_by_key(|entry| entry.file_name());

    for entry in entries {
        let path = NormalizedPath::new(entry.path());
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            collect_files(path.as_path(), files)?;
        } else if file_type.is_file() {
            files.push(path);
        }
    }
    Ok(())
}

fn safe_join(root: &Path, relative: &str) -> Result<NormalizedPath, RustPlanError> {
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

fn default_thin_classes() -> BTreeSet<RustArtifactClass> {
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

fn json_u32_field(value: &serde_json::Value, field: &'static str) -> Result<u32, RustPlanError> {
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

fn ensure_supported_cache_schema_version(cache_schema_version: u32) -> Result<(), RustPlanError> {
    if cache_schema_version != RUST_ARTIFACT_CACHE_SCHEMA_VERSION {
        return Err(RustPlanError::UnsupportedCacheSchemaVersion {
            found: cache_schema_version,
            supported: RUST_ARTIFACT_CACHE_SCHEMA_VERSION,
        });
    }
    Ok(())
}
fn relative_path_string(path: &Path) -> String {
    path.components()
        .filter_map(|component| match component {
            Component::Normal(part) => Some(part.to_string_lossy().into_owned()),
            Component::CurDir => None,
            _ => Some(component.as_os_str().to_string_lossy().into_owned()),
        })
        .collect::<Vec<_>>()
        .join("/")
}

fn has_component(path: &Path, needle: &str) -> bool {
    path.components()
        .any(|component| component.as_os_str() == OsStr::new(needle))
}

fn excluded_package_names(packages: &RustPlanPackages) -> BTreeSet<String> {
    packages
        .workspace_package_ids
        .iter()
        .chain(packages.excluded_path_package_ids.iter())
        .filter_map(|id| package_name_from_id(id))
        .collect()
}

fn package_name_from_id(id: &str) -> Option<String> {
    let candidate = if let Some(after_hash) = id.rsplit_once('#').map(|(_, right)| right) {
        after_hash.split('@').next().unwrap_or(after_hash)
    } else if let Some((left, _)) = id.split_once(' ') {
        left
    } else {
        id
    };
    let candidate = candidate
        .trim()
        .trim_matches('"')
        .trim_matches('\'')
        .replace('-', "_");
    if candidate.is_empty()
        || candidate.contains('/')
        || candidate.contains('\\')
        || candidate.contains(':')
    {
        None
    } else {
        Some(candidate)
    }
}

fn artifact_matches_excluded_package(rel: &Path, excluded_names: &BTreeSet<String>) -> bool {
    if excluded_names.is_empty() {
        return false;
    }
    rel.components().any(|component| {
        let name = component.as_os_str().to_string_lossy();
        excluded_names.iter().any(|package| {
            let without_lib = name.strip_prefix("lib").unwrap_or(&name);
            without_lib == package
                || without_lib.starts_with(&format!("{package}-"))
                || without_lib.starts_with(&format!("{package}."))
        })
    })
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_plan(root: &Path, mode: RustPlanMode) -> RustArtifactPlanV1 {
        RustArtifactPlanV1 {
            schema_version: 1,
            mode,
            workspace_root: root.into(),
            target_dir: root.join("target").into(),
            toolchain: RustToolchainIdentity {
                rustc: "rustc 1.94.1".to_string(),
                cargo: "cargo 1.94.1".to_string(),
                channel: "1.94.1".to_string(),
                host: "x86_64-pc-windows-msvc".to_string(),
            },
            target_triple: "x86_64-pc-windows-msvc".to_string(),
            profile: "debug".to_string(),
            inputs: RustPlanInputs {
                features_hash: "features".to_string(),
                rustflags_hash: "rustflags".to_string(),
                env_hash: "env".to_string(),
                lockfile_hash: "lock".to_string(),
                cargo_config_hash: "config".to_string(),
                manifest_hashes: vec!["manifest".to_string()],
            },
            packages: RustPlanPackages {
                selected_package_ids: vec!["app 0.1.0".to_string()],
                workspace_package_ids: vec!["app 0.1.0".to_string()],
                excluded_path_package_ids: vec!["local_dep 0.1.0".to_string()],
            },
            allowed_artifact_classes: vec![
                RustArtifactClass::Rlib,
                RustArtifactClass::Rmeta,
                RustArtifactClass::DepInfo,
                RustArtifactClass::CargoFingerprint,
                RustArtifactClass::BuildScriptMetadata,
                RustArtifactClass::BuildScriptOutput,
            ],
            cache_schema_version: 1,
            journal_log_path: Some(root.join("zccache-session.jsonl").into()),
        }
    }

    fn write(path: &Path, bytes: &[u8]) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, bytes).unwrap();
    }

    fn load_manifest(bundle_dir: &Path) -> RustArtifactBundleManifest {
        let manifest_path = bundle_dir.join(BUNDLE_MANIFEST_NAME);
        serde_json::from_slice(&std::fs::read(manifest_path).unwrap()).unwrap()
    }

    fn write_manifest(bundle_dir: &Path, manifest: &RustArtifactBundleManifest) {
        let manifest_path = bundle_dir.join(BUNDLE_MANIFEST_NAME);
        let bytes = serde_json::to_vec_pretty(manifest).unwrap();
        std::fs::write(manifest_path, bytes).unwrap();
    }

    fn synthetic_target(root: &Path) {
        let target = root.join("target").join("debug");
        write(
            &target.join("deps").join("libserde-abc.rlib"),
            b"serde rlib",
        );
        write(
            &target.join("deps").join("libserde-abc.rmeta"),
            b"serde rmeta",
        );
        write(&target.join("deps").join("serde-abc.d"), b"serde depinfo");
        write(
            &target.join("deps").join("libapp-abc.rlib"),
            b"workspace rlib",
        );
        write(
            &target.join("deps").join("liblocal_dep-abc.rlib"),
            b"path dep rlib",
        );
        write(
            &target
                .join(".fingerprint")
                .join("serde-abc")
                .join("dep-lib-serde"),
            b"fingerprint",
        );
        write(
            &target
                .join("build")
                .join("serde-abc")
                .join("invoked.timestamp"),
            b"timestamp",
        );
        write(
            &target
                .join("build")
                .join("serde-abc")
                .join("out")
                .join("gen.rs"),
            b"generated",
        );
        write(&target.join("incremental").join("state.bin"), b"transient");
    }

    fn synthetic_target_with_final_binary(root: &Path) {
        synthetic_target(root);
        let target = root.join("target").join("debug");
        #[cfg(windows)]
        write(&target.join("app.exe"), b"final binary");
        #[cfg(not(windows))]
        write(&target.join("app"), b"final binary");
    }

    fn synthetic_target_with_proc_macro_outputs(root: &Path) {
        synthetic_target(root);
        let target = root.join("target").join("debug");
        #[cfg(windows)]
        let proc_macro = target.join("deps").join("libproc_macro2-def456.dll");
        #[cfg(not(windows))]
        let proc_macro = target.join("deps").join("libproc_macro2-def456.so");
        write(&proc_macro, b"proc-macro dylib");

        #[cfg(windows)]
        let shared_lib = target.join("deps").join("libserde_shared-def456.dll");
        #[cfg(not(windows))]
        let shared_lib = target.join("deps").join("libserde_shared-def456.so");
        write(&shared_lib, b"shared lib");
    }

    fn synthetic_target_with_package_exclusions(root: &Path) {
        synthetic_target(root);
        let target = root.join("target").join("debug");
        write(
            &target.join("deps").join("libapp-abc.rmeta"),
            b"workspace rmeta",
        );
        write(&target.join("deps").join("app-abc.d"), b"workspace depinfo");
        write(
            &target.join("deps").join("liblocal_dep-abc.rmeta"),
            b"path dep rmeta",
        );
        write(
            &target.join("deps").join("local_dep-abc.d"),
            b"path dep depinfo",
        );
        write(
            &target
                .join(".fingerprint")
                .join("app-abc")
                .join("dep-lib-app"),
            b"workspace fingerprint",
        );
        write(
            &target
                .join(".fingerprint")
                .join("local_dep-abc")
                .join("dep-lib-local_dep"),
            b"path dep fingerprint",
        );
        write(
            &target
                .join("build")
                .join("app-abc")
                .join("invoked.timestamp"),
            b"workspace timestamp",
        );
        write(
            &target
                .join("build")
                .join("local_dep-abc")
                .join("invoked.timestamp"),
            b"path dep timestamp",
        );
        write(
            &target
                .join("build")
                .join("app-abc")
                .join("out")
                .join("gen.rs"),
            b"workspace generated",
        );
        write(
            &target
                .join("build")
                .join("local_dep-abc")
                .join("out")
                .join("gen.rs"),
            b"path dep generated",
        );
    }

    #[test]
    fn rejects_unsupported_schema_before_deserializing_unknown_fields() {
        let raw = serde_json::json!({
            "schema_version": 99,
            "cache_schema_version": 1,
            "unexpected_future_field": true
        });
        let err = RustArtifactPlanV1::from_json_value(raw).unwrap_err();
        assert!(matches!(
            err,
            RustPlanError::UnsupportedSchemaVersion {
                found: 99,
                supported: 1
            }
        ));
    }

    #[test]
    fn rejects_unsupported_cache_schema_before_filesystem_mutation() {
        let dir = tempfile::tempdir().unwrap();
        synthetic_target(dir.path());
        let plan = RustArtifactPlanV1 {
            cache_schema_version: 99,
            ..sample_plan(dir.path(), RustPlanMode::Thin)
        };
        let cache = dir.path().join("cache");

        let err = save_rust_plan_local(&plan, &cache).unwrap_err();
        assert!(matches!(
            err,
            RustPlanError::UnsupportedCacheSchemaVersion {
                found: 99,
                supported: 1
            }
        ));
        assert!(!cache.exists());
    }

    #[test]
    fn restore_rejects_unsupported_cache_schema_before_filesystem_mutation() {
        let dir = tempfile::tempdir().unwrap();
        synthetic_target(dir.path());
        let plan = RustArtifactPlanV1 {
            cache_schema_version: 99,
            ..sample_plan(dir.path(), RustPlanMode::Thin)
        };
        let cache = dir.path().join("cache");
        std::fs::create_dir_all(&cache).unwrap();
        std::fs::create_dir_all(plan.target_dir.as_path()).unwrap();
        let sentinel = plan.target_dir.join("sentinel.txt");
        std::fs::write(&sentinel, b"keep me").unwrap();

        let err = restore_rust_plan_local(&plan, &cache).unwrap_err();
        assert!(matches!(
            err,
            RustPlanError::UnsupportedCacheSchemaVersion {
                found: 99,
                supported: 1
            }
        ));
        assert!(sentinel.exists());
    }

    #[test]
    fn omitted_or_empty_allowed_classes_default_to_thin_classes() {
        let dir = tempfile::tempdir().unwrap();
        let mut plan_value =
            serde_json::to_value(sample_plan(dir.path(), RustPlanMode::Thin)).unwrap();

        plan_value
            .as_object_mut()
            .unwrap()
            .remove("allowed_artifact_classes");
        let omitted = RustArtifactPlanV1::from_json_value(plan_value.clone()).unwrap();
        assert_eq!(omitted.effective_allowed_classes(), default_thin_classes());

        plan_value.as_object_mut().unwrap().insert(
            "allowed_artifact_classes".to_string(),
            serde_json::json!([]),
        );
        let empty = RustArtifactPlanV1::from_json_value(plan_value).unwrap();
        assert_eq!(empty.effective_allowed_classes(), default_thin_classes());
    }
    #[test]
    fn thin_save_restore_selects_dependency_artifacts_and_metadata() {
        let dir = tempfile::tempdir().unwrap();
        synthetic_target(dir.path());
        let plan = sample_plan(dir.path(), RustPlanMode::Thin);
        let cache = dir.path().join("cache");

        let saved = save_rust_plan_local(&plan, &cache).unwrap();
        assert_eq!(saved.saved_file_count, 6);
        assert_eq!(
            saved
                .skipped_reasons
                .get("workspace_or_path_dependency_excluded_by_plan"),
            Some(&2)
        );
        assert_eq!(saved.skipped_reasons.get("transient_state"), Some(&1));

        std::fs::remove_dir_all(plan.target_dir.as_path()).unwrap();
        let restored = restore_rust_plan_local(&plan, &cache).unwrap();
        assert_eq!(restored.restored_file_count, 6);
        assert!(plan
            .target_dir
            .join("debug/deps/libserde-abc.rlib")
            .exists());
        assert!(plan
            .target_dir
            .join("debug/.fingerprint/serde-abc/dep-lib-serde")
            .exists());
        assert!(!plan.target_dir.join("debug/deps/libapp-abc.rlib").exists());
        assert!(!plan.target_dir.join("debug/incremental/state.bin").exists());
    }

    #[test]
    fn thin_plan_skips_final_binary_outputs() {
        let dir = tempfile::tempdir().unwrap();
        synthetic_target_with_final_binary(dir.path());
        let plan = sample_plan(dir.path(), RustPlanMode::Thin);
        let cache = dir.path().join("cache");

        let saved = save_rust_plan_local(&plan, &cache).unwrap();
        assert_eq!(saved.saved_file_count, 6);
        assert_eq!(
            saved
                .skipped_reasons
                .get("artifact_class_disallowed_by_plan"),
            Some(&1)
        );
        assert!(saved.skipped_samples.iter().any(|sample| {
            sample.path.ends_with("debug/app.exe") || sample.path.ends_with("debug/app")
        }));

        std::fs::remove_dir_all(plan.target_dir.as_path()).unwrap();
        let restored = restore_rust_plan_local(&plan, &cache).unwrap();
        assert_eq!(restored.restored_file_count, 6);
        assert!(!plan.target_dir.join("debug/app.exe").exists());
        assert!(!plan.target_dir.join("debug/app").exists());
    }

    #[test]
    fn thin_plan_respects_explicit_class_gates_for_dependency_metadata_and_outputs() {
        let dir = tempfile::tempdir().unwrap();
        synthetic_target(dir.path());
        let mut plan = sample_plan(dir.path(), RustPlanMode::Thin);
        plan.allowed_artifact_classes = vec![RustArtifactClass::Rlib, RustArtifactClass::Rmeta];
        let cache = dir.path().join("cache");

        let saved = save_rust_plan_local(&plan, &cache).unwrap();
        assert_eq!(saved.saved_file_count, 2);
        assert_eq!(
            saved
                .skipped_reasons
                .get("artifact_class_disallowed_by_plan"),
            Some(&4)
        );
        assert_eq!(
            saved
                .skipped_reasons
                .get("workspace_or_path_dependency_excluded_by_plan"),
            Some(&2)
        );
        assert!(saved
            .skipped_samples
            .iter()
            .any(|sample| sample.path.ends_with("debug/deps/serde-abc.d")));
        assert!(saved.skipped_samples.iter().any(|sample| sample
            .path
            .ends_with("debug/.fingerprint/serde-abc/dep-lib-serde")));
        assert!(saved.skipped_samples.iter().any(|sample| sample
            .path
            .ends_with("debug/build/serde-abc/invoked.timestamp")));
        assert!(saved
            .skipped_samples
            .iter()
            .any(|sample| sample.path.ends_with("debug/build/serde-abc/out/gen.rs")));
    }

    #[test]
    fn thin_plan_saves_and_restores_likely_proc_macro_dylibs_without_shared_libs() {
        let dir = tempfile::tempdir().unwrap();
        synthetic_target_with_proc_macro_outputs(dir.path());
        let mut plan = sample_plan(dir.path(), RustPlanMode::Thin);
        plan.allowed_artifact_classes = vec![
            RustArtifactClass::Rlib,
            RustArtifactClass::Rmeta,
            RustArtifactClass::DepInfo,
            RustArtifactClass::ProcMacro,
            RustArtifactClass::CargoFingerprint,
            RustArtifactClass::BuildScriptMetadata,
            RustArtifactClass::BuildScriptOutput,
        ];
        let cache = dir.path().join("cache");

        let saved = save_rust_plan_local(&plan, &cache).unwrap();
        assert_eq!(saved.saved_file_count, 7);
        assert_eq!(
            saved
                .skipped_reasons
                .get("artifact_class_disallowed_by_plan"),
            Some(&1)
        );
        assert!(saved.skipped_samples.iter().any(|sample| {
            sample
                .path
                .ends_with("debug/deps/libserde_shared-def456.dll")
                || sample
                    .path
                    .ends_with("debug/deps/libserde_shared-def456.so")
        }));

        std::fs::remove_dir_all(plan.target_dir.as_path()).unwrap();
        let restored = restore_rust_plan_local(&plan, &cache).unwrap();
        assert_eq!(restored.restored_file_count, 7);
        assert!(plan
            .target_dir
            .join(if cfg!(windows) {
                "debug/deps/libproc_macro2-def456.dll"
            } else {
                "debug/deps/libproc_macro2-def456.so"
            })
            .exists());
        assert!(!plan
            .target_dir
            .join(if cfg!(windows) {
                "debug/deps/libserde_shared-def456.dll"
            } else {
                "debug/deps/libserde_shared-def456.so"
            })
            .exists());
    }

    #[test]
    fn thin_plan_skips_likely_proc_macro_dylibs_when_disallowed() {
        let dir = tempfile::tempdir().unwrap();
        synthetic_target_with_proc_macro_outputs(dir.path());
        let mut plan = sample_plan(dir.path(), RustPlanMode::Thin);
        plan.allowed_artifact_classes = vec![
            RustArtifactClass::Rlib,
            RustArtifactClass::Rmeta,
            RustArtifactClass::DepInfo,
            RustArtifactClass::SharedLib,
            RustArtifactClass::CargoFingerprint,
            RustArtifactClass::BuildScriptMetadata,
            RustArtifactClass::BuildScriptOutput,
        ];
        let cache = dir.path().join("cache");

        let saved = save_rust_plan_local(&plan, &cache).unwrap();
        assert_eq!(saved.saved_file_count, 7);
        assert_eq!(
            saved
                .skipped_reasons
                .get("artifact_class_disallowed_by_plan"),
            Some(&1)
        );
        assert!(saved.skipped_samples.iter().any(|sample| {
            sample
                .path
                .ends_with("debug/deps/libproc_macro2-def456.dll")
                || sample.path.ends_with("debug/deps/libproc_macro2-def456.so")
        }));
    }

    #[test]
    fn restore_skips_mismatched_bundles_without_mutating_target_dir() {
        let dir = tempfile::tempdir().unwrap();
        synthetic_target(dir.path());
        let plan = sample_plan(dir.path(), RustPlanMode::Thin);
        let cache = dir.path().join("cache");

        save_rust_plan_local(&plan, &cache).unwrap();
        let bundle_dir = rust_plan_bundle_dir(&cache, &rust_plan_cache_key(&plan));
        let mut manifest = load_manifest(&bundle_dir);
        manifest.cache_key = "rust-plan-v1-deadbeefdeadbeefdeadbeefdeadbeef".to_string();
        manifest.mode = RustPlanMode::Full;
        manifest.plan_identity_hash =
            "deadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef".to_string();
        write_manifest(&bundle_dir, &manifest);

        std::fs::remove_dir_all(plan.target_dir.as_path()).unwrap();
        std::fs::create_dir_all(plan.target_dir.as_path()).unwrap();

        let restored = restore_rust_plan_local(&plan, &cache).unwrap();
        assert_eq!(restored.restored_file_count, 0);
        assert_eq!(restored.compatibility.status, "warning");
        assert_eq!(restored.key_input_mismatches.len(), 3);
        assert_eq!(
            restored
                .miss_classifications
                .get("toolchain_profile_rustflags_target_mismatch"),
            Some(&2)
        );
        assert_eq!(
            restored
                .miss_classifications
                .get("lockfile_config_manifest_hash_mismatch"),
            Some(&2)
        );
        assert!(std::fs::read_dir(plan.target_dir.as_path())
            .unwrap()
            .next()
            .is_none());
    }

    #[test]
    fn full_save_restore_includes_workspace_outputs_but_prunes_incremental() {
        let dir = tempfile::tempdir().unwrap();
        synthetic_target(dir.path());
        let plan = sample_plan(dir.path(), RustPlanMode::Full);
        let cache = dir.path().join("cache");

        let saved = save_rust_plan_local(&plan, &cache).unwrap();
        assert_eq!(saved.skipped_reasons.get("transient_state"), Some(&1));
        assert!(saved.saved_file_count > 6);

        std::fs::remove_dir_all(plan.target_dir.as_path()).unwrap();
        let restored = restore_rust_plan_local(&plan, &cache).unwrap();
        assert!(restored.restored_file_count > 6);
        assert!(plan.target_dir.join("debug/deps/libapp-abc.rlib").exists());
        assert!(!plan.target_dir.join("debug/incremental/state.bin").exists());
    }

    #[test]
    fn restore_missing_bundle_is_a_diagnostic_cache_miss() {
        let dir = tempfile::tempdir().unwrap();
        let plan = sample_plan(dir.path(), RustPlanMode::Thin);
        let summary = restore_rust_plan_local(&plan, &dir.path().join("cache")).unwrap();
        assert_eq!(summary.backend, "local");
        assert_eq!(summary.restored_file_count, 0);
        assert_eq!(
            summary
                .skipped_reasons
                .get("artifact_absent_from_restored_plan"),
            Some(&1)
        );
        assert_eq!(
            summary
                .miss_classifications
                .get("artifact_absent_from_restored_plan"),
            Some(&1)
        );
    }

    #[test]
    fn summary_records_backend_identity_and_manual_skips() {
        let dir = tempfile::tempdir().unwrap();
        let plan = sample_plan(dir.path(), RustPlanMode::Thin);
        let mut summary = RustPlanSummary::validation_success(&plan, &dir.path().join("cache"));
        assert_eq!(summary.backend, "local");
        assert!(summary.backend_cache_key.is_none());

        summary.set_backend(
            "gha",
            Some("rust-plan-v1-key".to_string()),
            Some("version".to_string()),
        );
        summary.record_skip("<gha-cache>", "backend_cache_miss");

        assert_eq!(summary.backend, "gha");
        assert_eq!(
            summary.backend_cache_key.as_deref(),
            Some("rust-plan-v1-key")
        );
        assert_eq!(summary.backend_cache_version.as_deref(), Some("version"));
        assert_eq!(summary.skipped_reasons.get("backend_cache_miss"), Some(&1));
        assert_eq!(
            summary.miss_classifications.get("backend_cache_miss"),
            Some(&1)
        );
    }

    #[test]
    fn summary_derives_miss_classifications_from_existing_diagnostics() {
        let dir = tempfile::tempdir().unwrap();
        let plan = sample_plan(dir.path(), RustPlanMode::Thin);
        let mut summary = RustPlanSummary::validation_success(&plan, &dir.path().join("cache"));

        summary.record_skip("debug/deps/app.exe", "artifact_class_disallowed_by_plan");
        summary.record_skip(
            "debug/deps/libapp-abc.rlib",
            "workspace_or_path_dependency_excluded_by_plan",
        );
        summary
            .key_input_mismatches
            .push("bundle mode does not match requested plan".to_string());
        summary
            .key_input_mismatches
            .push("bundle input hash does not match requested plan".to_string());
        summary.compile_cache_stats = Some(serde_json::json!({
            "compilations": 4,
            "hits": 1,
            "misses": 3,
        }));
        summary.refresh_miss_classifications();

        assert_eq!(
            summary
                .miss_classifications
                .get("artifact_class_disallowed_by_plan"),
            Some(&1)
        );
        assert_eq!(
            summary
                .miss_classifications
                .get("workspace_or_path_dependency_excluded_by_plan"),
            Some(&1)
        );
        assert_eq!(
            summary
                .miss_classifications
                .get("toolchain_profile_rustflags_target_mismatch"),
            Some(&1)
        );
        assert_eq!(
            summary
                .miss_classifications
                .get("lockfile_config_manifest_hash_mismatch"),
            Some(&1)
        );
        assert_eq!(
            summary
                .miss_classifications
                .get("zccache_compile_cache_miss_despite_equivalent_rustc_command"),
            Some(&3)
        );
    }

    #[test]
    fn serialized_summary_recomputes_miss_classifications_from_session_stats() {
        let dir = tempfile::tempdir().unwrap();
        let plan = sample_plan(dir.path(), RustPlanMode::Thin);
        let mut summary = RustPlanSummary::validation_success(&plan, &dir.path().join("cache"));
        summary.record_skip("<gha-cache>", "backend_cache_miss");

        summary.compile_cache_stats = Some(serde_json::json!({
            "status": "ok",
            "cache_misses": 2,
        }));

        let json = serde_json::to_value(&summary).unwrap();
        assert_eq!(
            json["miss_classifications"]["backend_cache_miss"].as_u64(),
            Some(1)
        );
        assert_eq!(
            json["miss_classifications"]
                ["zccache_compile_cache_miss_despite_equivalent_rustc_command"]
                .as_u64(),
            Some(2)
        );
    }

    #[test]
    fn restore_skips_missing_wrong_size_and_wrong_hash_payloads() {
        let dir = tempfile::tempdir().unwrap();
        synthetic_target(dir.path());
        let plan = sample_plan(dir.path(), RustPlanMode::Thin);
        let cache = dir.path().join("cache");

        let saved = save_rust_plan_local(&plan, &cache).unwrap();
        assert_eq!(saved.saved_file_count, 6);

        let bundle_dir = rust_plan_bundle_dir(&cache, &rust_plan_cache_key(&plan));
        let mut manifest = load_manifest(&bundle_dir);

        std::fs::remove_file(
            bundle_dir
                .join(BUNDLE_FILES_DIR)
                .join("debug/deps/libserde-abc.rlib"),
        )
        .unwrap();
        manifest.artifacts[1].size += 1;
        manifest.artifacts[2].content_hash = "not-the-right-hash".to_string();
        write_manifest(&bundle_dir, &manifest);

        std::fs::remove_dir_all(plan.target_dir.as_path()).unwrap();
        let restored = restore_rust_plan_local(&plan, &cache).unwrap();

        assert_eq!(restored.restored_file_count, 3);
        assert_eq!(
            restored
                .skipped_reasons
                .get("restored_payload_missing_or_corrupt"),
            Some(&3)
        );
        assert_eq!(
            restored
                .miss_classifications
                .get("restored_payload_missing_or_corrupt"),
            Some(&3)
        );
        assert!(!plan
            .target_dir
            .join("debug/deps/libserde-abc.rlib")
            .exists());
    }

    #[test]
    fn restore_skips_manifest_path_traversal_entries() {
        let dir = tempfile::tempdir().unwrap();
        synthetic_target(dir.path());
        let plan = sample_plan(dir.path(), RustPlanMode::Thin);
        let cache = dir.path().join("cache");

        save_rust_plan_local(&plan, &cache).unwrap();

        let bundle_dir = rust_plan_bundle_dir(&cache, &rust_plan_cache_key(&plan));
        let mut manifest = load_manifest(&bundle_dir);
        manifest.artifacts[0].relative_path = "../escape.txt".to_string();
        write_manifest(&bundle_dir, &manifest);

        std::fs::remove_dir_all(plan.target_dir.as_path()).unwrap();
        let restored = restore_rust_plan_local(&plan, &cache).unwrap();

        assert_eq!(restored.restored_file_count, 5);
        assert_eq!(restored.skipped_count, 1);
        assert_eq!(restored.skipped_reasons.get("path_traversal"), Some(&1));
        assert!(!dir.path().join("escape.txt").exists());
    }

    #[test]
    fn safe_join_rejects_path_traversal() {
        let err = safe_join(Path::new("root"), "../outside").unwrap_err();
        assert!(matches!(err, RustPlanError::UnsafeRelativePath(_)));
    }

    #[test]
    fn package_name_parsing_handles_cargo_package_id_shapes() {
        assert_eq!(
            package_name_from_id(
                "registry+https://github.com/rust-lang/crates.io-index#serde@1.0.0"
            ),
            Some("serde".to_string())
        );
        assert_eq!(
            package_name_from_id("path+file:///repo#my-crate@0.1.0"),
            Some("my_crate".to_string())
        );
    }

    #[test]
    fn thin_package_exclusions_match_deps_fingerprint_and_build_by_package_stem() {
        let dir = tempfile::tempdir().unwrap();
        synthetic_target_with_package_exclusions(dir.path());
        let mut plan = sample_plan(dir.path(), RustPlanMode::Thin);
        plan.packages.workspace_package_ids =
            vec!["registry+https://github.com/rust-lang/crates.io-index#app@0.1.0".to_string()];
        plan.packages.excluded_path_package_ids =
            vec!["path+file:///workspace/local_dep#local-dep@0.1.0".to_string()];
        let cache = dir.path().join("cache");

        let saved = save_rust_plan_local(&plan, &cache).unwrap();
        assert_eq!(saved.saved_file_count, 6);
        assert_eq!(
            saved
                .skipped_reasons
                .get("workspace_or_path_dependency_excluded_by_plan"),
            Some(&12)
        );
        assert_eq!(saved.skipped_reasons.get("transient_state"), Some(&1));
        assert!(saved
            .skipped_samples
            .iter()
            .any(|sample| sample.path.ends_with("debug/deps/libapp-abc.rlib")));
        assert!(saved.skipped_samples.iter().any(|sample| sample
            .path
            .ends_with("debug/.fingerprint/app-abc/dep-lib-app")));
        assert!(saved.skipped_samples.iter().any(|sample| sample
            .path
            .ends_with("debug/build/local_dep-abc/out/gen.rs")));
    }
    #[test]
    fn from_json_str_accepts_utf8_bom() {
        let dir = tempfile::tempdir().unwrap();
        let plan = sample_plan(dir.path(), RustPlanMode::Thin);
        let json = serde_json::to_string(&plan).unwrap();
        let loaded = RustArtifactPlanV1::from_json_str(&format!("\u{feff}{json}")).unwrap();
        assert_eq!(loaded.schema_version, 1);
    }

    // ─── rust-plan tar thread resolver (issue #177) ──────────────────────

    #[test]
    fn tar_threads_parser_accepts_grammar_from_soldr_273() {
        // unset / auto / empty / whitespace → default (vCPU-bounded, capped at 8)
        let default = default_rust_plan_tar_threads();
        assert!((1..=DEFAULT_RUST_PLAN_TAR_THREADS_CAP).contains(&default));
        assert_eq!(parse_rust_plan_tar_threads(None), default);
        assert_eq!(parse_rust_plan_tar_threads(Some("auto")), default);
        assert_eq!(parse_rust_plan_tar_threads(Some("AUTO")), default);
        assert_eq!(parse_rust_plan_tar_threads(Some("")), default);
        assert_eq!(parse_rust_plan_tar_threads(Some("   ")), default);

        // 1 → sequential escape hatch
        assert_eq!(parse_rust_plan_tar_threads(Some("1")), 1);

        // Positive integer → clamped to MAX_RUST_PLAN_TAR_THREADS
        assert_eq!(parse_rust_plan_tar_threads(Some("4")), 4);
        assert_eq!(
            parse_rust_plan_tar_threads(Some("9999")),
            MAX_RUST_PLAN_TAR_THREADS
        );

        // Garbage / 0 → default (defensive)
        assert_eq!(parse_rust_plan_tar_threads(Some("0")), default);
        assert_eq!(parse_rust_plan_tar_threads(Some("not-a-number")), default);
        assert_eq!(parse_rust_plan_tar_threads(Some("-1")), default);
    }

    #[test]
    fn parallel_bundling_matches_sequential_byte_for_byte() {
        // `select_artifacts` pre-sorts by relative_path; with rayon's ordered
        // `par_iter().collect()` we must end up with the same artifact list,
        // same hashes, same sizes — regardless of thread count.
        fn bundle_with(threads: usize) -> Vec<RustBundledArtifact> {
            let dir = tempfile::tempdir().unwrap();
            synthetic_target(dir.path());
            let plan = sample_plan(dir.path(), RustPlanMode::Thin);

            let mut candidates = Vec::new();
            collect_files(plan.target_dir.as_path(), &mut candidates).unwrap();
            candidates.sort();
            let mut summary = RustPlanSummary::new(
                RustPlanOperation::Save,
                plan.mode,
                plan.schema_version,
                plan.cache_schema_version,
                rust_plan_cache_key(&plan),
                None,
                None,
            );
            let selected = select_artifacts(&plan, candidates, &mut summary);

            let files_dir = dir.path().join("out").join(format!("t{threads}"));
            std::fs::create_dir_all(&files_dir).unwrap();
            bundle_selected_artifacts_with_threads(&selected, &files_dir, threads).unwrap()
        }

        let sequential = bundle_with(1);
        let parallel = bundle_with(4);

        assert!(!sequential.is_empty());
        assert_eq!(sequential.len(), parallel.len());
        for (seq, par) in sequential.iter().zip(parallel.iter()) {
            assert_eq!(seq.relative_path, par.relative_path);
            assert_eq!(seq.size, par.size);
            assert_eq!(seq.content_hash, par.content_hash);
            assert_eq!(seq.class, par.class);
        }
    }
}
