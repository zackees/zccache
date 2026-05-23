//! `zccache-stamp` — CI helper that appends the 96-byte release marker
//! to a built binary.
//!
//! Run after stripping but before archiving. Cross-compile-safe: only
//! appends bytes, never executes the target binary.
//!
//! ```text
//! zccache-stamp \
//!     --binary path/to/zccache.exe \
//!     --sha $GITHUB_SHA \
//!     --version 1.7.2 \
//!     --triple x86_64-pc-windows-msvc \
//!     --timestamp 1700000000
//! ```

use std::path::PathBuf;
use std::process::ExitCode;
use zccache::symbols::marker::{write_marker_to_binary, ReleaseMarker};

fn parse_args() -> Result<Args, String> {
    let mut binary = None;
    let mut sha = None;
    let mut version = None;
    let mut triple = None;
    let mut timestamp = None;

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let take_value =
            |a: &mut std::iter::Skip<std::env::Args>, name: &str| -> Result<String, String> {
                a.next().ok_or_else(|| format!("missing value for {name}"))
            };
        match arg.as_str() {
            "--binary" => binary = Some(PathBuf::from(take_value(&mut args, &arg)?)),
            "--sha" => sha = Some(take_value(&mut args, &arg)?),
            "--version" => version = Some(take_value(&mut args, &arg)?),
            "--triple" => triple = Some(take_value(&mut args, &arg)?),
            "--timestamp" => {
                timestamp = Some(
                    take_value(&mut args, &arg)?
                        .parse::<u64>()
                        .map_err(|e| format!("--timestamp: {e}"))?,
                )
            }
            "--help" | "-h" => {
                print_usage();
                std::process::exit(0);
            }
            other => return Err(format!("unknown flag: {other}")),
        }
    }
    Ok(Args {
        binary: binary.ok_or("--binary is required")?,
        sha: sha.ok_or("--sha is required")?,
        version: version.ok_or("--version is required")?,
        triple: triple.ok_or("--triple is required")?,
        timestamp: timestamp.ok_or("--timestamp is required")?,
    })
}

struct Args {
    binary: PathBuf,
    sha: String,
    version: String,
    triple: String,
    timestamp: u64,
}

fn print_usage() {
    eprintln!(
        "zccache-stamp --binary <path> --sha <hex40> --version <semver> \\\n\
         \t--triple <rustc-target-triple> --timestamp <unix-secs>"
    );
}

fn main() -> ExitCode {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("zccache-stamp: {e}");
            print_usage();
            return ExitCode::from(2);
        }
    };

    let marker = ReleaseMarker {
        git_sha: args.sha,
        version: args.version,
        triple: args.triple,
        build_timestamp: args.timestamp,
    };

    if let Err(e) = write_marker_to_binary(&args.binary, &marker) {
        eprintln!(
            "zccache-stamp: failed to append marker to {}: {e}",
            args.binary.display()
        );
        return ExitCode::from(1);
    }
    println!(
        "stamped {} ({} bytes appended)",
        args.binary.display(),
        zccache::symbols::marker::MARKER_SIZE
    );
    ExitCode::SUCCESS
}
