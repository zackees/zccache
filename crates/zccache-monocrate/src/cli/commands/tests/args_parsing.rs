//! clap-driven CLI argument-parsing tests for `rust-plan`, `session-*`,
//! and `analyze` subcommands, plus the small `session-stats` JSON helpers
//! that share the same surface. These verify the user-visible CLI grammar
//! — adding/renaming a flag should require touching one of the assertions
//! here.

use super::super::args::{Cli, Commands, RustPlanBackendArg, RustPlanCommands};
use super::super::rust_plan::rust_plan_gha_version;
use super::super::session::{
    session_stats_error_json, session_stats_json, session_stats_unavailable_json,
};

#[test]
fn rust_plan_cli_parses_validate_restore_save() {
    use clap::Parser;
    let validate = Cli::try_parse_from([
        "zccache",
        "rust-plan",
        "validate",
        "--plan",
        "plan.json",
        "--json",
    ])
    .unwrap();
    assert!(matches!(
        validate.command,
        Some(Commands::RustPlan {
            action: RustPlanCommands::Validate { json: true, .. }
        })
    ));

    let restore = Cli::try_parse_from([
        "zccache",
        "rust-plan",
        "restore",
        "--plan",
        "plan.json",
        "--backend",
        "local",
        "--session-id",
        "session-123",
        "--endpoint",
        "tcp:127.0.0.1:9",
        "--journal",
        "session.jsonl",
        "--cache-dir",
        ".cache/rust-plan",
    ])
    .unwrap();
    assert!(matches!(
        restore.command,
        Some(Commands::RustPlan {
            action: RustPlanCommands::Restore {
                backend: RustPlanBackendArg::Local,
                session_id: Some(_),
                endpoint: Some(_),
                journal: Some(_),
                ..
            }
        })
    ));

    let save = Cli::try_parse_from([
        "zccache",
        "rust-plan",
        "save",
        "--plan",
        "plan.json",
        "--backend",
        "gha",
    ])
    .unwrap();
    assert!(matches!(
        save.command,
        Some(Commands::RustPlan {
            action: RustPlanCommands::Save {
                backend: RustPlanBackendArg::Gha,
                ..
            }
        })
    ));
}

#[test]
fn rust_plan_session_stats_json_separates_compile_cache_stats() {
    let stats = zccache_monocrate::protocol::SessionStats {
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
    let json = session_stats_json("session-123", &stats);
    assert_eq!(json["status"], "ok");
    assert_eq!(json["session_id"], "session-123");
    assert_eq!(json["compilations"], 10);
    assert_eq!(json["hits"], 7);
    assert_eq!(json["misses"], 3);
    assert_eq!(json["hit_rate"].as_f64().unwrap(), 0.7);
}

#[test]
fn session_end_accepts_json_flag() {
    use clap::Parser;
    let cli = Cli::try_parse_from(["zccache", "session-end", "session-123", "--json"]).unwrap();
    assert!(matches!(
        cli.command,
        Some(Commands::SessionEnd { json: true, .. })
    ));
}

#[test]
fn session_stats_accepts_json_flag() {
    use clap::Parser;
    let cli = Cli::try_parse_from(["zccache", "session-stats", "session-123", "--json"]).unwrap();
    assert!(matches!(
        cli.command,
        Some(Commands::SessionStatsCmd { json: true, .. })
    ));
}

// Issue #256 -- CLI flag parsing for session-start --profile
// and the new zccache analyze filter/sort flags.

#[test]
fn session_start_profile_flag_defaults_to_false() {
    use clap::Parser;
    let cli = Cli::try_parse_from(["zccache", "session-start"]).unwrap();
    match cli.command {
        Some(Commands::SessionStart { profile, .. }) => assert!(!profile),
        other => panic!("expected SessionStart, got {other:?}"),
    }
}

#[test]
fn session_start_profile_flag_parses_when_set() {
    use clap::Parser;
    let cli = Cli::try_parse_from(["zccache", "session-start", "--profile"]).unwrap();
    match cli.command {
        Some(Commands::SessionStart { profile, .. }) => assert!(profile),
        other => panic!("expected SessionStart, got {other:?}"),
    }
}

#[test]
fn analyze_parses_session_crate_outcome_sort_top_flags() {
    use clap::Parser;
    let cli = Cli::try_parse_from([
        "zccache",
        "analyze",
        "x.jsonl",
        "--session",
        "s1",
        "--crate",
        "soldr_cli",
        "--outcome",
        "miss",
        "--sort",
        "misses",
        "--top",
        "5",
    ])
    .unwrap();
    match cli.command {
        Some(Commands::Analyze {
            session,
            crate_name,
            outcome,
            sort,
            top,
            ..
        }) => {
            assert_eq!(session.as_deref(), Some("s1"));
            assert_eq!(crate_name.as_deref(), Some("soldr_cli"));
            assert_eq!(outcome.as_deref(), Some("miss"));
            assert_eq!(sort, "misses");
            assert_eq!(top, Some(5));
        }
        other => panic!("expected Analyze, got {other:?}"),
    }
}

#[test]
fn analyze_sort_defaults_to_wall_clock() {
    use clap::Parser;
    let cli = Cli::try_parse_from(["zccache", "analyze", "x.jsonl"]).unwrap();
    match cli.command {
        Some(Commands::Analyze { sort, top, .. }) => {
            assert_eq!(sort, "wall-clock");
            assert!(top.is_none());
        }
        other => panic!("expected Analyze, got {other:?}"),
    }
}

#[test]
fn session_stats_unavailable_json_has_scrapeable_status() {
    let json = session_stats_unavailable_json("session-123", "stats_not_enabled");
    assert_eq!(json["status"], "unavailable");
    assert_eq!(json["session_id"], "session-123");
    assert_eq!(json["reason"], "stats_not_enabled");
}

#[test]
fn session_stats_error_json_has_scrapeable_status() {
    let json = session_stats_error_json("session-123", "unknown session");
    assert_eq!(json["status"], "error");
    assert_eq!(json["session_id"], "session-123");
    assert_eq!(json["error"], "unknown session");
}

#[test]
fn rust_plan_gha_version_is_stable_for_backend_diagnostics() {
    let key = "rust-plan-v1-test";
    assert_eq!(rust_plan_gha_version(key), rust_plan_gha_version(key));
    assert_ne!(rust_plan_gha_version(key), rust_plan_gha_version("other"));
}
