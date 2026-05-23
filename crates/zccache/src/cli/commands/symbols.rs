//! `zccache symbols` — install + symbolicate subcommands.

use std::path::PathBuf;
use std::process::ExitCode;

use super::super::symbols::{self, InstallOptions as SymbolsInstallOptions};

pub(crate) fn cmd_symbols_symbolicate(dumps: Vec<PathBuf>) -> ExitCode {
    let marker = match crate::symbols::read_marker_from_current_exe() {
        Some(m) => m,
        None => {
            eprintln!(
                "zccache symbolicate: this binary has no release marker (dev build). \
                 No automatic symbol fetch possible — use the local \
                 target/release/zccache.{{pdb,dwp,dSYM}} manually."
            );
            return ExitCode::from(2);
        }
    };

    // Cache layout: `<cache>/symbols/<version>-<triple>/`. One symbol
    // copy per build, referenced from each crash via a `.symref`
    // sidecar — true dedup. The existing `symbols::install` is the
    // battle-tested fetch path; we just point its `--prefix` at our
    // shared dir.
    let cache_root: PathBuf = crate::core::config::default_cache_dir().into_path_buf();
    let symbols_dir =
        crate::symbols::symbols_dir_for(&cache_root, &marker.version, &marker.triple);
    let symbols_dir_path: PathBuf = symbols_dir.into_path_buf();

    let opts = SymbolsInstallOptions {
        version: Some(marker.version.clone()),
        target: Some(marker.triple.clone()),
        prefix: Some(symbols_dir_path.clone()),
        force: false,
        lock_behavior: super::super::symbols::LockBehavior::Wait,
    };
    let report = match symbols::install(opts) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("zccache symbolicate: failed to install symbols: {e}");
            return ExitCode::FAILURE;
        }
    };

    if let Err(e) = crate::symbols::mark_ready(&symbols_dir_path) {
        eprintln!(
            "zccache symbolicate: warning — failed to write .ready sentinel in {}: {e}",
            symbols_dir_path.display()
        );
    }

    println!(
        "zccache symbolicate: symbols at {} (version {} / {})",
        symbols_dir_path.display(),
        marker.version,
        marker.triple,
    );
    if !report.skipped_already_present {
        let source = if report.cache_hit {
            "cached archive"
        } else {
            "GitHub release"
        };
        println!(
            "  (downloaded {} sidecar(s) from {})",
            report.installed.len(),
            source,
        );
    }

    let mut had_error = false;
    for dump in dumps {
        match crate::symbols::write_symref_sidecar(&dump, &symbols_dir_path) {
            Ok(sidecar) => println!("  wrote {}", sidecar.display()),
            Err(e) => {
                eprintln!(
                    "zccache symbolicate: failed to write sidecar for {}: {e}",
                    dump.display()
                );
                had_error = true;
            }
        }
    }
    if had_error {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

pub(crate) fn cmd_symbols_install(
    version: Option<String>,
    target: Option<String>,
    prefix: Option<PathBuf>,
    force: bool,
) -> ExitCode {
    let opts = SymbolsInstallOptions {
        version,
        target,
        prefix,
        force,
        // The user invoked the subcommand directly; wait for any peer
        // install to finish rather than skipping silently.
        lock_behavior: super::super::symbols::LockBehavior::Wait,
    };
    match symbols::install(opts) {
        Ok(report) => {
            if report.skipped_already_present {
                println!(
                    "zccache symbols: already installed in {}",
                    report.prefix.display()
                );
            } else {
                let source = if report.cache_hit {
                    "cached archive"
                } else {
                    "GitHub release"
                };
                println!(
                    "zccache symbols: installed {} sidecar(s) into {} (from {}: {})",
                    report.installed.len(),
                    report.prefix.display(),
                    source,
                    report.url,
                );
                for path in &report.installed {
                    println!("  {}", path.display());
                }
            }
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("zccache symbols install: {err}");
            ExitCode::FAILURE
        }
    }
}
