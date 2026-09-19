//! `zccache snapshot` / `zccache replay` tests (#1595): CLI grammar,
//! `--min-applied-ratio` parsing, report rendering, the ratio gate, and an
//! end-to-end snapshot-then-replay round trip.

use std::path::Path;
use std::process::ExitCode;

use clap::Parser;
use filetime::{set_file_mtime, FileTime};
use zccache_fingerprint::mtime_replay::ReplayReport;

use super::super::args::{Cli, Commands, KNOWN_SUBCOMMANDS};
use super::super::mtime::{cmd_replay, cmd_snapshot, ratio_gate, render_report};

// ─── CLI grammar ─────────────────────────────────────────────────────

#[test]
fn snapshot_parses_workspace_out_and_repeated_exclude() {
    let cli = Cli::try_parse_from([
        "zccache",
        "snapshot",
        "--workspace",
        "ws",
        "--out",
        "m.json",
        "--exclude",
        "build",
        "--exclude",
        "target",
    ])
    .unwrap();
    match cli.command {
        Some(Commands::Snapshot {
            workspace,
            out,
            exclude,
        }) => {
            assert_eq!(workspace, Path::new("ws"));
            assert_eq!(out, Path::new("m.json"));
            assert_eq!(exclude, vec![Path::new("build"), Path::new("target")]);
        }
        other => panic!("expected Snapshot, got {other:?}"),
    }
}

#[test]
fn replay_parses_json_and_min_applied_ratio() {
    let cli = Cli::try_parse_from([
        "zccache",
        "replay",
        "--workspace",
        "ws",
        "--manifest",
        "m.json",
        "--json",
        "--min-applied-ratio",
        "0.9",
    ])
    .unwrap();
    match cli.command {
        Some(Commands::Replay {
            workspace,
            manifest,
            json,
            min_applied_ratio,
        }) => {
            assert_eq!(workspace, Path::new("ws"));
            assert_eq!(manifest, Path::new("m.json"));
            assert!(json);
            assert_eq!(min_applied_ratio, Some(0.9));
        }
        other => panic!("expected Replay, got {other:?}"),
    }
}

#[test]
fn min_applied_ratio_rejects_out_of_range_and_nan() {
    let out_of_range = Cli::try_parse_from([
        "zccache",
        "replay",
        "--workspace",
        "ws",
        "--manifest",
        "m.json",
        "--min-applied-ratio",
        "1.5",
    ]);
    assert!(out_of_range.is_err());

    let nan = Cli::try_parse_from([
        "zccache",
        "replay",
        "--workspace",
        "ws",
        "--manifest",
        "m.json",
        "--min-applied-ratio",
        "nan",
    ]);
    assert!(nan.is_err());
}

#[test]
fn known_subcommands_lists_snapshot_and_replay() {
    assert!(KNOWN_SUBCOMMANDS.contains(&"snapshot"));
    assert!(KNOWN_SUBCOMMANDS.contains(&"replay"));
}

// ─── report rendering ────────────────────────────────────────────────

fn sample_report() -> ReplayReport {
    ReplayReport {
        total: 4,
        applied: 3,
        missing: 1,
        size_mismatch: 0,
        modified: 0,
    }
}

#[test]
fn render_report_text_matches_expected_format() {
    let report = sample_report();
    assert_eq!(
        render_report(&report, false),
        "applied=3 missing=1 size_mismatch=0 modified=0 total=4"
    );
}

#[test]
fn render_report_json_has_expected_keys_and_ratio() {
    let report = sample_report();
    let rendered = render_report(&report, true);
    let parsed: serde_json::Value = serde_json::from_str(&rendered).expect("valid json");
    assert_eq!(parsed["total"], 4);
    assert_eq!(parsed["applied"], 3);
    assert_eq!(parsed["missing"], 1);
    assert_eq!(parsed["size_mismatch"], 0);
    assert_eq!(parsed["modified"], 0);
    assert_eq!(parsed["applied_ratio"].as_f64().unwrap(), 0.75);
}

// ─── ratio gate ──────────────────────────────────────────────────────

#[test]
fn ratio_gate_allows_none() {
    assert!(ratio_gate(&sample_report(), None).is_ok());
}

#[test]
fn ratio_gate_allows_ratio_at_or_above_min() {
    // 3 of 4 applied == 0.75.
    assert!(ratio_gate(&sample_report(), Some(0.75)).is_ok());
}

#[test]
fn ratio_gate_rejects_ratio_below_min() {
    assert!(ratio_gate(&sample_report(), Some(0.9)).is_err());
}

#[test]
fn ratio_gate_rejects_zero_total_when_min_is_set() {
    let empty = ReplayReport {
        total: 0,
        applied: 0,
        missing: 0,
        size_mismatch: 0,
        modified: 0,
    };
    assert!(ratio_gate(&empty, Some(0.5)).is_err());
}

// ─── end-to-end snapshot / replay round trip ────────────────────────

#[test]
fn snapshot_then_replay_restores_only_unchanged_files() {
    let workspace = tempfile::tempdir().expect("workspace tempdir");
    let outside = tempfile::tempdir().expect("outside tempdir");

    let unchanged = workspace.path().join("unchanged.txt");
    let changed = workspace.path().join("changed.txt");
    std::fs::write(&unchanged, "unchanged content").expect("write unchanged");

    let original_content = "a".repeat(16);
    let mutated_content = format!("b{}", "a".repeat(15));
    assert_eq!(
        original_content.len(),
        mutated_content.len(),
        "mutation must preserve file size"
    );
    std::fs::write(&changed, &original_content).expect("write changed");

    let old_mtime = FileTime::from_unix_time(1_000_000, 0);
    set_file_mtime(&unchanged, old_mtime).expect("set unchanged mtime");
    set_file_mtime(&changed, old_mtime).expect("set changed mtime");

    let manifest_path = outside.path().join("manifest.json");
    let snapshot_result = cmd_snapshot(workspace.path(), &manifest_path, &[]);
    assert_eq!(snapshot_result, ExitCode::SUCCESS);

    // Same size, different content -- blake3 must no longer match.
    std::fs::write(&changed, &mutated_content).expect("mutate changed");

    // Simulate a fresh checkout: every file gets a new mtime.
    let fresh_mtime = FileTime::from_unix_time(2_000_000, 0);
    set_file_mtime(&unchanged, fresh_mtime).expect("bump unchanged mtime");
    set_file_mtime(&changed, fresh_mtime).expect("bump changed mtime");

    // Only 1 of 2 files verify, so a ratio gate requiring 100% fails.
    let gated = cmd_replay(workspace.path(), &manifest_path, false, Some(1.0));
    assert_eq!(gated, ExitCode::from(1));

    // Without a gate, replay still succeeds even though it only applied
    // some of the entries.
    let ungated = cmd_replay(workspace.path(), &manifest_path, false, None);
    assert_eq!(ungated, ExitCode::SUCCESS);

    let unchanged_mtime =
        FileTime::from_last_modification_time(&std::fs::metadata(&unchanged).unwrap());
    let changed_mtime =
        FileTime::from_last_modification_time(&std::fs::metadata(&changed).unwrap());
    assert_eq!(
        unchanged_mtime, old_mtime,
        "content-verified file should have its recorded mtime restored"
    );
    assert_eq!(
        changed_mtime, fresh_mtime,
        "changed file must keep its fresh mtime so the build system rebuilds it"
    );
}

#[test]
fn replay_with_missing_manifest_returns_exit_code_two() {
    let workspace = tempfile::tempdir().expect("workspace tempdir");
    let missing_manifest = workspace.path().join("does-not-exist.json");

    let result = cmd_replay(workspace.path(), &missing_manifest, false, None);
    assert_eq!(result, ExitCode::from(2));
}
