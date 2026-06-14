//! Internal CLI dispatch for the `zccache` binary.
//!
//! `main.rs` is a thin entry point that hands raw argv to [`run`] below;
//! this module owns the clap definitions (`args`), the dispatch match,
//! and every per-subcommand implementation.

use crate::core::NormalizedPath;
use std::path::Path;
use std::process::ExitCode;

pub(crate) mod analyze;
pub(crate) mod args;
pub(crate) mod cache_ops;
pub(crate) mod cargo_registry;
pub(crate) mod daemon;
pub(crate) mod download;
pub(crate) mod exec;
pub(crate) mod fp;
pub(crate) mod gha;
pub(crate) mod meson_cache;
pub(crate) mod rust_plan;
pub(crate) mod service_definition;
pub(crate) mod session;
pub(crate) mod status;
pub(crate) mod symbols;
pub(crate) mod targz;
pub(crate) mod util;
pub(crate) mod wrap;

use super::defender;
use super::symbols as symbols_lib;
use super::{run_ino_convert_cached, InoConvertOptions};
use super::{ArchiveFormat, DownloadParams, WaitMode};

use args::{
    CargoRegistryCommands, Cli, Commands, DefenderExclusionsCommands, FpCommands, GhaCacheCommands,
    SymbolsCommands, KNOWN_SUBCOMMANDS,
};
use util::{absolute_path, init_tracing, resolve_endpoint, run_async};

/// Parse argv, run the requested subcommand or wrapper path, and return
/// the process exit code.
pub fn run() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();

    // Best-effort: if the user opted in via env, fetch matching debug
    // sidecars before doing anything else so the very first command's
    // failure (if any) lands with resolvable symbols. Idempotent — skips
    // when already installed. See `crate::cli::symbols`.
    symbols_lib::maybe_auto_install();

    // Auto-detect: if first arg isn't a known subcommand or a --flag, enter wrap mode.
    // e.g., `zccache clang++ -c foo.cpp -o foo.o`
    match wrap::strip_leading_strict_paths_flags(&args[1..]) {
        Ok((strict_paths, wrapper_args))
            if !wrapper_args.is_empty()
                && !KNOWN_SUBCOMMANDS.contains(&wrapper_args[0].as_str())
                && !wrapper_args[0].starts_with("--") =>
        {
            return wrap::run_wrap(&wrapper_args, strict_paths);
        }
        Err(err) => {
            eprintln!("zccache: {err}");
            return ExitCode::FAILURE;
        }
        _ => {}
    }

    use clap::Parser;
    let cli = Cli::parse();
    let global_strict_paths = cli.strict_paths.clone();

    init_tracing();

    // Handle top-level flags (sccache-compatible)
    if cli.clear {
        let endpoint = resolve_endpoint(None);
        return run_async(cache_ops::cmd_clear(&endpoint));
    }
    if cli.show_stats {
        let endpoint = resolve_endpoint(None);
        return run_async(status::cmd_status(&endpoint, false));
    }

    let command = match cli.command {
        Some(cmd) => cmd,
        None => {
            // No subcommand and no flag — show help.
            use clap::CommandFactory;
            Cli::command().print_help().ok();
            return ExitCode::FAILURE;
        }
    };

    dispatch(command, global_strict_paths.as_deref())
}

fn dispatch(command: Commands, global_strict_paths: Option<&str>) -> ExitCode {
    match command {
        Commands::Start => daemon::run_start(),
        Commands::Stop => daemon::run_stop(),
        Commands::Status { json } => {
            let endpoint = resolve_endpoint(None);
            run_async(status::cmd_status(&endpoint, json))
        }
        Commands::Analyze {
            journal,
            json,
            session,
            crate_name,
            outcome,
            sort,
            top,
        } => analyze::cmd_analyze(
            &journal,
            analyze::AnalyzeOptions {
                json,
                session,
                crate_name,
                outcome,
                sort,
                top,
            },
        ),
        Commands::Clear => {
            let endpoint = resolve_endpoint(None);
            run_async(cache_ops::cmd_clear(&endpoint))
        }
        Commands::InstallServicedef { daemon_binary, dir } => {
            service_definition::run_install_servicedef(daemon_binary, dir)
        }
        Commands::Ino {
            input,
            output,
            clang_args,
            no_arduino_include,
        } => match run_ino_convert_cached(
            Path::new(&input),
            Path::new(&output),
            &InoConvertOptions {
                clang_args,
                inject_arduino_include: !no_arduino_include,
            },
        ) {
            Ok(_) => ExitCode::SUCCESS,
            Err(err) => {
                eprintln!("zccache: {err}");
                ExitCode::FAILURE
            }
        },
        Commands::GhaCache { action } => match action {
            GhaCacheCommands::Status => gha::cmd_gha_status(),
            GhaCacheCommands::Save { key, path } => run_async(gha::cmd_gha_save(&key, &path)),
            GhaCacheCommands::Restore { key, path } => run_async(gha::cmd_gha_restore(&key, &path)),
        },
        Commands::RustPlan { action } => run_async(rust_plan::cmd_rust_plan(action)),
        Commands::Download {
            url,
            part_urls,
            archive_path,
            unarchive_path,
            expected_sha256,
            max_connections,
            min_segment_size,
            no_wait,
            dry_run,
            force,
        } => download::cmd_download(DownloadParams {
            source: match download::resolve_download_source(url, part_urls) {
                Ok(source) => source,
                Err(err) => {
                    eprintln!("zccache download: {err}");
                    return ExitCode::FAILURE;
                }
            },
            archive_path: archive_path.map(Into::into),
            unarchive_path: unarchive_path.map(Into::into),
            expected_sha256,
            archive_format: ArchiveFormat::Auto,
            max_connections,
            min_segment_size,
            wait_mode: if no_wait {
                WaitMode::NoWait
            } else {
                WaitMode::Block
            },
            dry_run,
            force,
        }),
        Commands::SessionStart {
            cwd,
            log,
            endpoint,
            cache_dir,
            private_daemon,
            daemon_name,
            owner_pid,
            private_env,
            stats,
            journal,
            profile,
        } => {
            let cache_dir = cache_dir.map(|p| absolute_path(&p));
            let private_env = match session::parse_private_env_assignments(&private_env) {
                Ok(env) => env,
                Err(err) => {
                    eprintln!("error: {err}");
                    return ExitCode::FAILURE;
                }
            };
            let mut private_options = session::SessionStartPrivateOptions {
                cache_dir,
                private_daemon,
                daemon_name,
                owner_pids: owner_pid,
                private_env,
            };
            private_options.ensure_private_identity(endpoint.as_deref());
            let endpoint =
                session::resolve_session_start_endpoint(endpoint.as_deref(), &private_options);
            let cwd = cwd
                .map(NormalizedPath::from)
                .unwrap_or_else(|| std::env::current_dir().unwrap_or_default().into());
            let log = log.map(|p| absolute_path(&p));
            let journal = journal.map(|p| {
                if !p.ends_with(".jsonl") {
                    eprintln!("error: --journal path must end in .jsonl");
                    std::process::exit(1);
                }
                absolute_path(&p)
            });
            run_async(session::cmd_session_start(
                &endpoint,
                cwd.as_path(),
                log.as_deref(),
                stats,
                journal,
                profile,
                private_options,
            ))
        }
        Commands::SessionEnd {
            session_id,
            endpoint,
            json,
        } => {
            let endpoint = resolve_endpoint(endpoint.as_deref());
            session::cmd_session_end(&endpoint, session_id, json)
        }
        Commands::SessionStatsCmd {
            session_id,
            endpoint,
            json,
        } => {
            let endpoint = resolve_endpoint(endpoint.as_deref());
            run_async(session::cmd_session_stats(&endpoint, session_id, json))
        }
        Commands::Wrap { strict_paths, args } => {
            let strict_paths = match wrap::parse_optional_strict_paths(
                strict_paths.as_deref().or(global_strict_paths),
            ) {
                Ok(mode) => mode,
                Err(err) => {
                    eprintln!("zccache: {err}");
                    return ExitCode::FAILURE;
                }
            };
            wrap::run_wrap(&args, strict_paths)
        }
        Commands::Inspect { key } => {
            eprintln!("zccache inspect {key}: not yet implemented");
            ExitCode::FAILURE
        }
        Commands::Crashes { clear } => cache_ops::cmd_crashes(clear),
        Commands::Fp {
            cache_file,
            cache_type,
            endpoint,
            fp_command,
        } => {
            let endpoint = resolve_endpoint(endpoint.as_deref());
            let cache_file = absolute_path(&cache_file);
            match fp_command {
                FpCommands::Check {
                    root,
                    ext,
                    include,
                    exclude,
                } => {
                    let root = absolute_path(&root);
                    run_async(fp::cmd_fp_check(
                        &endpoint,
                        cache_file.as_path(),
                        &cache_type,
                        root.as_path(),
                        &ext,
                        &include,
                        &exclude,
                    ))
                }
                FpCommands::MarkSuccess => {
                    run_async(fp::cmd_fp_mark(&endpoint, cache_file.as_path(), true))
                }
                FpCommands::MarkFailure => {
                    run_async(fp::cmd_fp_mark(&endpoint, cache_file.as_path(), false))
                }
                FpCommands::Invalidate => {
                    run_async(fp::cmd_fp_invalidate(&endpoint, cache_file.as_path()))
                }
            }
        }
        Commands::CargoRegistry { action } => match action {
            CargoRegistryCommands::Save { key, cargo_home } => {
                cargo_registry::cmd_cargo_registry_save(&key, cargo_home.as_deref())
            }
            CargoRegistryCommands::Restore { key, cargo_home } => {
                cargo_registry::cmd_cargo_registry_restore(&key, cargo_home.as_deref())
            }
            CargoRegistryCommands::Hash { lockfile } => {
                cargo_registry::cmd_cargo_registry_hash(&lockfile)
            }
            CargoRegistryCommands::Clean => cargo_registry::cmd_cargo_registry_clean(),
        },
        Commands::Kv { action } => cache_ops::cmd_kv(action),
        Commands::Warm {
            target_dir,
            profile,
            ..
        } => {
            let target_dir = absolute_path(&target_dir);
            cache_ops::cmd_warm(target_dir.as_path(), &profile)
        }
        Commands::SnapshotBytes {
            target,
            prune_incremental,
            prune_build_script_out,
        } => cache_ops::cmd_snapshot_bytes(&target, prune_incremental, prune_build_script_out),
        Commands::SnapshotFpRecord {
            target_dir,
            workspace_root,
            profile,
            manifest_path,
        } => {
            cache_ops::cmd_snapshot_fp_record(&target_dir, workspace_root, &profile, manifest_path)
        }
        Commands::SnapshotFpValidate {
            target_dir,
            workspace_root,
            profile,
            manifest_path,
            stamp_seconds_ahead,
        } => cache_ops::cmd_snapshot_fp_validate(
            &target_dir,
            workspace_root,
            &profile,
            manifest_path,
            stamp_seconds_ahead,
        ),
        Commands::Symbols { action } => match action {
            SymbolsCommands::Install {
                version,
                target,
                prefix,
                force,
            } => symbols::cmd_symbols_install(version, target, prefix, force),
            SymbolsCommands::Symbolicate { dumps } => symbols::cmd_symbols_symbolicate(dumps),
        },
        Commands::CacheRoot { json } => cache_ops::cmd_cache_root(json),
        Commands::DefenderExclusions { action } => match action {
            DefenderExclusionsCommands::Check { json } => defender::cmd_check(json),
            DefenderExclusionsCommands::Add => defender::cmd_add(),
            DefenderExclusionsCommands::Remove => defender::cmd_remove(),
        },
        Commands::Cc { args } => {
            // Build the wrap argv: ["cc", <args...>] and let the existing
            // dispatcher detect Gcc (cc falls through to Gcc in
            // `detect_family`), resolve the binary on PATH, and route to
            // the compile path.
            let mut wrap_args: Vec<String> = Vec::with_capacity(args.len() + 1);
            wrap_args.push("cc".to_string());
            wrap_args.extend(args);
            let strict_paths = match wrap::parse_optional_strict_paths(global_strict_paths) {
                Ok(mode) => mode,
                Err(err) => {
                    eprintln!("zccache: {err}");
                    return ExitCode::FAILURE;
                }
            };
            wrap::run_wrap(&wrap_args, strict_paths)
        }
        Commands::Cache { command } => match command {
            args::CacheCommands::Size { json } => cache_ops::cmd_cache_size(json),
        },
        Commands::Meson { command } => match command {
            args::MesonCommands::Configure {
                source_dir,
                build_dir,
                meson_bin,
                input_env,
                input_file,
                no_walk,
                meson_args,
            } => meson_cache::cmd_configure(
                source_dir, build_dir, meson_bin, input_env, input_file, no_walk, meson_args,
            ),
        },
        Commands::Exec {
            input_file,
            input_env,
            input_extra,
            output_stdout,
            output_stderr,
            output_file,
            tool_hash,
            no_cache,
            no_cwd_in_key,
            endpoint,
            include_scan,
            include_dir,
            system_include,
            iquote_dir,
            depfile,
            non_deterministic,
            key_args_filter,
            tool_command,
        } => exec::cmd_exec(exec::ExecParams {
            input_files: input_file,
            input_env,
            input_extra,
            output_stdout,
            output_stderr,
            output_files: output_file,
            tool_hash,
            no_cache,
            no_cwd_in_key,
            endpoint,
            tool_command,
            include_scan,
            include_dir,
            system_include,
            iquote_dir,
            depfile,
            non_deterministic,
            key_args_filter,
        }),
    }
}

#[cfg(test)]
mod tests;
