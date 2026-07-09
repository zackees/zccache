//! Engine phase-profile reporting for `session-stats --json` payloads.

use std::fmt;
use std::process::ExitCode;

use serde_json::Value;

pub(crate) fn cmd_engine_profile(stats_json: &str, json: bool) -> ExitCode {
    match read_engine_profile_report(stats_json) {
        Ok(report) => {
            if json {
                println!("{}", engine_profile_report_json(&report));
            } else {
                print!("{}", render_engine_profile_report(&report));
            }
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("zccache engine-profile: {stats_json}: {err}");
            ExitCode::FAILURE
        }
    }
}

pub(crate) fn read_engine_profile_report(
    stats_json: &str,
) -> Result<EngineProfileReport, EngineProfileError> {
    let raw = std::fs::read_to_string(stats_json).map_err(EngineProfileError::Io)?;
    let value = serde_json::from_str(&raw).map_err(EngineProfileError::Json)?;
    engine_profile_report_from_value(&value)
}

pub(crate) fn engine_profile_report_from_value(
    value: &Value,
) -> Result<EngineProfileReport, EngineProfileError> {
    let phase_profile = value
        .get("phase_profile")
        .and_then(Value::as_object)
        .ok_or(EngineProfileError::MissingPhaseProfile)?;

    let hit_count = u64_field(phase_profile, "hit_count");
    let miss_count = u64_field(phase_profile, "miss_count");
    let hit_total_ns = u64_field(phase_profile, "total_hit_ns");
    let miss_total_ns = u64_field(phase_profile, "total_miss_ns");

    Ok(EngineProfileReport {
        hit_path: PathReport::new(
            hit_count,
            hit_total_ns,
            &[
                ("parse_args", u64_field(phase_profile, "parse_args_ns")),
                (
                    "build_context",
                    u64_field(phase_profile, "build_context_ns"),
                ),
                ("hash_source", u64_field(phase_profile, "hash_source_ns")),
                ("hash_headers", u64_field(phase_profile, "hash_headers_ns")),
                (
                    "depgraph_check",
                    u64_field(phase_profile, "depgraph_check_ns"),
                ),
                (
                    "request_cache_lookup",
                    u64_field(phase_profile, "request_cache_lookup_ns"),
                ),
                (
                    "cross_root_validate",
                    u64_field(phase_profile, "cross_root_validate_ns"),
                ),
                (
                    "artifact_lookup",
                    u64_field(phase_profile, "artifact_lookup_ns"),
                ),
                ("write_output", u64_field(phase_profile, "write_output_ns")),
                ("bookkeeping", u64_field(phase_profile, "bookkeeping_ns")),
            ],
        ),
        miss_path: PathReport::new(
            miss_count,
            miss_total_ns,
            &[
                (
                    "compiler_exec",
                    u64_field(phase_profile, "compiler_exec_ns"),
                ),
                ("include_scan", u64_field(phase_profile, "include_scan_ns")),
                ("hash_all", u64_field(phase_profile, "hash_all_ns")),
                (
                    "artifact_store",
                    u64_field(phase_profile, "artifact_store_ns"),
                ),
            ],
        ),
    })
}

pub(crate) fn engine_profile_report_json(report: &EngineProfileReport) -> Value {
    serde_json::json!({
        "status": "ok",
        "hit_path": path_report_json(&report.hit_path),
        "miss_path": path_report_json(&report.miss_path),
    })
}

pub(crate) fn render_engine_profile_report(report: &EngineProfileReport) -> String {
    let mut out = String::new();
    out.push_str("zccache engine profile\n");
    render_path("hit path", &report.hit_path, &mut out);
    render_path("miss path", &report.miss_path, &mut out);
    out
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct EngineProfileReport {
    pub(crate) hit_path: PathReport,
    pub(crate) miss_path: PathReport,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PathReport {
    pub(crate) count: u64,
    pub(crate) total_ns: u64,
    pub(crate) avg_ns: u64,
    pub(crate) avg_ms: f64,
    pub(crate) dominant_phase: Option<String>,
    pub(crate) phases: Vec<PhaseRow>,
}

impl PathReport {
    fn new(count: u64, total_ns: u64, phases: &[(&'static str, u64)]) -> Self {
        let avg_ns = average_ns(total_ns, count);
        let phases: Vec<PhaseRow> = phases
            .iter()
            .map(|(name, phase_total_ns)| PhaseRow {
                name: (*name).to_string(),
                total_ns: *phase_total_ns,
                avg_ns: average_ns(*phase_total_ns, count),
                avg_ms: ns_to_ms(average_ns(*phase_total_ns, count)),
                percent_of_total: percent(*phase_total_ns, total_ns),
            })
            .collect();
        let dominant_phase = phases
            .iter()
            .filter(|phase| phase.total_ns > 0)
            .max_by_key(|phase| phase.total_ns)
            .map(|phase| phase.name.clone());
        Self {
            count,
            total_ns,
            avg_ns,
            avg_ms: ns_to_ms(avg_ns),
            dominant_phase,
            phases,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PhaseRow {
    pub(crate) name: String,
    pub(crate) total_ns: u64,
    pub(crate) avg_ns: u64,
    pub(crate) avg_ms: f64,
    pub(crate) percent_of_total: f64,
}

#[derive(Debug)]
pub(crate) enum EngineProfileError {
    Io(std::io::Error),
    Json(serde_json::Error),
    MissingPhaseProfile,
}

impl fmt::Display for EngineProfileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(err) => write!(f, "failed to read stats JSON: {err}"),
            Self::Json(err) => write!(f, "failed to parse stats JSON: {err}"),
            Self::MissingPhaseProfile => write!(
                f,
                "missing phase_profile; collect stats with `zccache session-start --stats` \
                 and `zccache session-end --json`"
            ),
        }
    }
}

impl std::error::Error for EngineProfileError {}

fn render_path(label: &str, report: &PathReport, out: &mut String) {
    let dominant = report.dominant_phase.as_deref().unwrap_or("none");
    out.push_str(&format!(
        "  {label}: {} samples, total {} ns ({:.3} ms), avg {} ns ({:.3} ms), dominant {dominant}\n",
        report.count,
        report.total_ns,
        ns_to_ms(report.total_ns),
        report.avg_ns,
        report.avg_ms,
    ));
    for phase in &report.phases {
        out.push_str(&format!(
            "    {:<22} total={} ns avg={} ns avg_ms={:.3} pct={:.1}%\n",
            phase.name, phase.total_ns, phase.avg_ns, phase.avg_ms, phase.percent_of_total,
        ));
    }
}

fn path_report_json(report: &PathReport) -> Value {
    let phases: Vec<Value> = report
        .phases
        .iter()
        .map(|phase| {
            serde_json::json!({
                "name": phase.name,
                "total_ns": phase.total_ns,
                "avg_ns": phase.avg_ns,
                "avg_ms": phase.avg_ms,
                "percent_of_total": phase.percent_of_total,
            })
        })
        .collect();
    serde_json::json!({
        "count": report.count,
        "total_ns": report.total_ns,
        "avg_ns": report.avg_ns,
        "avg_ms": report.avg_ms,
        "dominant_phase": report.dominant_phase,
        "phases": phases,
    })
}

fn u64_field(map: &serde_json::Map<String, Value>, field: &str) -> u64 {
    map.get(field).and_then(Value::as_u64).unwrap_or(0)
}

fn average_ns(total_ns: u64, count: u64) -> u64 {
    if count == 0 {
        0
    } else {
        total_ns / count
    }
}

fn ns_to_ms(ns: u64) -> f64 {
    ns as f64 / 1_000_000.0
}

fn percent(part: u64, total: u64) -> f64 {
    if total == 0 {
        0.0
    } else {
        part as f64 / total as f64 * 100.0
    }
}
