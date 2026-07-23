//! Shared aggregate log-audit engine for perf and integration gates.
//!
//! Rust owns every rule and every source parser. Python and shell harnesses
//! invoke the `zccache-ci audit-logs` adapter instead of duplicating patterns.

use serde::Serialize;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs;
use std::path::Path;
use zccache_core::path::NormalizedPath;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct RuleId(pub &'static str);

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum LogAuditContext {
    Perf,
    Integration,
}

impl std::str::FromStr for LogAuditContext {
    type Err = AuditError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "perf" => Ok(Self::Perf),
            "integration" => Ok(Self::Integration),
            _ => Err(AuditError::Context(value.to_string())),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LogSource {
    CompileJournal,
    LifecycleLog,
    AuditJsonl,
    DaemonEventLog,
    Directory,
}

#[derive(Clone, Copy, Debug)]
pub enum FieldMatch {
    Equals {
        key: &'static str,
        value: &'static str,
    },
    NotEquals {
        key: &'static str,
        value: &'static str,
    },
    OneOf {
        key: &'static str,
        values: &'static [&'static str],
    },
    Exists {
        key: &'static str,
    },
}

#[derive(Clone, Copy, Debug)]
pub struct EventPattern {
    pub event: Option<&'static str>,
    pub fields: &'static [FieldMatch],
    pub text_contains: Option<&'static str>,
}

impl EventPattern {
    pub const fn event(event: &'static str) -> Self {
        Self {
            event: Some(event),
            fields: &[],
            text_contains: None,
        }
    }

    pub const fn fields(fields: &'static [FieldMatch]) -> Self {
        Self {
            event: None,
            fields,
            text_contains: None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DirectoryCheck {
    Exists,
    Missing,
}

#[derive(Clone, Copy, Debug)]
pub enum RequiredCheck {
    MatchingRows {
        source: LogSource,
        pattern: EventPattern,
        minimum: usize,
    },
    WarmRun {
        minimum_hits: usize,
    },
    Directory {
        relative_path: &'static str,
        check: DirectoryCheck,
    },
}

#[derive(Clone, Copy, Debug)]
pub enum AuditRule {
    MalformedSourceLines,
    Forbidden {
        source: LogSource,
        pattern: EventPattern,
    },
    ForbiddenAny {
        checks: &'static [SourcePattern],
    },
    Bounded {
        source: LogSource,
        pattern: EventPattern,
        maximum: usize,
    },
    Required {
        check: RequiredCheck,
    },
}

#[derive(Clone, Copy, Debug)]
pub struct SourcePattern {
    pub source: LogSource,
    pub pattern: EventPattern,
}

#[derive(Clone, Copy)]
pub struct RuleRegistration {
    pub id: RuleId,
    pub owner_issue: u32,
    pub contexts: &'static [LogAuditContext],
    pub rule: fn() -> AuditRule,
}

const PERF: &[LogAuditContext] = &[LogAuditContext::Perf];
const ALL: &[LogAuditContext] = &[LogAuditContext::Perf, LogAuditContext::Integration];
const MISS_OUTCOMES: &[&str] = &["miss", "link_miss"];
const HIT_OUTCOMES: &[&str] = &["hit", "link_hit"];
const UNKNOWN_MISS: &[FieldMatch] = &[
    FieldMatch::OneOf {
        key: "outcome",
        values: MISS_OUTCOMES,
    },
    FieldMatch::Equals {
        key: "miss_reason",
        value: "unknown",
    },
];
const NON_MIGRATION_LEGACY_ACCESS: &[FieldMatch] = &[FieldMatch::NotEquals {
    key: "purpose",
    value: "migration",
}];
const EVICTED_DESTINATION_FAILURE: &[FieldMatch] = &[FieldMatch::Equals {
    key: "evicted",
    value: "true",
}];

const UNKNOWN_CHECKS: &[SourcePattern] = &[
    SourcePattern {
        source: LogSource::CompileJournal,
        pattern: EventPattern::fields(UNKNOWN_MISS),
    },
    SourcePattern {
        source: LogSource::LifecycleLog,
        pattern: EventPattern::event("miss_reason_unknown"),
    },
];

fn no_unknown_miss_reason() -> AuditRule {
    AuditRule::ForbiddenAny {
        checks: UNKNOWN_CHECKS,
    }
}

fn no_legacy_path_access() -> AuditRule {
    AuditRule::Forbidden {
        source: LogSource::LifecycleLog,
        pattern: EventPattern {
            event: Some("legacy_artifact_path_accessed"),
            fields: NON_MIGRATION_LEGACY_ACCESS,
            text_contains: None,
        },
    }
}

fn no_destination_error_eviction() -> AuditRule {
    AuditRule::Forbidden {
        source: LogSource::LifecycleLog,
        pattern: EventPattern {
            event: Some("destination_write_failed"),
            fields: EVICTED_DESTINATION_FAILURE,
            text_contains: None,
        },
    }
}

fn no_wrapper_local_fallback() -> AuditRule {
    AuditRule::Forbidden {
        source: LogSource::LifecycleLog,
        pattern: EventPattern::event("wrapper-local-fallback"),
    }
}

fn bounded_publication_conflicts() -> AuditRule {
    AuditRule::Bounded {
        source: LogSource::LifecycleLog,
        pattern: EventPattern::event("staged_publication_conflict"),
        maximum: 1,
    }
}

fn warm_run_has_hits() -> AuditRule {
    AuditRule::Required {
        check: RequiredCheck::WarmRun { minimum_hits: 1 },
    }
}

fn malformed_source_lines() -> AuditRule {
    AuditRule::MalformedSourceLines
}

/// Single source of truth for rule ids, owners, contexts, and constructors.
pub const REGISTRY: &[RuleRegistration] = &[
    RuleRegistration {
        id: RuleId("malformed-log-line"),
        owner_issue: 1159,
        contexts: ALL,
        rule: malformed_source_lines,
    },
    RuleRegistration {
        id: RuleId("no-unknown-miss-reason"),
        owner_issue: 1155,
        contexts: ALL,
        rule: no_unknown_miss_reason,
    },
    RuleRegistration {
        id: RuleId("no-legacy-path-access"),
        owner_issue: 1152,
        contexts: ALL,
        rule: no_legacy_path_access,
    },
    RuleRegistration {
        id: RuleId("no-destination-error-eviction"),
        owner_issue: 1155,
        contexts: ALL,
        rule: no_destination_error_eviction,
    },
    RuleRegistration {
        id: RuleId("no-wrapper-local-fallback"),
        owner_issue: 1159,
        contexts: ALL,
        rule: no_wrapper_local_fallback,
    },
    RuleRegistration {
        id: RuleId("bounded-publication-conflicts"),
        owner_issue: 1159,
        contexts: ALL,
        rule: bounded_publication_conflicts,
    },
    RuleRegistration {
        id: RuleId("warm-run-has-hits"),
        owner_issue: 1159,
        contexts: PERF,
        rule: warm_run_has_hits,
    },
];

#[derive(Clone, Debug, Default)]
pub struct AuditOptions {
    test_allow: Option<TestAllow>,
}

#[derive(Clone, Debug)]
struct TestAllow {
    test_name: String,
    rule_ids: BTreeSet<RuleId>,
}

impl AuditOptions {
    /// Explicitly exempt rules for one named negative test.
    #[must_use]
    pub fn allow_for_test(
        mut self,
        test_name: impl Into<String>,
        rule_ids: impl IntoIterator<Item = RuleId>,
    ) -> Self {
        let test_name = test_name.into();
        assert!(
            !test_name.trim().is_empty(),
            "log-audit test exemptions require a visible test name"
        );
        let rule_ids = rule_ids.into_iter().collect::<BTreeSet<_>>();
        assert!(
            rule_ids
                .iter()
                .all(|id| REGISTRY.iter().any(|registration| registration.id == *id)),
            "log-audit test exemptions must name registered rule ids"
        );
        self.test_allow = Some(TestAllow {
            test_name,
            rule_ids,
        });
        self
    }

    fn allows(&self, id: RuleId) -> bool {
        self.test_allow
            .as_ref()
            .is_some_and(|allow| allow.rule_ids.contains(&id))
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct Violation {
    pub rule_id: RuleId,
    pub source_kind: LogSource,
    pub source: NormalizedPath,
    pub line: Option<usize>,
    pub message: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct AuditReport {
    pub cache_root: NormalizedPath,
    pub context: LogAuditContext,
    pub test_allow_name: Option<String>,
    pub violations: Vec<Violation>,
}

impl AuditReport {
    pub fn passed(&self) -> bool {
        self.violations.is_empty()
    }

    pub fn format_human(&self) -> String {
        if self.passed() {
            return format!("log audit passed: {}\n", self.cache_root.display());
        }
        let mut output = format!(
            "log audit failed: {} violation(s) under {}\n",
            self.violations.len(),
            self.cache_root.display()
        );
        for violation in &self.violations {
            let line = violation
                .line
                .map(|line| format!(":{line}"))
                .unwrap_or_default();
            output.push_str(&format!(
                "{} [{:?}] {}{}: {}\n",
                violation.rule_id.0,
                violation.source_kind,
                violation.source.display(),
                line,
                violation.message
            ));
        }
        output
    }
}

#[derive(Debug)]
pub enum AuditError {
    Io(std::io::Error),
    Context(String),
}

impl fmt::Display for AuditError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => error.fmt(formatter),
            Self::Context(value) => write!(formatter, "unknown audit context {value:?}"),
        }
    }
}

impl std::error::Error for AuditError {}

impl From<std::io::Error> for AuditError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

#[derive(Debug)]
struct ParsedLine {
    source_kind: LogSource,
    path: NormalizedPath,
    line: usize,
    text: String,
    json: Option<Value>,
}

#[derive(Default)]
struct ParsedSources {
    lines: Vec<ParsedLine>,
    malformed: Vec<Violation>,
}

/// Parse every supported source once and aggregate every matching violation.
pub fn audit_cache_root(
    root: &Path,
    context: LogAuditContext,
    options: &AuditOptions,
) -> Result<AuditReport, AuditError> {
    let metadata = fs::metadata(root)?;
    if !metadata.is_dir() {
        return Err(AuditError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("audit cache root is not a directory: {}", root.display()),
        )));
    }
    let parsed = parse_sources(root)?;
    let mut report = AuditReport {
        cache_root: root.into(),
        context,
        test_allow_name: options
            .test_allow
            .as_ref()
            .map(|allow| allow.test_name.clone()),
        violations: Vec::new(),
    };

    for registration in REGISTRY.iter().filter(|registration| {
        registration.contexts.contains(&context) && !options.allows(registration.id)
    }) {
        apply_rule(
            root,
            &parsed.lines,
            &parsed.malformed,
            *registration,
            &mut report.violations,
        );
    }
    Ok(report)
}

fn apply_rule(
    root: &Path,
    lines: &[ParsedLine],
    malformed: &[Violation],
    registration: RuleRegistration,
    violations: &mut Vec<Violation>,
) {
    match (registration.rule)() {
        AuditRule::MalformedSourceLines => {
            violations.extend(malformed.iter().cloned());
        }
        AuditRule::Forbidden { source, pattern } => {
            for line in matching_lines(lines, source, pattern) {
                violations.push(Violation {
                    rule_id: registration.id,
                    source_kind: source,
                    source: line.path.clone(),
                    line: Some(line.line),
                    message: "forbidden log record matched".to_string(),
                });
            }
        }
        AuditRule::ForbiddenAny { checks } => {
            for check in checks {
                for line in matching_lines(lines, check.source, check.pattern) {
                    violations.push(Violation {
                        rule_id: registration.id,
                        source_kind: check.source,
                        source: line.path.clone(),
                        line: Some(line.line),
                        message: "forbidden log record matched".to_string(),
                    });
                }
            }
        }
        AuditRule::Bounded {
            source,
            pattern,
            maximum,
        } => {
            let matched = matching_lines(lines, source, pattern);
            if matched.len() > maximum {
                violations.push(Violation {
                    rule_id: registration.id,
                    source_kind: source,
                    source: root.into(),
                    line: None,
                    message: format!(
                        "observed {} matching records; maximum is {maximum}",
                        matched.len()
                    ),
                });
            }
        }
        AuditRule::Required { check } => {
            apply_required(root, lines, registration.id, check, violations);
        }
    }
}

fn apply_required(
    root: &Path,
    lines: &[ParsedLine],
    id: RuleId,
    check: RequiredCheck,
    violations: &mut Vec<Violation>,
) {
    match check {
        RequiredCheck::MatchingRows {
            source,
            pattern,
            minimum,
        } => {
            let count = matching_lines(lines, source, pattern).len();
            if count < minimum {
                violations.push(aggregate_violation(
                    id,
                    source,
                    root,
                    format!("observed {count} matching records; required at least {minimum}"),
                ));
            }
        }
        RequiredCheck::WarmRun { minimum_hits } => {
            let hit_pattern = EventPattern::fields(&[FieldMatch::OneOf {
                key: "outcome",
                values: HIT_OUTCOMES,
            }]);
            let hits = matching_lines(lines, LogSource::CompileJournal, hit_pattern).len();
            if hits < minimum_hits {
                violations.push(aggregate_violation(
                    id,
                    LogSource::CompileJournal,
                    root,
                    format!("warm scenario recorded {hits} hits; required at least {minimum_hits}"),
                ));
            }

            // Explicit warm markers are authoritative. Older journals lack
            // them, so fall back to request identity: after an identity's
            // first cold observation, every repeated warm observation must
            // be a hit.
            let warm_rows = lines
                .iter()
                .filter(|line| line.source_kind == LogSource::CompileJournal)
                .filter(|line| {
                    line.json.as_ref().is_some_and(|row| {
                        matches!(
                            json_string(row, "phase").or_else(|| json_string(row, "scenario")),
                            Some("warm")
                        )
                    })
                })
                .collect::<Vec<_>>();
            if warm_rows.is_empty() {
                apply_repeated_request_warm_check(root, lines, id, violations);
            } else {
                for line in warm_rows.into_iter().filter(|line| {
                    !line.json.as_ref().is_some_and(|row| {
                        matches!(json_string(row, "outcome"), Some("hit" | "link_hit"))
                    })
                }) {
                    violations.push(Violation {
                        rule_id: id,
                        source_kind: LogSource::CompileJournal,
                        source: line.path.clone(),
                        line: Some(line.line),
                        message:
                            "warm-marked cached translation unit did not complete as a cache hit"
                                .to_string(),
                    });
                }
            }
        }
        RequiredCheck::Directory {
            relative_path,
            check,
        } => {
            let path = root.join(relative_path);
            let exists = path.is_dir();
            let passed = match check {
                DirectoryCheck::Exists => exists,
                DirectoryCheck::Missing => !exists,
            };
            if !passed {
                violations.push(aggregate_violation(
                    id,
                    LogSource::Directory,
                    &path,
                    format!("directory check {check:?} failed"),
                ));
            }
        }
    }
}

fn apply_repeated_request_warm_check(
    root: &Path,
    lines: &[ParsedLine],
    id: RuleId,
    violations: &mut Vec<Violation>,
) {
    let mut by_identity: BTreeMap<String, Vec<&ParsedLine>> = BTreeMap::new();
    for line in lines
        .iter()
        .filter(|line| line.source_kind == LogSource::CompileJournal)
    {
        let Some(row) = line.json.as_ref() else {
            continue;
        };
        let Some(identity) = journal_request_identity(row) else {
            continue;
        };
        by_identity.entry(identity).or_default().push(line);
    }
    let mut repeated_identities = 0_usize;
    for observations in by_identity.values().filter(|rows| rows.len() > 1) {
        repeated_identities = repeated_identities.saturating_add(1);
        for line in observations.iter().skip(1) {
            if !line
                .json
                .as_ref()
                .is_some_and(|row| matches!(json_string(row, "outcome"), Some("hit" | "link_hit")))
            {
                violations.push(Violation {
                    rule_id: id,
                    source_kind: LogSource::CompileJournal,
                    source: line.path.clone(),
                    line: Some(line.line),
                    message: "repeated warm request did not complete as a cache hit".to_string(),
                });
            }
        }
    }
    if repeated_identities == 0 {
        violations.push(aggregate_violation(
            id,
            LogSource::CompileJournal,
            root,
            "no request identity was exercised in both cold and warm phases".to_string(),
        ));
    }
}

fn journal_request_identity(row: &Value) -> Option<String> {
    let compiler = json_path(row, "compiler")?;
    let args = json_path(row, "args")?;
    let cwd = json_path(row, "cwd")?;
    serde_json::to_string(&(compiler, args, cwd)).ok()
}

fn aggregate_violation(
    id: RuleId,
    source_kind: LogSource,
    source: &Path,
    message: String,
) -> Violation {
    Violation {
        rule_id: id,
        source_kind,
        source: source.into(),
        line: None,
        message,
    }
}

fn matching_lines(
    lines: &[ParsedLine],
    source: LogSource,
    pattern: EventPattern,
) -> Vec<&ParsedLine> {
    lines
        .iter()
        .filter(|line| line.source_kind == source && pattern_matches(line, pattern))
        .collect()
}

fn pattern_matches(line: &ParsedLine, pattern: EventPattern) -> bool {
    if let Some(needle) = pattern.text_contains {
        if !line.text.contains(needle) {
            return false;
        }
    }
    let Some(row) = line.json.as_ref() else {
        return pattern.event.is_none() && pattern.fields.is_empty();
    };
    if let Some(event) = pattern.event {
        let actual = json_string(row, "event")
            .or_else(|| json_string(row, "name"))
            .or_else(|| json_string(row, "event_name"));
        if actual != Some(event) {
            return false;
        }
    }
    pattern
        .fields
        .iter()
        .all(|field| field_matches(row, *field))
}

fn field_matches(row: &Value, matcher: FieldMatch) -> bool {
    match matcher {
        FieldMatch::Equals { key, value } => json_scalar(row, key).as_deref() == Some(value),
        FieldMatch::NotEquals { key, value } => json_scalar(row, key).as_deref() != Some(value),
        FieldMatch::OneOf { key, values } => json_scalar(row, key)
            .as_deref()
            .is_some_and(|actual| values.contains(&actual)),
        FieldMatch::Exists { key } => json_path(row, key).is_some(),
    }
}

fn json_string<'a>(row: &'a Value, key: &str) -> Option<&'a str> {
    json_path(row, key)?.as_str()
}

fn json_scalar(row: &Value, key: &str) -> Option<String> {
    match json_path(row, key)? {
        Value::String(value) => Some(value.clone()),
        Value::Bool(value) => Some(value.to_string()),
        Value::Number(value) => Some(value.to_string()),
        _ => None,
    }
}

fn json_path<'a>(row: &'a Value, key: &str) -> Option<&'a Value> {
    key.split('.')
        .try_fold(row, |value, component| value.get(component))
}

fn parse_sources(root: &Path) -> Result<ParsedSources, AuditError> {
    let mut files = Vec::new();
    collect_files(root, &mut files)?;
    files.sort();

    let mut parsed = ParsedSources::default();
    for path in files {
        let Some(source_kind) = classify_source(path.as_path()) else {
            continue;
        };
        let content = fs::read_to_string(path.as_path())?;
        for (offset, text) in content.lines().enumerate() {
            if text.trim().is_empty() {
                continue;
            }
            let line = offset + 1;
            if source_kind == LogSource::DaemonEventLog {
                parsed.lines.push(ParsedLine {
                    source_kind,
                    path: path.clone(),
                    line,
                    text: text.to_string(),
                    json: None,
                });
                continue;
            }
            match serde_json::from_str::<Value>(text) {
                Ok(json) if json.is_object() => parsed.lines.push(ParsedLine {
                    source_kind,
                    path: path.clone(),
                    line,
                    text: text.to_string(),
                    json: Some(json),
                }),
                Ok(_) => parsed.malformed.push(Violation {
                    rule_id: RuleId("malformed-log-line"),
                    source_kind,
                    source: path.clone(),
                    line: Some(line),
                    message: "JSONL row must be an object".to_string(),
                }),
                Err(error) => parsed.malformed.push(Violation {
                    rule_id: RuleId("malformed-log-line"),
                    source_kind,
                    source: path.clone(),
                    line: Some(line),
                    message: format!("invalid JSONL: {error}"),
                }),
            }
        }
    }
    Ok(parsed)
}

fn collect_files(directory: &Path, output: &mut Vec<NormalizedPath>) -> Result<(), AuditError> {
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let path: NormalizedPath = entry.path().into();
        let kind = entry.file_type()?;
        if kind.is_symlink() {
            continue;
        }
        if kind.is_dir() {
            collect_files(path.as_path(), output)?;
        } else if kind.is_file() {
            output.push(path);
        }
    }
    Ok(())
}

fn classify_source(path: &Path) -> Option<LogSource> {
    let name = path.file_name()?.to_str()?;
    if name == "audit.jsonl" || name.starts_with("audit.jsonl.") {
        return Some(LogSource::AuditJsonl);
    }
    if name.ends_with(".jsonl") || name.contains(".jsonl.") {
        return Some(LogSource::CompileJournal);
    }
    if name.starts_with("daemon-lifecycle") && name.contains(".log") {
        return Some(LogSource::LifecycleLog);
    }
    if name == "daemon.log" || name.starts_with("daemon.log.") {
        return Some(LogSource::DaemonEventLog);
    }
    None
}

/// JSON snapshot used by the cross-language registry drift guard.
///
/// # Errors
///
/// Returns a serialization error if a future registry field cannot be encoded.
pub fn registry_json() -> Result<String, serde_json::Error> {
    #[derive(Serialize)]
    struct Registry {
        rules: Vec<RuleRecord>,
    }
    #[derive(Serialize)]
    struct RuleRecord {
        id: &'static str,
        owner_issue: u32,
        contexts: Vec<LogAuditContext>,
    }

    let rules = REGISTRY
        .iter()
        .map(|registration| RuleRecord {
            id: registration.id.0,
            owner_issue: registration.owner_issue,
            contexts: registration.contexts.to_vec(),
        })
        .collect();
    serde_json::to_string_pretty(&Registry { rules }).map(|json| json + "\n")
}

/// In-process integration teardown guard. Call [`Self::finish`] for a normal
/// assertion path; `Drop` remains a belt-and-suspenders gate.
pub struct CacheRootAuditGuard {
    root: NormalizedPath,
    context: LogAuditContext,
    options: AuditOptions,
    finished: bool,
}

impl CacheRootAuditGuard {
    pub fn integration(root: impl Into<NormalizedPath>) -> Self {
        Self {
            root: root.into(),
            context: LogAuditContext::Integration,
            options: AuditOptions::default(),
            finished: false,
        }
    }

    #[must_use]
    pub fn allow_for_test(
        mut self,
        test_name: impl Into<String>,
        rule_ids: impl IntoIterator<Item = RuleId>,
    ) -> Self {
        self.options = std::mem::take(&mut self.options).allow_for_test(test_name, rule_ids);
        self
    }

    pub fn finish(mut self) -> Result<AuditReport, AuditError> {
        self.finished = true;
        audit_cache_root(self.root.as_path(), self.context, &self.options)
    }
}

impl Drop for CacheRootAuditGuard {
    fn drop(&mut self) {
        if self.finished || std::thread::panicking() {
            return;
        }
        match audit_cache_root(self.root.as_path(), self.context, &self.options) {
            Ok(report) => assert!(report.passed(), "{}", report.format_human()),
            Err(error) => panic!("log audit could not scan {}: {error}", self.root.display()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, contents: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, contents).unwrap();
    }

    #[test]
    fn scans_all_json_sources_once_and_aggregates_violations() {
        let root = tempfile::tempdir().unwrap();
        write(
            &root.path().join("logs/compile_journal.jsonl"),
            "{\"outcome\":\"miss\",\"miss_reason\":\"unknown\"}\nnot-json\n",
        );
        write(
            &root.path().join("logs/daemon-lifecycle.log"),
            concat!(
                "{\"event\":\"wrapper-local-fallback\"}\n",
                "{\"event\":\"legacy_artifact_path_accessed\",\"purpose\":\"compatibility_read\"}\n",
                "{\"event\":\"destination_write_failed\",\"evicted\":true}\n",
                "{\"event\":\"miss_reason_unknown\"}\n"
            ),
        );
        let report = audit_cache_root(
            root.path(),
            LogAuditContext::Integration,
            &AuditOptions::default(),
        )
        .unwrap();
        let ids = report
            .violations
            .iter()
            .map(|violation| violation.rule_id.0)
            .collect::<BTreeSet<_>>();
        assert_eq!(
            ids,
            BTreeSet::from([
                "malformed-log-line",
                "no-destination-error-eviction",
                "no-legacy-path-access",
                "no-unknown-miss-reason",
                "no-wrapper-local-fallback",
            ])
        );
    }

    #[test]
    fn migration_access_and_non_evicting_destination_failure_are_allowed() {
        let root = tempfile::tempdir().unwrap();
        write(
            &root.path().join("logs/daemon-lifecycle.log"),
            concat!(
                "{\"event\":\"legacy_artifact_path_accessed\",\"purpose\":\"migration\"}\n",
                "{\"event\":\"destination_write_failed\",\"evicted\":false}\n"
            ),
        );
        let report = audit_cache_root(
            root.path(),
            LogAuditContext::Integration,
            &AuditOptions::default(),
        )
        .unwrap();
        assert!(report.passed(), "{}", report.format_human());
    }

    #[test]
    fn perf_requires_hits_and_rejects_warm_marked_misses() {
        let root = tempfile::tempdir().unwrap();
        write(
            &root.path().join("logs/compile_journal.jsonl"),
            concat!(
                "{\"outcome\":\"hit\",\"scenario\":\"warm\"}\n",
                "{\"outcome\":\"miss\",\"miss_reason\":\"context_not_found\",\"scenario\":\"warm\"}\n"
            ),
        );
        let report =
            audit_cache_root(root.path(), LogAuditContext::Perf, &AuditOptions::default()).unwrap();
        assert_eq!(
            report
                .violations
                .iter()
                .filter(|violation| violation.rule_id.0 == "warm-run-has-hits")
                .count(),
            1
        );
    }

    #[test]
    fn perf_rejects_every_warm_marked_non_hit_outcome() {
        let root = tempfile::tempdir().unwrap();
        write(
            &root.path().join("logs/compile_journal.jsonl"),
            concat!(
                "{\"outcome\":\"hit\",\"scenario\":\"warm\"}\n",
                "{\"outcome\":\"direct\",\"scenario\":\"warm\"}\n",
                "{\"scenario\":\"warm\"}\n"
            ),
        );
        let report =
            audit_cache_root(root.path(), LogAuditContext::Perf, &AuditOptions::default()).unwrap();
        assert_eq!(
            report
                .violations
                .iter()
                .filter(|violation| violation.rule_id.0 == "warm-run-has-hits")
                .count(),
            2
        );
    }

    #[test]
    fn bounded_rule_reports_one_aggregate_violation() {
        let root = tempfile::tempdir().unwrap();
        write(
            &root.path().join("logs/daemon-lifecycle.log"),
            concat!(
                "{\"event\":\"staged_publication_conflict\"}\n",
                "{\"event\":\"staged_publication_conflict\"}\n"
            ),
        );
        let report = audit_cache_root(
            root.path(),
            LogAuditContext::Integration,
            &AuditOptions::default(),
        )
        .unwrap();
        let conflicts = report
            .violations
            .iter()
            .filter(|violation| violation.rule_id.0 == "bounded-publication-conflicts")
            .collect::<Vec<_>>();
        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].line, None);
    }

    #[test]
    fn recognizes_every_supported_source_and_malformed_audit_rows() {
        let root = tempfile::tempdir().unwrap();
        write(
            &root.path().join("logs/compile_journal.jsonl"),
            "{\"outcome\":\"hit\"}\n",
        );
        write(
            &root.path().join("sessions/session-one.jsonl"),
            "{\"outcome\":\"hit\"}\n",
        );
        write(
            &root.path().join("logs/daemon-lifecycle.log"),
            "{\"event\":\"spawn\"}\n",
        );
        write(&root.path().join("logs/daemon.log"), "human event line\n");
        write(&root.path().join("audit.jsonl"), "not-json\n");

        let parsed = parse_sources(root.path()).unwrap();
        let sources = parsed
            .lines
            .iter()
            .map(|line| line.source_kind)
            .collect::<BTreeSet<_>>();
        assert_eq!(
            sources,
            BTreeSet::from([
                LogSource::CompileJournal,
                LogSource::LifecycleLog,
                LogSource::DaemonEventLog,
            ])
        );
        assert_eq!(parsed.malformed.len(), 1);
        assert_eq!(parsed.malformed[0].source_kind, LogSource::AuditJsonl);
    }

    #[test]
    fn nonexistent_cache_root_is_an_audit_error() {
        let root = tempfile::tempdir().unwrap();
        let missing = root.path().join("missing");
        let error = audit_cache_root(
            &missing,
            LogAuditContext::Integration,
            &AuditOptions::default(),
        )
        .unwrap_err();
        assert!(matches!(error, AuditError::Io(_)));

        // `finish` marks the guard consumed before scanning, so this error
        // must not trigger a second panic when the guard is dropped.
        let error = CacheRootAuditGuard::integration(&missing)
            .finish()
            .unwrap_err();
        assert!(matches!(error, AuditError::Io(_)));
    }

    #[test]
    fn named_test_allow_is_narrow() {
        let root = tempfile::tempdir().unwrap();
        write(
            &root.path().join("logs/daemon-lifecycle.log"),
            "{\"event\":\"wrapper-local-fallback\"}\n",
        );
        let options = AuditOptions::default().allow_for_test(
            "wrapper_fallback_negative_fixture",
            [RuleId("no-wrapper-local-fallback")],
        );
        let report = audit_cache_root(root.path(), LogAuditContext::Integration, &options).unwrap();
        assert!(report.passed(), "{}", report.format_human());
        assert_eq!(
            report.test_allow_name.as_deref(),
            Some("wrapper_fallback_negative_fixture")
        );
    }

    #[test]
    fn registry_ids_are_unique_and_constructors_match() {
        let ids = REGISTRY
            .iter()
            .map(|registration| registration.id)
            .collect::<BTreeSet<_>>();
        assert_eq!(ids.len(), REGISTRY.len());
        for registration in REGISTRY {
            let _ = (registration.rule)();
        }
        assert_eq!(
            registry_json().unwrap(),
            include_str!("../../../ci/log_audit_registry.json")
        );
    }
}
