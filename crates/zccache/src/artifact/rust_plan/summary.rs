//! Rust artifact plan summary and miss classification diagnostics.

use std::collections::BTreeMap;
use std::path::Path;

use crate::core::NormalizedPath;
use serde::ser::SerializeStruct;
use serde::{Deserialize, Serialize};

use super::local::{rust_plan_bundle_dir, rust_plan_cache_key};
use super::schema::{RustArtifactPlanV1, RustPlanError, RustPlanMode};

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

    pub(super) fn new(
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

    pub(super) fn skip(&mut self, path: impl Into<String>, reason: &'static str) {
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

    pub(super) fn refresh_effectiveness(&mut self, eligible: u64) {
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

pub(super) fn compile_cache_misses(stats: &serde_json::Value) -> u64 {
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
