//! `zccache snapshot` / `zccache replay` — content-verified mtime replay
//! (#1595).
//!
//! `snapshot` walks a workspace and records path, size, mtime, and BLAKE3 of
//! every regular file into a JSON manifest. `replay` reads that manifest back
//! and restores the recorded mtime only onto files whose size and BLAKE3
//! still match; anything changed, missing, or unverifiable keeps its fresh
//! mtime so the build system rebuilds it instead of trusting a stale
//! fingerprint. See `zccache_fingerprint::mtime_replay` for the walk/verify
//! implementation this module wraps in CLI plumbing.
//!
//! Exit-code contract for `replay`: `0` ok, `1` below
//! `--min-applied-ratio`, `2` error (unreadable manifest, or `snapshot`
//! failing to walk/write).

use std::path::Path;
use std::process::ExitCode;

use zccache_fingerprint::mtime_replay::{
    read_manifest, replay, snapshot, write_manifest, ReplayReport,
};

/// clap `value_parser` for `--min-applied-ratio`: a finite f64 in `0.0..=1.0`.
pub(crate) fn parse_ratio(s: &str) -> Result<f64, String> {
    let value: f64 = s
        .parse()
        .map_err(|_| format!("--min-applied-ratio must be a number, got {s:?}"))?;
    if value.is_nan() {
        return Err("--min-applied-ratio must not be NaN".to_string());
    }
    if !(0.0..=1.0).contains(&value) {
        return Err(format!(
            "--min-applied-ratio must be in 0.0..=1.0, got {value}"
        ));
    }
    Ok(value)
}

/// `zccache snapshot --workspace <dir> --out <manifest.json> [--exclude <dir>]...`
pub(crate) fn cmd_snapshot(workspace: &Path, out: &Path, excludes: &[&Path]) -> ExitCode {
    let manifest = match snapshot(workspace, excludes) {
        Ok(manifest) => manifest,
        Err(e) => {
            eprintln!("zccache snapshot: {e}");
            return ExitCode::from(2);
        }
    };
    match write_manifest(out, &manifest) {
        Ok(()) => {
            eprintln!(
                "zccache snapshot: wrote {} ({} files)",
                out.display(),
                manifest.entries.len()
            );
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("zccache snapshot: {e}");
            ExitCode::from(2)
        }
    }
}

/// Render a [`ReplayReport`] either as a human-readable one-liner or as a
/// JSON object with an `applied_ratio` field alongside the raw counts.
pub(crate) fn render_report(report: &ReplayReport, json: bool) -> String {
    if json {
        serde_json::json!({
            "total": report.total,
            "applied": report.applied,
            "missing": report.missing,
            "size_mismatch": report.size_mismatch,
            "modified": report.modified,
            "applied_ratio": report.applied_ratio(),
        })
        .to_string()
    } else {
        format!(
            "applied={} missing={} size_mismatch={} modified={} total={}",
            report.applied, report.missing, report.size_mismatch, report.modified, report.total
        )
    }
}

/// `Ok(())` when `report`'s applied ratio meets `min` (or `min` is `None`).
pub(crate) fn ratio_gate(report: &ReplayReport, min: Option<f64>) -> Result<(), String> {
    let Some(min) = min else {
        return Ok(());
    };
    let ratio = report.applied_ratio();
    if ratio < min {
        return Err(format!(
            "applied ratio {:.4} ({} of {}) is below --min-applied-ratio {}",
            ratio, report.applied, report.total, min
        ));
    }
    Ok(())
}

/// `zccache replay --workspace <dir> --manifest <manifest.json> [--json] [--min-applied-ratio <r>]`
pub(crate) fn cmd_replay(
    workspace: &Path,
    manifest: &Path,
    json: bool,
    min_applied_ratio: Option<f64>,
) -> ExitCode {
    let manifest = match read_manifest(manifest) {
        Ok(manifest) => manifest,
        Err(e) => {
            eprintln!("zccache replay: {e}");
            return ExitCode::from(2);
        }
    };
    let report = replay(workspace, &manifest);
    if json {
        println!("{}", render_report(&report, true));
    } else {
        println!("zccache replay: {}", render_report(&report, false));
    }
    match ratio_gate(&report, min_applied_ratio) {
        Ok(()) => ExitCode::SUCCESS,
        Err(msg) => {
            eprintln!("zccache replay: {msg}");
            ExitCode::from(1)
        }
    }
}
