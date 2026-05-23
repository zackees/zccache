//! Compile-journal `analyze` aggregation, JSON shape, error classes,
//! filtering, per-crate rollup, and sort tests. The largest test domain;
//! everything here drives `super::super::analyze::*` directly with
//! synthesized journal lines.

use super::super::analyze::{
    analyze_error_json, analyze_journal, analyze_journal_with, extract_flag_value, AnalyzeError,
    AnalyzeOptions, AnalyzeReport, AnalyzeSort, ANALYZE_EXPECTED_INPUT,
};
use super::super::session::session_stats_json;

fn make_journal_line(
    outcome: &str,
    compiler: &str,
    crate_name: &str,
    crate_type: &str,
    latency_ns: u128,
) -> serde_json::Value {
    serde_json::json!({
        "ts": "2026-05-14T18:00:00Z",
        "outcome": outcome,
        "compiler": compiler,
        "args": [
            "--crate-name", crate_name,
            "--crate-type", crate_type,
            "--edition=2021",
        ],
        "cwd": "/repo",
        "exit_code": 0,
        "session_id": null,
        "latency_ns": latency_ns as u64,
    })
}

#[test]
fn analyze_aggregates_outcomes_by_extension_and_tool() {
    let mut report = AnalyzeReport::default();
    report.ingest(&make_journal_line(
        "hit",
        "/rustup/rustc",
        "soldr_cli",
        "bin",
        5_000_000,
    ));
    report.ingest(&make_journal_line(
        "miss",
        "/rustup/rustc",
        "soldr_cli",
        "bin",
        120_000_000,
    ));
    report.ingest(&make_journal_line(
        "hit",
        "/rustup/rustc",
        "serde",
        "lib",
        12_000_000,
    ));
    report.ingest(&make_journal_line(
        "miss",
        "/rustup/clippy-driver",
        "lints",
        "lib",
        45_000_000,
    ));

    assert_eq!(report.compile_count, 4);
    assert_eq!(report.hit_count, 2);
    assert_eq!(report.miss_count, 2);
    assert_eq!(report.hit_rate(), Some(0.5));

    let bin = report.by_extension.get("bin").expect("bin bucket");
    assert_eq!(bin.hits, 1);
    assert_eq!(bin.misses, 1);

    let rlib = report.by_extension.get("rlib").expect("rlib bucket");
    assert_eq!(rlib.hits, 1);
    assert_eq!(rlib.misses, 1);

    let rustc_ms = report.by_tool_total_ns.get("rustc").copied().unwrap();
    assert!(rustc_ms > 0);
    let clippy_calls = report.by_tool_calls.get("clippy-driver").copied().unwrap();
    assert_eq!(clippy_calls, 1);

    let top = report.top_miss_crates(5);
    assert_eq!(top.len(), 2);
    let names: Vec<&str> = top.iter().map(|c| c.crate_name.as_str()).collect();
    assert!(names.contains(&"soldr_cli"));
    assert!(names.contains(&"lints"));
}

#[test]
fn analyze_buckets_links_separately() {
    let mut report = AnalyzeReport::default();
    let mut entry = make_journal_line("link_hit", "/tools/ld", "soldr_cli", "bin", 9_000_000);
    // Strip --crate-type since linker invocations don't usually carry one.
    entry["args"] = serde_json::json!([]);
    report.ingest(&entry);
    let mut miss = make_journal_line("link_miss", "/tools/ld", "soldr_cli", "bin", 22_000_000);
    miss["args"] = serde_json::json!([]);
    report.ingest(&miss);

    assert_eq!(report.link_count, 2);
    assert_eq!(report.link_hit_count, 1);
    assert_eq!(report.link_miss_count, 1);

    let link_bucket = report.by_extension.get("link");
    // Link entries don't carry crate_type but still get a bucket name via
    // classify_extension; verify it lives under "link" when reached via
    // a hit/miss outcome. For pure link_hit/link_miss outcomes we do not
    // add to by_extension; assert that's the documented behavior.
    assert!(link_bucket.is_none());
}

#[test]
fn analyze_top_slowest_caps_at_twenty() {
    let mut report = AnalyzeReport::default();
    for i in 0..30u128 {
        report.ingest(&make_journal_line(
            "miss",
            "/rustup/rustc",
            &format!("crate{i}"),
            "lib",
            i * 1_000_000,
        ));
    }
    assert_eq!(report.slowest_entries.len(), 20);
    let first = report.slowest_entries.first().unwrap();
    let last = report.slowest_entries.last().unwrap();
    assert!(first.latency_ns >= last.latency_ns);
    // The slowest miss should be 29ms; the cutoff should be 10ms.
    assert_eq!(first.latency_ns, 29_000_000);
    assert_eq!(last.latency_ns, 10_000_000);
}

#[test]
fn analyze_to_json_has_stable_top_level_keys() {
    let mut report = AnalyzeReport::default();
    report.ingest(&make_journal_line(
        "hit",
        "/rustup/rustc",
        "demo",
        "bin",
        1_000_000,
    ));
    let v = report.to_json("/tmp/journal.jsonl");
    assert_eq!(v["status"], "ok");
    assert_eq!(v["schema_version"], 1);
    assert_eq!(v["journal_path"], "/tmp/journal.jsonl");
    assert!(v["hit_rate"].is_number() || v["hit_rate"].is_null());
    assert!(v["by_extension"].is_object());
    assert!(v["by_tool_total_ms"].is_object());
    assert!(v["top_slowest"].is_array());
    assert!(v["top_miss_crates"].is_array());
}

#[test]
fn extract_flag_value_handles_space_and_equals_forms() {
    let args = vec![
        "--crate-name".to_string(),
        "demo".to_string(),
        "--edition=2021".to_string(),
    ];
    assert_eq!(
        extract_flag_value(&args, "--crate-name"),
        Some("demo".to_string())
    );
    assert_eq!(
        extract_flag_value(&args, "--edition"),
        Some("2021".to_string())
    );
    assert_eq!(extract_flag_value(&args, "--crate-type"), None);
}

// Note: tool_basename's behavior is exercised through
// analyze_aggregates_outcomes_by_extension_and_tool above (which feeds
// it `/rustup/rustc` and `/rustup/clippy-driver` paths and asserts the
// by-tool rollup keys come out as "rustc" / "clippy-driver"). A direct
// test was removed after a Linux/macOS CI cache-poisoning incident
// kept replaying a stale assertion — the function logic itself is
// already covered.

#[test]
fn analyze_journal_reads_jsonl_file() {
    use std::io::Write;
    let tmp = tempfile::tempdir().expect("tempdir");
    let path = tmp.path().join("session.jsonl");
    let mut f = std::fs::File::create(&path).unwrap();
    let lines = [
        make_journal_line("hit", "/rustup/rustc", "a", "lib", 1_000_000),
        make_journal_line("miss", "/rustup/rustc", "b", "bin", 2_000_000),
    ];
    for line in &lines {
        writeln!(f, "{}", serde_json::to_string(line).unwrap()).unwrap();
    }
    drop(f);
    let report = analyze_journal(path.to_str().unwrap()).expect("analyze");
    assert_eq!(report.line_count, 2);
    assert_eq!(report.parsed_count, 2);
    assert_eq!(report.hit_count, 1);
    assert_eq!(report.miss_count, 1);
}

#[test]
fn analyze_journal_missing_file_has_structured_error_hint() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let path = tmp.path().join("missing.jsonl");
    let path_str = path.to_str().unwrap();

    let err = analyze_journal(path_str).expect_err("missing file should fail");
    match &err {
        AnalyzeError::Read(_) => {}
        other => panic!("expected read error, got: {other:?}"),
    }

    let json = analyze_error_json(path_str, &err);
    assert_eq!(json["status"], "error");
    assert_eq!(json["journal_path"].as_str().unwrap(), path_str);
    assert_eq!(
        json["expected_input"].as_str().unwrap(),
        ANALYZE_EXPECTED_INPUT
    );
    assert!(json["error"].as_str().unwrap().contains("failed to read"));
}

#[test]
fn analyze_journal_rejects_session_stats_json() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let path = tmp.path().join("last-session-stats.json");
    let stats = crate::protocol::SessionStats {
        duration_ms: 1000,
        compilations: 10,
        hits: 7,
        misses: 3,
        non_cacheable: 2,
        errors: 1,
        time_saved_ms: 250,
        unique_sources: 8,
        bytes_read: 1024,
        bytes_written: 2048,
        phase_profile: None,
    };
    let stats_json = session_stats_json("session-123", &stats);
    std::fs::write(&path, serde_json::to_string_pretty(&stats_json).unwrap()).unwrap();

    let err = analyze_journal(path.to_str().unwrap()).expect_err("stats JSON should fail");
    match &err {
        AnalyzeError::SessionStatsJson => {}
        other => panic!("expected session-stats JSON error, got: {other:?}"),
    }
    let rendered = err.to_string();
    assert!(rendered.contains("session-stats JSON"));
    assert!(rendered.contains(ANALYZE_EXPECTED_INPUT));
}

#[test]
fn analyze_journal_rejects_empty_file() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let path = tmp.path().join("empty.jsonl");
    std::fs::write(&path, "").unwrap();

    let err = analyze_journal(path.to_str().unwrap()).expect_err("empty file should fail");
    match &err {
        AnalyzeError::EmptyInput => {}
        other => panic!("expected empty input error, got: {other:?}"),
    }
    assert!(err.to_string().contains(ANALYZE_EXPECTED_INPUT));
}

#[test]
fn analyze_journal_rejects_file_without_journal_entries() {
    use std::io::Write;
    let tmp = tempfile::tempdir().expect("tempdir");
    let path = tmp.path().join("not-a-journal.jsonl");
    let mut f = std::fs::File::create(&path).unwrap();
    writeln!(f).unwrap();
    writeln!(f, "not json").unwrap();
    writeln!(f, "{{}}").unwrap();
    drop(f);

    let err = analyze_journal(path.to_str().unwrap()).expect_err("no journal entries should fail");
    match &err {
        AnalyzeError::NoJournalEntries { line_count } => assert_eq!(*line_count, 3),
        other => panic!("expected no journal entries error, got: {other:?}"),
    }
    assert!(err.to_string().contains("no compile journal entries"));
    assert!(err.to_string().contains(ANALYZE_EXPECTED_INPUT));
}

#[test]
fn analyze_journal_skips_blank_and_malformed_lines() {
    use std::io::Write;
    let tmp = tempfile::tempdir().expect("tempdir");
    let path = tmp.path().join("messy.jsonl");
    let mut f = std::fs::File::create(&path).unwrap();
    writeln!(f).unwrap();
    writeln!(f, "not json").unwrap();
    writeln!(
        f,
        "{}",
        serde_json::to_string(&make_journal_line(
            "hit",
            "/rustup/rustc",
            "ok",
            "lib",
            500_000
        ))
        .unwrap()
    )
    .unwrap();
    drop(f);
    let report = analyze_journal(path.to_str().unwrap()).expect("analyze");
    // 3 lines read; 2 non-blank; only 1 successfully parsed.
    assert_eq!(report.line_count, 3);
    assert_eq!(report.parsed_count, 1);
    assert_eq!(report.hit_count, 1);
}

// Issue #256 -- AnalyzeOptions filtering, per-crate rollup, sort.

fn make_journal_line_full(
    outcome: &str,
    compiler: &str,
    crate_name: &str,
    crate_type: &str,
    latency_ns: u128,
    session_id: Option<&str>,
) -> serde_json::Value {
    serde_json::json!({
        "ts": "2026-05-14T18:00:00Z",
        "outcome": outcome,
        "compiler": compiler,
        "args": [
            "--crate-name", crate_name,
            "--crate-type", crate_type,
            "--edition=2021",
        ],
        "cwd": "/repo",
        "exit_code": 0,
        "session_id": session_id,
        "latency_ns": latency_ns as u64,
    })
}

fn write_fixture_journal(entries: &[serde_json::Value]) -> (tempfile::TempDir, std::path::PathBuf) {
    use std::io::Write;
    let tmp = tempfile::tempdir().expect("tempdir");
    let path = tmp.path().join("fixture.jsonl");
    let mut f = std::fs::File::create(&path).unwrap();
    for e in entries {
        writeln!(f, "{}", serde_json::to_string(e).unwrap()).unwrap();
    }
    drop(f);
    (tmp, path)
}

fn default_opts() -> AnalyzeOptions {
    AnalyzeOptions {
        json: false,
        session: None,
        crate_name: None,
        outcome: None,
        sort: "wall-clock".into(),
        top: None,
    }
}

#[test]
fn analyze_by_crate_default_sorts_by_wall_clock_desc() {
    let entries = vec![
        make_journal_line_full("hit", "/rustc", "alpha", "lib", 200_000_000, None),
        make_journal_line_full("miss", "/rustc", "alpha", "lib", 100_000_000, None),
        make_journal_line_full("hit", "/rustc", "beta", "bin", 500_000_000, None),
    ];
    let (_tmp, path) = write_fixture_journal(&entries);
    let report = analyze_journal_with(path.to_str().unwrap(), &default_opts()).expect("ok");
    let rows = report.crate_rows(&default_opts());
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].crate_name, "beta");
    assert_eq!(rows[1].crate_name, "alpha");
    assert_eq!(rows[0].total_ns, 500_000_000);
    assert_eq!(rows[1].total_ns, 300_000_000);
}

#[test]
fn analyze_sort_misses_orders_by_miss_count() {
    let entries = vec![
        make_journal_line_full("miss", "/rustc", "a", "lib", 10, None),
        make_journal_line_full("miss", "/rustc", "a", "lib", 10, None),
        make_journal_line_full("miss", "/rustc", "b", "lib", 10, None),
    ];
    let (_tmp, path) = write_fixture_journal(&entries);
    let mut opts = default_opts();
    opts.sort = "misses".into();
    let report = analyze_journal_with(path.to_str().unwrap(), &opts).expect("ok");
    let rows = report.crate_rows(&opts);
    assert_eq!(rows[0].crate_name, "a");
    assert_eq!(rows[0].misses, 2);
    assert_eq!(rows[1].crate_name, "b");
    assert_eq!(rows[1].misses, 1);
}

#[test]
fn analyze_top_truncates_rows() {
    let mut entries = Vec::new();
    for i in 0..5 {
        entries.push(make_journal_line_full(
            "hit",
            "/rustc",
            &format!("c{i}"),
            "lib",
            100 * (i as u128 + 1),
            None,
        ));
    }
    let (_tmp, path) = write_fixture_journal(&entries);
    let mut opts = default_opts();
    opts.top = Some(2);
    let report = analyze_journal_with(path.to_str().unwrap(), &opts).expect("ok");
    let rows = report.crate_rows(&opts);
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].crate_name, "c4");
    assert_eq!(rows[1].crate_name, "c3");
}

#[test]
fn analyze_session_filter_excludes_other_sessions() {
    let entries = vec![
        make_journal_line_full("hit", "/rustc", "a", "lib", 1, Some("s1")),
        make_journal_line_full("hit", "/rustc", "b", "lib", 1, Some("s2")),
    ];
    let (_tmp, path) = write_fixture_journal(&entries);
    let mut opts = default_opts();
    opts.session = Some("s1".into());
    let report = analyze_journal_with(path.to_str().unwrap(), &opts).expect("ok");
    let rows = report.crate_rows(&opts);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].crate_name, "a");
}

#[test]
fn analyze_crate_filter_matches_by_crate_name_arg() {
    let entries = vec![
        make_journal_line_full("hit", "/rustc", "needle", "lib", 1, None),
        make_journal_line_full("hit", "/rustc", "other", "lib", 1, None),
    ];
    let (_tmp, path) = write_fixture_journal(&entries);
    let mut opts = default_opts();
    opts.crate_name = Some("needle".into());
    let report = analyze_journal_with(path.to_str().unwrap(), &opts).expect("ok");
    assert_eq!(report.hit_count, 1);
    assert_eq!(report.parsed_count, 1);
}

#[test]
fn analyze_outcome_filter_miss_includes_link_miss() {
    let entries = vec![
        make_journal_line_full("hit", "/rustc", "a", "lib", 1, None),
        make_journal_line_full("miss", "/rustc", "a", "lib", 1, None),
        make_journal_line_full("link_miss", "/lld", "a", "lib", 1, None),
    ];
    let (_tmp, path) = write_fixture_journal(&entries);
    let mut opts = default_opts();
    opts.outcome = Some("miss".into());
    let report = analyze_journal_with(path.to_str().unwrap(), &opts).expect("ok");
    // Both `miss` and `link_miss` flow through; the `hit` is excluded.
    assert_eq!(report.miss_count, 1);
    assert_eq!(report.link_miss_count, 1);
    assert_eq!(report.hit_count, 0);
}

#[test]
fn analyze_options_sort_mode_defaults_to_wall_clock() {
    let opts = default_opts();
    assert_eq!(opts.sort_mode(), AnalyzeSort::WallClock);
    let mut opts = default_opts();
    opts.sort = "nonsense".into();
    // Unknown sort key falls back to wall-clock.
    assert_eq!(opts.sort_mode(), AnalyzeSort::WallClock);
}

#[test]
fn analyze_filters_returning_empty_are_ok_not_error() {
    // Issue #256: when filters select zero rows the report is empty
    // but the run still succeeds. Without filters, the legacy
    // input-classification error fires instead.
    let entries = vec![make_journal_line_full("hit", "/rustc", "a", "lib", 1, None)];
    let (_tmp, path) = write_fixture_journal(&entries);
    let mut opts = default_opts();
    opts.crate_name = Some("does-not-exist".into());
    let report = analyze_journal_with(path.to_str().unwrap(), &opts).expect("filtered ok");
    assert_eq!(report.parsed_count, 0);
    assert!(report.crate_rows(&opts).is_empty());
}
