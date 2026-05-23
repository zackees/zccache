//! `zccache analyze` — read a per-session compile journal and roll it up.

use std::process::ExitCode;

use super::util::print_json_value;

/// Filters and sort/limit knobs for `zccache analyze`. Constructed
/// from the matching CLI flags (issue #256). All filters are
/// permissive: when `None`/missing on a journal line the filter
/// passes (so legacy journals that lack `crate_name` still flow
/// through when no `--crate` is supplied).
pub(crate) struct AnalyzeOptions {
    pub json: bool,
    pub session: Option<String>,
    pub crate_name: Option<String>,
    pub outcome: Option<String>,
    pub sort: String,
    pub top: Option<usize>,
}

impl AnalyzeOptions {
    pub(crate) fn sort_mode(&self) -> AnalyzeSort {
        match self.sort.as_str() {
            "misses" => AnalyzeSort::Misses,
            "hits" => AnalyzeSort::Hits,
            _ => AnalyzeSort::WallClock,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AnalyzeSort {
    WallClock,
    Misses,
    Hits,
}

pub(crate) fn cmd_analyze(journal_path: &str, opts: AnalyzeOptions) -> ExitCode {
    let report = match analyze_journal_with(journal_path, &opts) {
        Ok(report) => report,
        Err(AnalyzeError::Read(err)) if matches!(err.kind(), std::io::ErrorKind::NotFound) => {
            // Missing journal: per issue #256 acceptance criteria the
            // analyzer exits 0 with a `(no journal)` message so callers
            // can scaffold the file before the first build runs.
            if opts.json {
                print_json_value(&serde_json::json!({
                    "status": "ok",
                    "schema_version": 1,
                    "journal_path": journal_path,
                    "line_count": 0,
                    "note": "(no journal)",
                }));
            } else {
                println!("zccache analyze: {journal_path}: (no journal)");
            }
            return ExitCode::SUCCESS;
        }
        Err(e) => {
            let message = format_analyze_error(journal_path, &e);
            if opts.json {
                print_json_value(&analyze_error_json(journal_path, &e));
            } else {
                eprintln!("{message}");
            }
            return ExitCode::FAILURE;
        }
    };

    if opts.json {
        print_json_value(&report.to_json(journal_path));
    } else {
        report.print_human_by_crate(journal_path, &opts);
    }
    ExitCode::SUCCESS
}

pub(crate) const ANALYZE_EXPECTED_INPUT: &str =
    "compile journal JSONL from zccache session-start --journal";

#[derive(Debug)]
pub(crate) enum AnalyzeError {
    Read(std::io::Error),
    EmptyInput,
    SessionStatsJson,
    JsonDocument,
    NoJournalEntries { line_count: u64 },
}

impl std::fmt::Display for AnalyzeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Read(err) => write!(f, "failed to read: {err}"),
            Self::EmptyInput => write!(f, "input is empty; expected {ANALYZE_EXPECTED_INPUT}"),
            Self::SessionStatsJson => {
                write!(
                    f,
                    "input is session-stats JSON; expected {ANALYZE_EXPECTED_INPUT}"
                )
            }
            Self::JsonDocument => {
                write!(
                    f,
                    "input is a JSON document, not a JSONL compile journal; expected {ANALYZE_EXPECTED_INPUT}"
                )
            }
            Self::NoJournalEntries { line_count } => {
                write!(
                    f,
                    "no compile journal entries found in {line_count} line(s); expected {ANALYZE_EXPECTED_INPUT}"
                )
            }
        }
    }
}

pub(crate) fn format_analyze_error(journal_path: &str, err: &AnalyzeError) -> String {
    format!("zccache analyze: {journal_path}: {err}")
}

pub(crate) fn analyze_error_json(journal_path: &str, err: &AnalyzeError) -> serde_json::Value {
    serde_json::json!({
        "status": "error",
        "journal_path": journal_path,
        "error": format_analyze_error(journal_path, err),
        "expected_input": ANALYZE_EXPECTED_INPUT,
    })
}

/// Aggregated read-only view of a compile journal.
#[derive(Debug, Default)]
pub(crate) struct AnalyzeReport {
    pub(crate) line_count: u64,
    pub(crate) parsed_count: u64,
    pub(crate) compile_count: u64,
    pub(crate) link_count: u64,
    pub(crate) hit_count: u64,
    pub(crate) miss_count: u64,
    pub(crate) error_count: u64,
    pub(crate) link_hit_count: u64,
    pub(crate) link_miss_count: u64,
    pub(crate) total_latency_ns: u128,
    /// Per-output-extension hit/miss/total-ms counters.
    pub(crate) by_extension: std::collections::BTreeMap<String, ExtensionBucket>,
    /// Per-tool total latency (basename of `compiler`).
    pub(crate) by_tool_total_ns: std::collections::BTreeMap<String, u128>,
    /// Hit counts per tool — useful to see which tools dominate the workload.
    pub(crate) by_tool_calls: std::collections::BTreeMap<String, u64>,
    /// Sorted slowest entries (any outcome). Bounded at 20.
    pub(crate) slowest_entries: Vec<SlowestEntry>,
    /// Per-crate-name miss counts. Bounded by HashMap during accumulation,
    /// surfaced as a sorted top-N in the report.
    pub(crate) miss_crate_counts: std::collections::HashMap<String, u64>,
    /// Issue #256: per-crate hit/miss/wall-clock rollup used by the
    /// default human-readable table. Keyed by crate_name (or
    /// `<unknown>` when the journal line lacks one).
    pub(crate) by_crate: std::collections::HashMap<String, CrateBucket>,
}

#[derive(Debug, Default, Clone)]
pub(crate) struct CrateBucket {
    pub(crate) hits: u64,
    pub(crate) misses: u64,
    pub(crate) errors: u64,
    pub(crate) total_ns: u128,
}

#[derive(Debug, Default)]
pub(crate) struct ExtensionBucket {
    pub(crate) hits: u64,
    pub(crate) misses: u64,
    pub(crate) total_ns: u128,
}

#[derive(Debug, Clone)]
pub(crate) struct SlowestEntry {
    pub(crate) outcome: String,
    pub(crate) crate_name: Option<String>,
    pub(crate) crate_type: Option<String>,
    pub(crate) tool: String,
    pub(crate) latency_ns: u128,
}

#[derive(Debug, Clone)]
pub(crate) struct TopMissCrate {
    pub(crate) crate_name: String,
    pub(crate) misses: u64,
}

impl AnalyzeReport {
    pub(crate) fn ingest(&mut self, line: &serde_json::Value) {
        self.parsed_count += 1;
        let outcome = line
            .get("outcome")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");
        let latency_ns = line
            .get("latency_ns")
            .and_then(|v| v.as_u64())
            .map(u128::from)
            .or_else(|| {
                line.get("latency_ns")
                    .and_then(|v| v.as_f64())
                    .map(|f| f as u128)
            })
            .unwrap_or(0);
        self.total_latency_ns = self.total_latency_ns.saturating_add(latency_ns);

        let args = line
            .get("args")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .collect::<Vec<String>>()
            })
            .unwrap_or_default();
        let compiler = line.get("compiler").and_then(|v| v.as_str()).unwrap_or("");
        let tool = tool_basename(compiler);
        let crate_name = extract_flag_value(&args, "--crate-name");
        let crate_type = extract_flag_value(&args, "--crate-type");
        let extension_bucket = classify_extension(outcome, &crate_type);

        // Issue #256: roll every compile entry up into a per-crate
        // bucket so the human-readable table can group by crate.
        // Falls back to "<unknown>" so journal lines lacking a
        // crate name still appear in the rollup.
        let crate_key = crate_name
            .clone()
            .unwrap_or_else(|| "<unknown>".to_string());
        match outcome {
            "hit" => {
                self.compile_count += 1;
                self.hit_count += 1;
                let bucket = self.by_extension.entry(extension_bucket).or_default();
                bucket.hits += 1;
                bucket.total_ns = bucket.total_ns.saturating_add(latency_ns);
                let cb = self.by_crate.entry(crate_key).or_default();
                cb.hits += 1;
                cb.total_ns = cb.total_ns.saturating_add(latency_ns);
            }
            "miss" => {
                self.compile_count += 1;
                self.miss_count += 1;
                let bucket = self.by_extension.entry(extension_bucket).or_default();
                bucket.misses += 1;
                bucket.total_ns = bucket.total_ns.saturating_add(latency_ns);
                if let Some(name) = &crate_name {
                    *self.miss_crate_counts.entry(name.clone()).or_default() += 1;
                }
                let cb = self.by_crate.entry(crate_key).or_default();
                cb.misses += 1;
                cb.total_ns = cb.total_ns.saturating_add(latency_ns);
            }
            "error" => {
                self.compile_count += 1;
                self.error_count += 1;
                let cb = self.by_crate.entry(crate_key).or_default();
                cb.errors += 1;
                cb.total_ns = cb.total_ns.saturating_add(latency_ns);
            }
            "link_hit" => {
                self.link_count += 1;
                self.link_hit_count += 1;
            }
            "link_miss" => {
                self.link_count += 1;
                self.link_miss_count += 1;
            }
            _ => {}
        }

        *self.by_tool_calls.entry(tool.clone()).or_default() += 1;
        let tool_entry = self.by_tool_total_ns.entry(tool.clone()).or_default();
        *tool_entry = tool_entry.saturating_add(latency_ns);

        let entry = SlowestEntry {
            outcome: outcome.to_string(),
            crate_name,
            crate_type,
            tool,
            latency_ns,
        };
        // Maintain a top-20 sorted descending by latency.
        if self.slowest_entries.len() < 20 {
            self.slowest_entries.push(entry);
            self.slowest_entries
                .sort_by(|a, b| b.latency_ns.cmp(&a.latency_ns));
        } else if latency_ns
            > self
                .slowest_entries
                .last()
                .map(|e| e.latency_ns)
                .unwrap_or(0)
        {
            self.slowest_entries.pop();
            self.slowest_entries.push(entry);
            self.slowest_entries
                .sort_by(|a, b| b.latency_ns.cmp(&a.latency_ns));
        }
    }

    pub(crate) fn hit_rate(&self) -> Option<f64> {
        let total = self.hit_count + self.miss_count;
        if total == 0 {
            None
        } else {
            Some(self.hit_count as f64 / total as f64)
        }
    }

    pub(crate) fn top_miss_crates(&self, limit: usize) -> Vec<TopMissCrate> {
        let mut v: Vec<TopMissCrate> = self
            .miss_crate_counts
            .iter()
            .map(|(k, v)| TopMissCrate {
                crate_name: k.clone(),
                misses: *v,
            })
            .collect();
        v.sort_by(|a, b| {
            b.misses
                .cmp(&a.misses)
                .then_with(|| a.crate_name.cmp(&b.crate_name))
        });
        v.truncate(limit);
        v
    }

    pub(crate) fn to_json(&self, journal_path: &str) -> serde_json::Value {
        let by_extension: serde_json::Map<String, serde_json::Value> = self
            .by_extension
            .iter()
            .map(|(ext, bucket)| {
                (
                    ext.clone(),
                    serde_json::json!({
                        "hits": bucket.hits,
                        "misses": bucket.misses,
                        "total_ms": bucket.total_ns / 1_000_000,
                    }),
                )
            })
            .collect();
        let by_tool_total_ms: serde_json::Map<String, serde_json::Value> = self
            .by_tool_total_ns
            .iter()
            .map(|(tool, ns)| {
                (
                    tool.clone(),
                    serde_json::Value::from((ns / 1_000_000) as u64),
                )
            })
            .collect();
        let by_tool_calls: serde_json::Map<String, serde_json::Value> = self
            .by_tool_calls
            .iter()
            .map(|(tool, calls)| (tool.clone(), serde_json::Value::from(*calls)))
            .collect();
        let slowest = self
            .slowest_entries
            .iter()
            .map(|e| {
                serde_json::json!({
                    "outcome": e.outcome,
                    "crate_name": e.crate_name,
                    "crate_type": e.crate_type,
                    "tool": e.tool,
                    "ms": e.latency_ns / 1_000_000,
                })
            })
            .collect::<Vec<_>>();
        let top_miss_crates = self
            .top_miss_crates(10)
            .into_iter()
            .map(|c| {
                serde_json::json!({
                    "crate_name": c.crate_name,
                    "misses": c.misses,
                })
            })
            .collect::<Vec<_>>();
        serde_json::json!({
            "status": "ok",
            "schema_version": 1,
            "journal_path": journal_path,
            "line_count": self.line_count,
            "parsed_count": self.parsed_count,
            "compile_count": self.compile_count,
            "link_count": self.link_count,
            "hit_count": self.hit_count,
            "miss_count": self.miss_count,
            "error_count": self.error_count,
            "link_hit_count": self.link_hit_count,
            "link_miss_count": self.link_miss_count,
            "hit_rate": self.hit_rate(),
            "total_latency_ms": (self.total_latency_ns / 1_000_000) as u64,
            "by_extension": by_extension,
            "by_tool_total_ms": by_tool_total_ms,
            "by_tool_calls": by_tool_calls,
            "top_slowest": slowest,
            "top_miss_crates": top_miss_crates,
            "by_crate": self.by_crate_json(),
        })
    }

    fn by_crate_json(&self) -> serde_json::Value {
        let mut rows: Vec<(&String, &CrateBucket)> = self.by_crate.iter().collect();
        rows.sort_by(|a, b| b.1.total_ns.cmp(&a.1.total_ns).then_with(|| a.0.cmp(b.0)));
        serde_json::Value::Array(
            rows.into_iter()
                .map(|(name, b)| {
                    serde_json::json!({
                        "crate_name": name,
                        "hits": b.hits,
                        "misses": b.misses,
                        "errors": b.errors,
                        "total_ms": (b.total_ns / 1_000_000) as u64,
                    })
                })
                .collect(),
        )
    }

    /// Legacy human printer, kept for reference and reused by
    /// older tests. The default `cmd_analyze` path uses
    /// [`Self::print_human_by_crate`] instead.
    #[allow(dead_code)]
    fn print_human(&self, journal_path: &str) {
        println!("zccache analyze: {journal_path}");
        println!(
            "  lines: {} parsed; compiles: {} (hits {} / misses {} / errors {}); links: {} (hits {} / misses {})",
            self.parsed_count,
            self.compile_count,
            self.hit_count,
            self.miss_count,
            self.error_count,
            self.link_count,
            self.link_hit_count,
            self.link_miss_count,
        );
        if let Some(rate) = self.hit_rate() {
            println!("  hit rate: {:.1}%", rate * 100.0);
        } else {
            println!("  hit rate: n/a");
        }
        println!(
            "  total wall-clock: {} ms",
            self.total_latency_ns / 1_000_000
        );
        if !self.by_extension.is_empty() {
            println!();
            println!("  by extension:");
            for (ext, bucket) in &self.by_extension {
                println!(
                    "    {ext:<14}  hits={:>6}  misses={:>6}  ms={}",
                    bucket.hits,
                    bucket.misses,
                    bucket.total_ns / 1_000_000
                );
            }
        }
        if !self.by_tool_total_ns.is_empty() {
            println!();
            println!("  by tool (wall-clock ms):");
            let mut sorted: Vec<_> = self.by_tool_total_ns.iter().collect();
            sorted.sort_by(|a, b| b.1.cmp(a.1));
            for (tool, ns) in sorted.iter().take(10) {
                let calls = self.by_tool_calls.get(*tool).copied().unwrap_or(0);
                println!("    {tool:<24}  ms={:>9}  calls={calls}", *ns / 1_000_000);
            }
        }
        let top_miss = self.top_miss_crates(10);
        if !top_miss.is_empty() {
            println!();
            println!("  top miss crates:");
            for c in &top_miss {
                println!("    {:<32}  misses={}", c.crate_name, c.misses);
            }
        }
        if !self.slowest_entries.is_empty() {
            println!();
            println!("  slowest entries (top {}):", self.slowest_entries.len());
            for e in &self.slowest_entries {
                let crate_label = e
                    .crate_name
                    .as_deref()
                    .unwrap_or_else(|| e.crate_type.as_deref().unwrap_or("?"));
                println!(
                    "    {:<10} {:<24}  ms={}  tool={}",
                    e.outcome,
                    crate_label,
                    e.latency_ns / 1_000_000,
                    e.tool
                );
            }
        }
    }

    /// Issue #256: default human-readable rollup grouped by crate
    /// name and sorted by the AnalyzeOptions sort knob. Empty input
    /// after filtering prints a header line plus an empty-table
    /// marker so scripts can distinguish "no entries matched" from
    /// "no journal file" (the latter is short-circuited by cmd_analyze).
    pub(crate) fn print_human_by_crate(&self, journal_path: &str, opts: &AnalyzeOptions) {
        println!("zccache analyze: {journal_path}");
        println!(
            "  lines: {} parsed; compiles: {} (hits {} / misses {} / errors {}); links: {} (hits {} / misses {})",
            self.parsed_count,
            self.compile_count,
            self.hit_count,
            self.miss_count,
            self.error_count,
            self.link_count,
            self.link_hit_count,
            self.link_miss_count,
        );
        if let Some(rate) = self.hit_rate() {
            println!("  hit rate: {:.1}%", rate * 100.0);
        }
        println!(
            "  total wall-clock: {} ms",
            self.total_latency_ns / 1_000_000
        );

        let rows = self.crate_rows(opts);
        if rows.is_empty() {
            println!("  (no rows after filters)");
            return;
        }
        println!();
        println!("  by crate (sorted by {}):", opts.sort);
        println!(
            "    {col:<32}  hits={h:>6}  misses={m:>6}  errors={e:>4}  ms",
            col = "crate",
            h = "h",
            m = "m",
            e = "e",
        );
        for row in &rows {
            println!(
                "    {:<32}  hits={:>6}  misses={:>6}  errors={:>4}  ms={}",
                row.crate_name,
                row.hits,
                row.misses,
                row.errors,
                row.total_ns / 1_000_000
            );
        }
    }

    /// Issue #256: build a sorted, optionally-truncated per-crate view
    /// over `self.by_crate`. Splitting this off the printer keeps the
    /// sorting logic testable without a stdout fixture.
    pub(crate) fn crate_rows(&self, opts: &AnalyzeOptions) -> Vec<CrateRow> {
        let mut rows: Vec<CrateRow> = self
            .by_crate
            .iter()
            .map(|(name, b)| CrateRow {
                crate_name: name.clone(),
                hits: b.hits,
                misses: b.misses,
                errors: b.errors,
                total_ns: b.total_ns,
            })
            .collect();
        match opts.sort_mode() {
            AnalyzeSort::WallClock => rows.sort_by(|a, b| {
                b.total_ns
                    .cmp(&a.total_ns)
                    .then_with(|| a.crate_name.cmp(&b.crate_name))
            }),
            AnalyzeSort::Misses => rows.sort_by(|a, b| {
                b.misses
                    .cmp(&a.misses)
                    .then_with(|| a.crate_name.cmp(&b.crate_name))
            }),
            AnalyzeSort::Hits => rows.sort_by(|a, b| {
                b.hits
                    .cmp(&a.hits)
                    .then_with(|| a.crate_name.cmp(&b.crate_name))
            }),
        }
        if let Some(n) = opts.top {
            rows.truncate(n);
        }
        rows
    }
}

/// One row of the per-crate rollup. Public-within-crate so tests can
/// pin sort order without going through stdout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CrateRow {
    pub crate_name: String,
    pub hits: u64,
    pub misses: u64,
    pub errors: u64,
    pub total_ns: u128,
}

/// Issue #256: streaming analyze with optional filters. Skips
/// journal lines that fail the `session`/`crate`/`outcome` filter
/// without counting them in the line tallies. Truncated or malformed
/// lines emit a single stderr warning per occurrence and are skipped.
pub(crate) fn analyze_journal_with(
    journal_path: &str,
    opts: &AnalyzeOptions,
) -> Result<AnalyzeReport, AnalyzeError> {
    let content = std::fs::read_to_string(journal_path).map_err(AnalyzeError::Read)?;
    let mut report = AnalyzeReport::default();
    let mut malformed = 0u64;
    for line in content.lines() {
        report.line_count += 1;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        match serde_json::from_str::<serde_json::Value>(trimmed) {
            Ok(value) => {
                if !is_compile_journal_entry(&value) {
                    continue;
                }
                if !analyze_passes_filters(&value, opts) {
                    continue;
                }
                report.ingest(&value);
            }
            Err(_) => {
                malformed += 1;
            }
        }
    }
    if malformed > 0 {
        eprintln!("zccache analyze: skipped {malformed} malformed line(s)");
    }
    if report.parsed_count == 0 {
        // When filters are active, an empty result is a legitimate
        // zero-row report rather than an input-classification error.
        let any_filter =
            opts.session.is_some() || opts.crate_name.is_some() || opts.outcome.is_some();
        if any_filter {
            return Ok(report);
        }
        return Err(classify_analyze_input_without_entries(
            content.trim(),
            report.line_count,
        ));
    }
    Ok(report)
}

/// Apply the analyze filter flags to a parsed journal value.
/// Filters are conjunctive; missing values on the line are
/// treated as non-matches so a `--crate foo` filter never
/// matches a record without a `crate_name`.
fn analyze_passes_filters(value: &serde_json::Value, opts: &AnalyzeOptions) -> bool {
    if let Some(want) = &opts.session {
        let got = value.get("session_id").and_then(|v| v.as_str());
        if got != Some(want.as_str()) {
            return false;
        }
    }
    if let Some(want) = &opts.crate_name {
        let direct = value
            .get("crate_name")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        let derived = direct.or_else(|| {
            let args: Vec<String> = value
                .get("args")
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            extract_flag_value(&args, "--crate-name")
        });
        if derived.as_deref() != Some(want.as_str()) {
            return false;
        }
    }
    if let Some(want) = &opts.outcome {
        let got = value.get("outcome").and_then(|v| v.as_str()).unwrap_or("");
        let matches_outcome = match want.as_str() {
            "hit" => got == "hit" || got == "link_hit",
            "miss" => got == "miss" || got == "link_miss",
            "non-cacheable" => got == "error",
            other => got == other,
        };
        if !matches_outcome {
            return false;
        }
    }
    true
}

/// Legacy unfiltered analyze entry point. Production callers use
/// [`analyze_journal_with`]; the tests pinned to this function pre-date
/// the filter flags and keep the simpler signature.
#[allow(dead_code)]
pub(crate) fn analyze_journal(journal_path: &str) -> Result<AnalyzeReport, AnalyzeError> {
    let content = std::fs::read_to_string(journal_path).map_err(AnalyzeError::Read)?;
    let mut report = AnalyzeReport::default();
    for line in content.lines() {
        report.line_count += 1;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        // Permissive parse: skip malformed lines rather than fail the run.
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed) {
            if !is_compile_journal_entry(&value) {
                continue;
            }
            report.ingest(&value);
        }
    }
    if report.parsed_count == 0 {
        return Err(classify_analyze_input_without_entries(
            content.trim(),
            report.line_count,
        ));
    }
    Ok(report)
}

fn classify_analyze_input_without_entries(trimmed: &str, line_count: u64) -> AnalyzeError {
    if trimmed.is_empty() {
        return AnalyzeError::EmptyInput;
    }
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed) {
        if is_session_stats_json(&value) {
            return AnalyzeError::SessionStatsJson;
        }
        return AnalyzeError::JsonDocument;
    }
    AnalyzeError::NoJournalEntries { line_count }
}

fn is_compile_journal_entry(value: &serde_json::Value) -> bool {
    let outcome = value.get("outcome").and_then(|v| v.as_str());
    let has_known_outcome = matches!(
        outcome,
        Some("hit" | "miss" | "error" | "link_hit" | "link_miss")
    );
    has_known_outcome
        && value.get("compiler").and_then(|v| v.as_str()).is_some()
        && value.get("args").and_then(|v| v.as_array()).is_some()
}

fn is_session_stats_json(value: &serde_json::Value) -> bool {
    let Some(object) = value.as_object() else {
        return false;
    };
    object.contains_key("compilations")
        && object.contains_key("hits")
        && object.contains_key("misses")
        && object.contains_key("hit_rate")
}

pub(crate) fn tool_basename(compiler: &str) -> String {
    // Split on both separators so Windows-style paths round-trip on Unix
    // (where std::path doesn't recognize `\` as a component boundary).
    let last_component = compiler.rsplit(['/', '\\']).next().unwrap_or(compiler);
    let stem = last_component
        .rsplit_once('.')
        .map(|(stem, _ext)| stem)
        .filter(|s| !s.is_empty())
        .unwrap_or(last_component);
    stem.to_string()
}

pub(crate) fn extract_flag_value(args: &[String], flag: &str) -> Option<String> {
    let prefix = format!("{flag}=");
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if arg == flag {
            return iter.next().cloned();
        }
        if let Some(rest) = arg.strip_prefix(&prefix) {
            return Some(rest.to_string());
        }
    }
    None
}

fn classify_extension(outcome: &str, crate_type: &Option<String>) -> String {
    // Links are not parameterized by --crate-type in the rustc sense; bucket
    // them separately so the rollup can distinguish linker work from compile
    // work even when the per-compile classification is unknown.
    if outcome == "link_hit" || outcome == "link_miss" {
        return "link".to_string();
    }
    match crate_type.as_deref() {
        Some("bin") => "bin".to_string(),
        Some("lib") | Some("rlib") => "rlib".to_string(),
        Some("dylib") => "dylib".to_string(),
        Some("cdylib") => "cdylib".to_string(),
        Some("staticlib") => "staticlib".to_string(),
        Some("proc-macro") => "proc-macro".to_string(),
        Some(other) => other.to_string(),
        None => "unknown".to_string(),
    }
}
