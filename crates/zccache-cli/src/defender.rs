//! `zccache defender-exclusions` CLI commands (issue #273).
//!
//! Thin shell over [`zccache_monocrate::core::defender`]. The path-set computation,
//! token elevation check, and PowerShell subprocess plumbing all live in
//! `zccache-core` so the daemon's first-run banner can reuse them.

use std::path::PathBuf;
use std::process::ExitCode;
use zccache_monocrate::core::defender::{
    add_exclusions, compute_exclusion_paths, is_elevated, query_excluded, remove_exclusions,
    ExclusionStatus,
};

/// Stderr line printed when an unprivileged caller runs `add` or `remove`.
const REELEVATE_MSG: &str = "zccache defender-exclusions: this command requires administrator \
elevation. Re-run from an elevated PowerShell or Administrator cmd.";

/// Cache root used by every subcommand. Centralized so a future
/// `zccache cache-root` (issue #275) only changes one site.
fn resolved_cache_root() -> PathBuf {
    zccache_monocrate::core::config::default_cache_dir().into_path_buf()
}

/// On non-Windows, every subcommand prints the same one-liner and exits
/// cleanly so cross-platform scripts can call `defender-exclusions add`
/// unconditionally without branching on OS.
fn windows_only_noop() -> ExitCode {
    println!("Defender exclusion is Windows-only.");
    ExitCode::SUCCESS
}

pub fn cmd_check(json: bool) -> ExitCode {
    if !cfg!(windows) {
        if json {
            let doc = serde_json::json!({
                "supported": false,
                "platform": std::env::consts::OS,
                "message": "Defender exclusion is Windows-only.",
            });
            println!("{doc}");
            return ExitCode::SUCCESS;
        }
        return windows_only_noop();
    }

    let cache_root = resolved_cache_root();
    let paths = compute_exclusion_paths(&cache_root);

    match query_excluded(&paths) {
        Ok(statuses) => {
            if json {
                print_check_json(&cache_root, &statuses, None);
            } else {
                print_check_human(&cache_root, &statuses);
            }
            ExitCode::SUCCESS
        }
        Err(err) => {
            if json {
                let placeholders: Vec<ExclusionStatus> = paths
                    .iter()
                    .map(|p| ExclusionStatus {
                        path: p.clone(),
                        excluded: false,
                    })
                    .collect();
                print_check_json(&cache_root, &placeholders, Some(&err.to_string()));
            } else {
                eprintln!("zccache defender-exclusions: {err}");
            }
            // Failing to query is not fatal — return non-zero so scripts
            // can distinguish "all excluded" (0) from "unknown" (2).
            ExitCode::from(2)
        }
    }
}

pub fn cmd_add() -> ExitCode {
    if !cfg!(windows) {
        return windows_only_noop();
    }
    if !is_elevated() {
        eprintln!("{REELEVATE_MSG}");
        return ExitCode::FAILURE;
    }
    let cache_root = resolved_cache_root();
    let paths = compute_exclusion_paths(&cache_root);
    match add_exclusions(&paths) {
        Ok(()) => {
            for p in &paths {
                println!("added: {}", p.display());
            }
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("zccache defender-exclusions add: {err}");
            ExitCode::FAILURE
        }
    }
}

pub fn cmd_remove() -> ExitCode {
    if !cfg!(windows) {
        return windows_only_noop();
    }
    if !is_elevated() {
        eprintln!("{REELEVATE_MSG}");
        return ExitCode::FAILURE;
    }
    let cache_root = resolved_cache_root();
    let paths = compute_exclusion_paths(&cache_root);
    match remove_exclusions(&paths) {
        Ok(()) => {
            for p in &paths {
                println!("removed: {}", p.display());
            }
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("zccache defender-exclusions remove: {err}");
            ExitCode::FAILURE
        }
    }
}

fn print_check_human(cache_root: &std::path::Path, statuses: &[ExclusionStatus]) {
    println!("cache root: {}", cache_root.display());
    println!("paths checked:");
    for s in statuses {
        let tag = if s.excluded { "yes" } else { "no" };
        println!("  [{tag:3}] {}", s.path.display());
    }
    let any_unexcluded = statuses.iter().any(|s| !s.excluded);
    if any_unexcluded {
        println!();
        println!(
            "hint: run 'zccache defender-exclusions add' from an elevated shell \
             to exclude these paths."
        );
    }
}

fn print_check_json(
    cache_root: &std::path::Path,
    statuses: &[ExclusionStatus],
    error: Option<&str>,
) {
    let paths: Vec<serde_json::Value> = statuses
        .iter()
        .map(|s| {
            serde_json::json!({
                "path": s.path.display().to_string(),
                "excluded": s.excluded,
            })
        })
        .collect();
    let doc = serde_json::json!({
        "supported": true,
        "platform": std::env::consts::OS,
        "cache_root": cache_root.display().to_string(),
        "paths": paths,
        "all_excluded": statuses.iter().all(|s| s.excluded) && !statuses.is_empty(),
        "error": error,
    });
    println!("{doc}");
}
