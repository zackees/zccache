//! clap-driven CLI argument-parsing tests for `rust-plan`, `session-*`,
//! and `analyze` subcommands, plus the small `session-stats` JSON helpers
//! that share the same surface. These verify the user-visible CLI grammar
//! — adding/renaming a flag should require touching one of the assertions
//! here.

use super::super::args::{Cli, Commands, RustPlanBackendArg, RustPlanCommands, KNOWN_SUBCOMMANDS};
use super::super::rust_plan::rust_plan_gha_version;
use super::super::session::{
    session_stats_error_json, session_stats_json, session_stats_unavailable_json,
    SessionStartPrivateOptions,
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

    let restore_layered = Cli::try_parse_from([
        "zccache",
        "rust-plan",
        "restore-layered",
        "--plan",
        "plan.json",
        "--base-cache-dir",
        ".cache/base",
        "--delta-cache-dir",
        ".cache/delta",
    ])
    .unwrap();
    assert!(matches!(
        restore_layered.command,
        Some(Commands::RustPlan {
            action: RustPlanCommands::RestoreLayered { .. }
        })
    ));

    let save_delta = Cli::try_parse_from([
        "zccache",
        "rust-plan",
        "save-delta",
        "--plan",
        "plan.json",
        "--base-cache-dir",
        ".cache/base",
        "--delta-cache-dir",
        ".cache/delta",
    ])
    .unwrap();
    assert!(matches!(
        save_delta.command,
        Some(Commands::RustPlan {
            action: RustPlanCommands::SaveDelta { .. }
        })
    ));
}

#[test]
fn rust_plan_session_stats_json_separates_compile_cache_stats() {
    let stats = crate::protocol::SessionStats {
        duration_ms: 1000,
        compilations: 10,
        hits: 7,
        misses: 3,
        non_cacheable: 2,
        errors: 1,
        errors_cached: 1,
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
fn session_start_private_daemon_flags_parse() {
    use clap::Parser;
    let cli = Cli::try_parse_from([
        "zccache",
        "session-start",
        "--private-daemon",
        "--daemon-name",
        "soldr-dev",
        "--cache-dir",
        ".zccache-soldr-dev",
        "--owner-pid",
        "1234",
        "--owner-pid",
        "5678",
        "--private-env",
        "ZCCACHE_PATH_REMAP=auto",
    ])
    .unwrap();
    match cli.command {
        Some(Commands::SessionStart {
            private_daemon,
            daemon_name,
            cache_dir,
            owner_pid,
            private_env,
            ..
        }) => {
            assert!(private_daemon);
            assert_eq!(daemon_name.as_deref(), Some("soldr-dev"));
            assert_eq!(cache_dir.as_deref(), Some(".zccache-soldr-dev"));
            assert_eq!(owner_pid, vec![1234, 5678]);
            assert_eq!(private_env, vec!["ZCCACHE_PATH_REMAP=auto"]);
        }
        other => panic!("expected SessionStart, got {other:?}"),
    }
}

#[test]
fn private_env_assignment_parser_rejects_missing_equals() {
    let raw = vec!["ZCCACHE_PATH_REMAP".to_string()];
    assert!(super::super::session::parse_private_env_assignments(&raw).is_err());
}

#[test]
fn session_start_private_options_generate_private_daemon_identity() {
    let mut options = SessionStartPrivateOptions {
        private_daemon: true,
        ..SessionStartPrivateOptions::default()
    };
    options.ensure_private_identity(None);

    assert!(options
        .daemon_name
        .as_deref()
        .is_some_and(|name| name.starts_with("private-")));
}

#[test]
fn session_start_explicit_endpoint_generates_non_default_private_identity() {
    let mut options = SessionStartPrivateOptions {
        private_env: vec![("ZCCACHE_PATH_REMAP".to_string(), "auto".to_string())],
        ..SessionStartPrivateOptions::default()
    };
    options.ensure_private_identity(Some("127.0.0.1:45454"));

    let daemon_name = options.daemon_name.as_deref().unwrap();
    assert_ne!(daemon_name, crate::core::config::DEFAULT_DAEMON_NAMESPACE);
    assert!(daemon_name.starts_with("endpoint-"));
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

/// Locks the auto-detect contract: every name in the `Commands` clap enum
/// MUST be listed in `KNOWN_SUBCOMMANDS`, otherwise `mod.rs::run` treats
/// the subcommand as a compiler invocation and surfaces "daemon error:
/// failed to run compiler: program not found" instead of dispatching. This
/// drift previously hid `zccache analyze` and `zccache kv` for users.
#[test]
fn known_subcommands_matches_clap_enum() {
    use clap::CommandFactory;
    let cmd = Cli::command();
    let clap_subs: std::collections::BTreeSet<String> = cmd
        .get_subcommands()
        .map(|s| s.get_name().to_string())
        .collect();
    let known: std::collections::BTreeSet<String> = KNOWN_SUBCOMMANDS
        .iter()
        .filter(|s| !s.starts_with('-'))
        // `help` is a clap-managed meta-subcommand that doesn't appear in the
        // `Commands` enum but is still a valid invocation we want auto-detect
        // to recognize, so keep it in KNOWN_SUBCOMMANDS without requiring it
        // in the enum.
        .filter(|s| **s != "help")
        .map(|s| s.to_string())
        .collect();

    let missing: Vec<&String> = clap_subs.difference(&known).collect();
    assert!(
        missing.is_empty(),
        "KNOWN_SUBCOMMANDS is missing {} clap subcommand(s): {:?}. \
         Without these entries, `zccache <name>` falls through to wrap mode \
         and fails with 'failed to run compiler: program not found'. Add them \
         to KNOWN_SUBCOMMANDS in args.rs.",
        missing.len(),
        missing,
    );

    let stale: Vec<&String> = known.difference(&clap_subs).collect();
    assert!(
        stale.is_empty(),
        "KNOWN_SUBCOMMANDS lists {} name(s) not in the clap Commands enum: {:?}. \
         Either re-add the subcommand or drop the stale entry.",
        stale.len(),
        stale,
    );
}

/// Lock in that the top-level `--help` output mentions the env vars users
/// most often need to discover from the CLI. Issue #672 surfaced the gap
/// for `ZCCACHE_PATH_REMAP` specifically — it has no clap flag, so without
/// the `after_help` block there is no way to learn about it short of
/// reading the README.
#[test]
fn top_level_help_lists_discoverable_env_vars() {
    use clap::CommandFactory;
    let help = Cli::command().render_help().to_string();
    for var in [
        "ZCCACHE_PATH_REMAP",
        "ZCCACHE_STRICT_PATHS",
        "ZCCACHE_CACHE_DIR",
    ] {
        assert!(
            help.contains(var),
            "`zccache --help` no longer mentions {var}. \
             If the env var was renamed or removed, update args.rs::ENV_VARS_HELP; \
             do not silently drop it (issue #672)."
        );
    }
}
