//! Durable audit schema types for embedded zccache integrations.
//!
//! These types intentionally model the JSON contract without owning transport
//! or daemon hot-path behavior. Hosts can serialize them directly to JSONL or
//! adapt them into their own audit sink.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::BTreeMap;
use std::fmt;

/// Current durable audit event schema identifier.
pub const AUDIT_SCHEMA: &str = "soldr.audit.v1";

/// Current durable audit schema version.
pub const AUDIT_SCHEMA_VERSION: u32 = 1;

/// JSON object used for extensible event and finding payloads.
pub type AuditFields = Map<String, Value>;

/// Stable identifier for an audit run, trace, span, command, compile, session,
/// event, evidence item, or artifact.
///
/// The schema keeps identifiers as strings so host systems can use their own
/// UUID, ULID, content-addressed, or trace-context formats without zccache
/// pulling in extra dependencies.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AuditId(pub String);

impl AuditId {
    /// Create an identifier from a non-empty string.
    pub fn new(value: impl Into<String>) -> Result<Self, AuditValidationError> {
        let value = value.into();
        if value.trim().is_empty() {
            return Err(AuditValidationError::EmptyId);
        }
        Ok(Self(value))
    }

    /// Borrow the identifier as a string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<AuditId> for String {
    fn from(value: AuditId) -> Self {
        value.0
    }
}

/// Host-provided causal context attached to embedded zccache requests.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AuditContext {
    pub run_id: AuditId,
    pub build_id: Option<AuditId>,
    pub trace_id: AuditId,
    pub span_id: Option<AuditId>,
    pub parent_span_id: Option<AuditId>,
    pub command_id: Option<AuditId>,
    pub compile_id: Option<AuditId>,
    pub session_id: Option<AuditId>,
}

impl AuditContext {
    pub fn new(run_id: AuditId, trace_id: AuditId) -> Self {
        Self {
            run_id,
            build_id: None,
            trace_id,
            span_id: None,
            parent_span_id: None,
            command_id: None,
            compile_id: None,
            session_id: None,
        }
    }

    pub fn child_span(mut self, span_id: AuditId) -> Self {
        self.parent_span_id = self.span_id.replace(span_id);
        self
    }
}

/// Durable audit capture level selected by the host.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuditMode {
    Off,
    Summary,
    #[default]
    Normal,
    Verbose,
    Forensic,
}

/// Sink backpressure/failure policy selected by the host.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuditSinkPolicy {
    Block,
    DropLowPriority,
    Degrade,
    #[default]
    FailLossless,
}

/// Redaction policy metadata recorded with audit output.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AuditRedactionPolicy {
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub redact_env_keys: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub redact_field_keys: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allow_field_keys: Vec<String>,
    pub replacement: String,
}

impl Default for AuditRedactionPolicy {
    fn default() -> Self {
        Self {
            enabled: true,
            redact_env_keys: vec![
                "TOKEN".to_string(),
                "SECRET".to_string(),
                "PASSWORD".to_string(),
                "KEY".to_string(),
            ],
            redact_field_keys: Vec::new(),
            allow_field_keys: Vec::new(),
            replacement: "<redacted>".to_string(),
        }
    }
}

impl AuditRedactionPolicy {
    pub fn should_redact_key(&self, key: &str) -> bool {
        if !self.enabled || self.allow_field_keys.iter().any(|allowed| allowed == key) {
            return false;
        }

        let upper = key.to_ascii_uppercase();
        self.redact_field_keys.iter().any(|pattern| key == pattern)
            || self
                .redact_env_keys
                .iter()
                .any(|pattern| upper.contains(&pattern.to_ascii_uppercase()))
    }

    pub fn redact_fields(&self, fields: &mut AuditFields) {
        if !self.enabled {
            return;
        }

        for (key, value) in fields.iter_mut() {
            if self.should_redact_key(key) {
                *value = Value::String(self.replacement.clone());
            }
        }
    }
}

/// Host-selected durable audit configuration.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AuditConfig {
    pub mode: AuditMode,
    pub sink_policy: AuditSinkPolicy,
    pub output_root: Option<String>,
    pub event_log: Option<String>,
    pub summary: Option<String>,
    pub zccache_journal: Option<String>,
    pub trace: Option<String>,
    pub tokio_console: Option<String>,
    pub redaction: AuditRedactionPolicy,
}

impl Default for AuditConfig {
    fn default() -> Self {
        Self {
            mode: AuditMode::Normal,
            sink_policy: AuditSinkPolicy::FailLossless,
            output_root: None,
            event_log: Some("audit.jsonl".to_string()),
            summary: Some("summary.json".to_string()),
            zccache_journal: Some("zccache-journal.jsonl".to_string()),
            trace: None,
            tokio_console: None,
            redaction: AuditRedactionPolicy::default(),
        }
    }
}

/// Audit event category.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AuditCategory(pub String);

impl AuditCategory {
    pub const SOLDR_LIFECYCLE: &'static str = "soldr.lifecycle";
    pub const SOLDR_PLAN: &'static str = "soldr.plan";
    pub const SOLDR_EXECUTE: &'static str = "soldr.execute";
    pub const SOLDR_SCHEDULER: &'static str = "soldr.scheduler";
    pub const SOLDR_PROCESS: &'static str = "soldr.process";
    pub const SOLDR_CACHE: &'static str = "soldr.cache";
    pub const FBUILD_LIFECYCLE: &'static str = "fbuild.lifecycle";
    pub const FBUILD_PLAN: &'static str = "fbuild.plan";
    pub const FBUILD_EXECUTE: &'static str = "fbuild.execute";
    pub const ZCCACHE_SESSION: &'static str = "zccache.session";
    pub const ZCCACHE_COMPILE: &'static str = "zccache.compile";
    pub const ZCCACHE_CACHE_LOOKUP: &'static str = "zccache.cache_lookup";
    pub const ZCCACHE_DEPGRAPH: &'static str = "zccache.depgraph";
    pub const ZCCACHE_ARTIFACT_STORE: &'static str = "zccache.artifact_store";
    pub const ZCCACHE_COMPILER_EXEC: &'static str = "zccache.compiler_exec";
    pub const ZCCACHE_IPC: &'static str = "zccache.ipc";
    pub const RUNTIME_TOKIO: &'static str = "runtime.tokio";
    pub const SYSTEM_IO: &'static str = "system.io";
    pub const SYSTEM_CPU: &'static str = "system.cpu";

    pub fn new(value: impl Into<String>) -> Result<Self, AuditValidationError> {
        non_empty_newtype(value).map(Self)
    }
}

/// Audit event name.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AuditEventName(pub String);

impl AuditEventName {
    pub const RUN_STARTED: &'static str = "run.started";
    pub const RUN_FINISHED: &'static str = "run.finished";
    pub const PHASE_STARTED: &'static str = "phase.started";
    pub const PHASE_FINISHED: &'static str = "phase.finished";
    pub const TARGET_PLANNED: &'static str = "target.planned";
    pub const COMMAND_STARTED: &'static str = "command.started";
    pub const COMMAND_FINISHED: &'static str = "command.finished";
    pub const COMPILE_STARTED: &'static str = "compile.started";
    pub const COMPILE_FINISHED: &'static str = "compile.finished";
    pub const CACHE_LOOKUP: &'static str = "cache.lookup";
    pub const CACHE_HIT: &'static str = "cache.hit";
    pub const CACHE_MISS: &'static str = "cache.miss";
    pub const CACHE_STORE: &'static str = "cache.store";
    pub const DEPGRAPH_CHECK: &'static str = "depgraph.check";
    pub const DEPGRAPH_UPDATE: &'static str = "depgraph.update";
    pub const PROCESS_SPAWN: &'static str = "process.spawn";
    pub const PROCESS_EXIT: &'static str = "process.exit";
    pub const RESOURCE_WAIT: &'static str = "resource.wait";
    pub const RUNTIME_TASK_BLOCKED: &'static str = "runtime.task.blocked";

    pub fn new(value: impl Into<String>) -> Result<Self, AuditValidationError> {
        non_empty_newtype(value).map(Self)
    }
}

/// Audit event severity.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuditLevel {
    Trace,
    Debug,
    #[default]
    Info,
    Warn,
    Error,
}

/// One durable audit event, suitable for JSONL serialization.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AuditEvent {
    pub schema: String,
    pub schema_version: u32,
    pub event_id: AuditId,
    pub run_id: AuditId,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub build_id: Option<AuditId>,
    pub trace_id: AuditId,
    pub span_id: AuditId,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_span_id: Option<AuditId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command_id: Option<AuditId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compile_id: Option<AuditId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<AuditId>,
    pub category: AuditCategory,
    pub event: AuditEventName,
    pub level: AuditLevel,
    pub mode: AuditMode,
    /// UTC timestamp formatted by the writer, normally RFC 3339.
    pub ts: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ns: Option<u64>,
    #[serde(default, skip_serializing_if = "AuditFields::is_empty")]
    pub fields: AuditFields,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence_ids: Vec<AuditId>,
}

impl AuditEvent {
    pub fn new(
        event_id: AuditId,
        context: AuditContext,
        span_id: AuditId,
        category: AuditCategory,
        event: AuditEventName,
        ts: impl Into<String>,
    ) -> Self {
        Self {
            schema: AUDIT_SCHEMA.to_string(),
            schema_version: AUDIT_SCHEMA_VERSION,
            event_id,
            run_id: context.run_id,
            build_id: context.build_id,
            trace_id: context.trace_id,
            span_id,
            parent_span_id: context.parent_span_id.or(context.span_id),
            command_id: context.command_id,
            compile_id: context.compile_id,
            session_id: context.session_id,
            category,
            event,
            level: AuditLevel::Info,
            mode: AuditMode::Normal,
            ts: ts.into(),
            duration_ns: None,
            fields: AuditFields::new(),
            evidence_ids: Vec::new(),
        }
    }

    pub fn with_field(mut self, key: impl Into<String>, value: impl Into<Value>) -> Self {
        self.fields.insert(key.into(), value.into());
        self
    }

    pub fn apply_redaction(mut self, policy: &AuditRedactionPolicy) -> Self {
        policy.redact_fields(&mut self.fields);
        self
    }
}

/// Machine-readable recommendation emitted by audit analysis.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AuditFinding {
    pub finding_id: AuditId,
    pub severity: AuditFindingSeverity,
    pub confidence: f32,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence_event_ids: Vec<AuditId>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence_artifact_ids: Vec<AuditId>,
    #[serde(default, skip_serializing_if = "AuditFields::is_empty")]
    pub estimated_impact: AuditFields,
    pub suggested_action: String,
    pub needs_reproduction: bool,
    #[serde(default, skip_serializing_if = "AuditFields::is_empty")]
    pub fields: AuditFields,
}

impl AuditFinding {
    pub fn new(
        finding_id: AuditId,
        severity: AuditFindingSeverity,
        confidence: f32,
        suggested_action: impl Into<String>,
    ) -> Self {
        Self {
            finding_id,
            severity,
            confidence,
            evidence_event_ids: Vec::new(),
            evidence_artifact_ids: Vec::new(),
            estimated_impact: AuditFields::new(),
            suggested_action: suggested_action.into(),
            needs_reproduction: false,
            fields: AuditFields::new(),
        }
    }

    pub fn validate(&self) -> Result<(), AuditValidationError> {
        if !(0.0..=1.0).contains(&self.confidence) {
            return Err(AuditValidationError::InvalidConfidence);
        }
        if self.suggested_action.trim().is_empty() {
            return Err(AuditValidationError::EmptySuggestedAction);
        }
        if self.evidence_event_ids.is_empty() && self.evidence_artifact_ids.is_empty() {
            return Err(AuditValidationError::MissingEvidence);
        }
        Ok(())
    }
}

/// Audit finding severity.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuditFindingSeverity {
    Low,
    Medium,
    High,
    Critical,
}

/// Manifest for a completed audit run.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AuditRunManifest {
    pub schema: String,
    pub schema_version: u32,
    pub run_id: AuditId,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub build_id: Option<AuditId>,
    pub mode: AuditMode,
    pub started_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<String>,
    pub summary: String,
    pub events: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub zccache_journal: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trace: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tokio_console: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub artifacts: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub metadata: BTreeMap<String, String>,
}

impl AuditRunManifest {
    pub fn new(run_id: AuditId, mode: AuditMode, started_at: impl Into<String>) -> Self {
        Self {
            schema: AUDIT_SCHEMA.to_string(),
            schema_version: AUDIT_SCHEMA_VERSION,
            run_id,
            build_id: None,
            mode,
            started_at: started_at.into(),
            finished_at: None,
            summary: "summary.json".to_string(),
            events: "audit.jsonl".to_string(),
            zccache_journal: Some("zccache-journal.jsonl".to_string()),
            trace: None,
            tokio_console: None,
            artifacts: Some("artifacts/".to_string()),
            metadata: BTreeMap::new(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AuditValidationError {
    EmptyId,
    InvalidConfidence,
    EmptySuggestedAction,
    MissingEvidence,
}

impl fmt::Display for AuditValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::EmptyId => "audit identifier must not be empty",
            Self::InvalidConfidence => "audit finding confidence must be between 0.0 and 1.0",
            Self::EmptySuggestedAction => "audit finding suggested_action must not be empty",
            Self::MissingEvidence => {
                "audit finding must reference at least one event or artifact evidence id"
            }
        };
        f.write_str(message)
    }
}

impl std::error::Error for AuditValidationError {}

fn non_empty_newtype(value: impl Into<String>) -> Result<String, AuditValidationError> {
    let value = value.into();
    if value.trim().is_empty() {
        return Err(AuditValidationError::EmptyId);
    }
    Ok(value)
}
