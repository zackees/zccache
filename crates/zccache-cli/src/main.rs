//! zccache CLI -- command-line interface for the compiler cache.
//!
//! Usage modes:
//!
//! 1. Subcommand mode:
//!    zccache session-start --compiler /path/to/clang++
//!    zccache session-end `<id>`
//!    zccache status
//!
//! 2. Compiler wrapper mode (auto-detected):
//!    ZCCACHE_SESSION_ID=42 zccache clang++ -c foo.cpp -o foo.o
//!
//!    If the first arg isn't a known subcommand, zccache treats
//!    the entire command line as a compiler invocation and forwards
//!    it to the daemon via the session from ZCCACHE_SESSION_ID.

#[cfg(unix)]
#[global_allocator]
static GLOBAL: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

#[cfg(windows)]
#[global_allocator]
static GLOBAL_WIN: mimalloc::MiMalloc = mimalloc::MiMalloc;

mod snapshot_fp;

use clap::{Parser, Subcommand, ValueEnum};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use zccache_artifact::{
    restore_rust_plan_local, rust_plan_bundle_dir, rust_plan_cache_key, save_rust_plan_local,
    RustArtifactPlanV1, RustPlanError, RustPlanOperation, RustPlanSummary,
};
use zccache_cli::{
    client_download, run_ino_convert_cached, session_end_idempotent, ArchiveFormat, DownloadParams,
    DownloadSource, InoConvertOptions, WaitMode,
};
use zccache_compiler::strict_paths::StrictPathsMode;
use zccache_core::NormalizedPath;
use zccache_gha::{GhaCache, GhaError};

/// zccache -- fast local compiler cache.
#[derive(Debug, Parser)]
#[command(name = "zccache", version, about)]
struct Cli {
    /// Clear the entire artifact cache (same as `zccache clear`).
    #[arg(long)]
    clear: bool,

    /// Show daemon and cache statistics (same as `zccache status`).
    #[arg(long)]
    show_stats: bool,

    /// Validate compiler path flag spelling: off, consistent, or absolute.
    #[arg(
        long,
        value_name = "MODE",
        num_args = 0..=1,
        default_missing_value = "absolute",
        require_equals = true
    )]
    strict_paths: Option<String>,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Start the daemon (if not already running).
    Start,
    /// Stop the daemon.
    #[command(visible_alias = "kill")]
    Stop,
    /// Show daemon and cache status.
    Status {
        /// Print the daemon status as a JSON document to stdout.
        #[arg(long)]
        json: bool,
    },
    /// Analyze a per-session JSONL compile journal and roll it up into a
    /// hit/miss breakdown by output extension, by tool, and by source file.
    /// Reads the file path passed positionally; does not contact the daemon.
    Analyze {
        /// Path to a compile journal JSONL file (the `--journal` output of
        /// `session-start`).
        journal: String,
        /// Print the analysis as a JSON document on stdout.
        #[arg(long)]
        json: bool,
    },
    /// Clear the artifact cache.
    Clear,
    /// Start a build session. Prints session ID to stdout.
    #[command(name = "session-start")]
    SessionStart {
        /// Working directory (defaults to current dir).
        #[arg(long)]
        cwd: Option<String>,
        /// Path to a log file for this session.
        #[arg(long)]
        log: Option<String>,
        /// IPC endpoint (default: platform-specific).
        #[arg(long)]
        endpoint: Option<String>,
        /// Enable per-session hit/miss statistics tracking.
        #[arg(long)]
        stats: bool,
        /// Write a per-session JSONL compile journal to this path (must end in .jsonl).
        #[arg(long)]
        journal: Option<String>,
    },
    /// End a build session.
    #[command(name = "session-end")]
    SessionEnd {
        /// Session ID to end.
        session_id: String,
        /// IPC endpoint (default: platform-specific).
        #[arg(long)]
        endpoint: Option<String>,
        /// Print final session statistics as JSON to stdout.
        #[arg(long)]
        json: bool,
    },
    /// Query stats for an active session (without ending it).
    #[command(name = "session-stats")]
    SessionStatsCmd {
        /// Session ID to query.
        session_id: String,
        /// IPC endpoint (default: platform-specific).
        #[arg(long)]
        endpoint: Option<String>,
        /// Print active session statistics as JSON to stdout.
        #[arg(long)]
        json: bool,
    },
    /// Wrap a compiler invocation (explicit mode).
    Wrap {
        /// Validate compiler path flag spelling: off, consistent, or absolute.
        #[arg(
            long,
            value_name = "MODE",
            num_args = 0..=1,
            default_missing_value = "absolute",
            require_equals = true
        )]
        strict_paths: Option<String>,
        /// The compiler and its arguments.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Show detailed information about a cache entry.
    Inspect {
        /// Cache key (hex).
        key: String,
    },
    /// Show or clear crash dumps from previous daemon crashes.
    Crashes {
        /// Delete all crash dumps.
        #[arg(long)]
        clear: bool,
    },
    /// Fingerprint-based file change detection.
    ///
    /// Answers "have files changed since the last successful operation?" by
    /// querying the daemon's in-memory watch state (<1ms on cache hit).
    #[command(name = "fp")]
    Fp {
        /// Path to the cache file (e.g., .cache/lint.json).
        #[arg(long)]
        cache_file: String,

        /// Cache algorithm: hash or two-layer.
        #[arg(long, default_value = "two-layer")]
        cache_type: String,

        /// IPC endpoint (default: platform-specific).
        #[arg(long)]
        endpoint: Option<String>,

        #[command(subcommand)]
        fp_command: FpCommands,
    },
    /// Convert an Arduino `.ino` sketch into a generated `.ino.cpp`.
    #[command(name = "ino")]
    Ino {
        /// Input `.ino` file.
        #[arg(long)]
        input: String,
        /// Output `.ino.cpp` file.
        #[arg(long)]
        output: String,
        /// Extra clang arguments used when parsing the `.ino`.
        #[arg(long = "clang-arg")]
        clang_args: Vec<String>,
        /// Do not inject `#include <Arduino.h>`.
        #[arg(long)]
        no_arduino_include: bool,
    },
    /// GitHub Actions cache operations.
    #[command(name = "gha-cache")]
    GhaCache {
        #[command(subcommand)]
        action: GhaCacheCommands,
    },
    /// Execute versioned Rust artifact cache plans.
    #[command(name = "rust-plan")]
    RustPlan {
        #[command(subcommand)]
        action: RustPlanCommands,
    },
    /// Download and optionally unarchive an artifact using the dedicated download daemon.
    Download {
        /// Source URL for a normal single-file download.
        #[arg(long)]
        url: Option<String>,
        /// One explicit URL per multipart segment, in concatenation order.
        #[arg(long = "part-url")]
        part_urls: Vec<String>,
        /// Optional archive/cache path. If omitted, zccache chooses a deterministic cache path.
        archive_path: Option<String>,
        /// Optional destination to expand or unarchive into.
        #[arg(long = "unarchive")]
        unarchive_path: Option<String>,
        /// Optional expected SHA-256 of the downloaded artifact.
        #[arg(long = "sha256")]
        expected_sha256: Option<String>,
        /// Number of parallel range connections to use for single-URL downloads.
        #[arg(long)]
        max_connections: Option<usize>,
        /// Minimum segment size before single-URL downloads switch to ranged fetching.
        #[arg(long)]
        min_segment_size: Option<u64>,
        /// Return immediately with `locked` if another client owns the artifact lock.
        #[arg(long)]
        no_wait: bool,
        /// Report what would happen without mutating the filesystem.
        #[arg(long)]
        dry_run: bool,
        /// Force re-download and re-expand even if cached state is already valid.
        #[arg(long)]
        force: bool,
    },
    /// Manage cargo registry cache (save/restore/hash/clean).
    #[command(name = "cargo-registry")]
    CargoRegistry {
        #[command(subcommand)]
        action: CargoRegistryCommands,
    },
    /// Namespaced blake3-keyed key/value store.
    ///
    /// Backed by `~/.zccache/index.redb` (separate redb table) and spilled
    /// payloads under `~/.zccache/kv/<namespace>/<hex>.bin`. See issue #130.
    Kv {
        #[command(subcommand)]
        action: KvCommands,
    },
    /// Pre-populate target/ with cached artifacts for near-instant builds.
    Warm {
        /// Cargo target directory (default: ./target).
        #[arg(long, default_value = "target")]
        target_dir: String,
        /// Build profile (default: debug).
        #[arg(long, default_value = "debug")]
        profile: String,
        /// IPC endpoint (default: platform-specific).
        #[arg(long)]
        endpoint: Option<String>,
    },
    /// Sum byte size of regular files under a target directory, with optional
    /// pruning. Used by `action/cleanup/prepare-target-snapshot.sh` instead of
    /// Python `os.walk` because jwalk parallelizes readdir+stat across cores —
    /// big win on Windows where per-file Defender callbacks dominate the walk.
    /// Prints total bytes as a decimal integer on stdout. See zccache#189.
    #[command(name = "snapshot-bytes")]
    SnapshotBytes {
        /// Directory to walk.
        #[arg(long)]
        target: PathBuf,
        /// Skip `incremental/` directories during the walk.
        #[arg(long)]
        prune_incremental: bool,
        /// Skip `*/build/*/out/` directories during the walk.
        #[arg(long)]
        prune_build_script_out: bool,
    },
    /// Pre-tar: record blake3 hashes of every workspace source tracked by
    /// each crate's cargo dep-info. The sidecar manifest written under
    /// `<target>/.zccache-fp-manifest.json` lets `snapshot-fp-validate`
    /// (run post-restore on a different runner) decide *per crate* which
    /// fingerprints still match the current source tree. See the rationale
    /// in `snapshot_fp.rs`.
    #[command(name = "snapshot-fp-record")]
    SnapshotFpRecord {
        /// Cargo target directory (default: ./target).
        #[arg(long, default_value = "target")]
        target_dir: PathBuf,
        /// Workspace root (paths in the manifest are stored relative to this).
        /// Defaults to the current working directory.
        #[arg(long)]
        workspace_root: Option<PathBuf>,
        /// Build profile under target/ to walk (default: debug).
        #[arg(long, default_value = "debug")]
        profile: String,
        /// Manifest output path (default: `<target>/.zccache-fp-manifest.json`).
        #[arg(long)]
        manifest_path: Option<PathBuf>,
    },
    /// Post-restore: read the manifest and bump only the dep-info mtimes of
    /// crates whose every tracked source still matches its recorded hash.
    /// Crates with any mismatch are left alone so cargo's normal
    /// `source.mtime > dep_info.mtime → rebuild` check fires for them.
    #[command(name = "snapshot-fp-validate")]
    SnapshotFpValidate {
        #[arg(long, default_value = "target")]
        target_dir: PathBuf,
        #[arg(long)]
        workspace_root: Option<PathBuf>,
        #[arg(long, default_value = "debug")]
        profile: String,
        #[arg(long)]
        manifest_path: Option<PathBuf>,
        /// How far in the future (seconds) to stamp clean crates' dep-info
        /// files. Default 60 matches the existing post-restore touch step
        /// in `action.yml` so output and fingerprint mtimes line up.
        #[arg(long, default_value_t = 60)]
        stamp_seconds_ahead: u64,
    },
}

#[derive(Debug, Subcommand)]
enum CargoRegistryCommands {
    /// Save cargo registry to a compressed archive.
    Save {
        /// Cache key (used as filename).
        #[arg(long)]
        key: String,
        /// Cargo home directory (default: ~/.cargo or $CARGO_HOME).
        #[arg(long)]
        cargo_home: Option<String>,
    },
    /// Restore cargo registry from a compressed archive.
    Restore {
        /// Cache key to restore.
        #[arg(long)]
        key: String,
        /// Cargo home directory (default: ~/.cargo or $CARGO_HOME).
        #[arg(long)]
        cargo_home: Option<String>,
    },
    /// Print hash of Cargo.lock for use as cache key.
    Hash {
        /// Path to Cargo.lock (default: ./Cargo.lock).
        #[arg(long, default_value = "Cargo.lock")]
        lockfile: String,
    },
    /// Remove cached registry archives.
    Clean,
}

/// `zccache kv` subcommands.
#[derive(Debug, Subcommand)]
enum KvCommands {
    /// Read a value to stdout. Exit 2 if missing.
    Get {
        /// Namespace (`[a-z0-9-]{1,64}`).
        namespace: String,
        /// 64-char hex key.
        hex_key: String,
    },
    /// Write a value. Source is either `--value-from <file>` or stdin.
    Put {
        /// Namespace (`[a-z0-9-]{1,64}`).
        namespace: String,
        /// 64-char hex key.
        hex_key: String,
        /// Read value bytes from this file.
        #[arg(long, conflicts_with = "value_from_stdin")]
        value_from: Option<String>,
        /// Read value bytes from stdin.
        #[arg(long, conflicts_with = "value_from")]
        value_from_stdin: bool,
    },
    /// Remove an entry. Idempotent — missing keys exit 0.
    Rm {
        /// Namespace.
        namespace: String,
        /// 64-char hex key.
        hex_key: String,
    },
    /// List entries under a namespace, sorted by hex key. One row per entry:
    /// `<hex>  <bytes>`.
    Ls {
        /// Namespace.
        namespace: String,
    },
    /// Drop every entry under a namespace.
    Clear {
        /// Namespace.
        namespace: String,
    },
    /// Print total bytes and per-namespace bytes.
    Stats,
}

/// Fingerprint subcommands.
#[derive(Debug, Subcommand)]
enum FpCommands {
    /// Check if files have changed since last success.
    ///
    /// Exit 0 = operation should run (files changed).
    /// Exit 1 = skip (no changes detected).
    Check {
        /// Root directory to scan (default: current directory).
        #[arg(long, default_value = ".")]
        root: String,

        /// File extensions to include (without dot, e.g., "rs", "cpp").
        /// Cannot be used with --include.
        #[arg(long, conflicts_with = "include")]
        ext: Vec<String>,

        /// Glob patterns for files to include (e.g., "**/*.rs").
        /// Cannot be used with --ext.
        #[arg(long, conflicts_with = "ext")]
        include: Vec<String>,

        /// Patterns or directory names to exclude.
        #[arg(long)]
        exclude: Vec<String>,
    },
    /// Mark the previous check as successful.
    #[command(name = "mark-success")]
    MarkSuccess,
    /// Mark the previous check as failed.
    #[command(name = "mark-failure")]
    MarkFailure,
    /// Invalidate the cache (delete all state).
    Invalidate,
}

/// GitHub Actions cache subcommands.
#[derive(Debug, Subcommand)]
enum GhaCacheCommands {
    /// Check if GHA cache API is available (env vars set).
    Status,
    /// Save a directory to the GHA cache (tar+gzip, then upload).
    Save {
        /// Cache key (must be unique per content).
        #[arg(long)]
        key: String,
        /// Path to the directory to cache.
        #[arg(long)]
        path: String,
    },
    /// Restore a directory from the GHA cache.
    Restore {
        /// Cache key to look up.
        #[arg(long)]
        key: String,
        /// Path to restore the directory into.
        #[arg(long)]
        path: String,
    },
}

/// Rust artifact plan subcommands.
#[derive(Debug, Subcommand)]
enum RustPlanCommands {
    /// Validate a soldr-generated Rust artifact plan.
    Validate {
        /// Path to the plan JSON file.
        #[arg(long)]
        plan: String,
        /// Print a machine-readable JSON summary.
        #[arg(long)]
        json: bool,
        /// Active zccache session ID whose compile-cache stats should be included.
        #[arg(long = "session-id")]
        session_id: Option<String>,
        /// IPC endpoint for session stats lookup.
        #[arg(long)]
        endpoint: Option<String>,
        /// Journal/log path to report in the summary, overriding the plan path.
        #[arg(long)]
        journal: Option<String>,
        /// Local cache directory for bundle path/key reporting.
        #[arg(long = "cache-dir")]
        cache_dir: Option<String>,
    },
    /// Restore Rust target artifacts from a saved plan bundle.
    Restore {
        /// Path to the plan JSON file.
        #[arg(long)]
        plan: String,
        /// Print a machine-readable JSON summary.
        #[arg(long)]
        json: bool,
        /// Active zccache session ID whose compile-cache stats should be included.
        #[arg(long = "session-id")]
        session_id: Option<String>,
        /// IPC endpoint for session stats lookup.
        #[arg(long)]
        endpoint: Option<String>,
        /// Journal/log path to report in the summary, overriding the plan path.
        #[arg(long)]
        journal: Option<String>,
        /// Cache backend to use.
        #[arg(long, default_value = "auto")]
        backend: RustPlanBackendArg,
        /// Local cache directory used for bundle storage.
        #[arg(long = "cache-dir")]
        cache_dir: Option<String>,
    },
    /// Save Rust target artifacts selected by a plan.
    Save {
        /// Path to the plan JSON file.
        #[arg(long)]
        plan: String,
        /// Print a machine-readable JSON summary.
        #[arg(long)]
        json: bool,
        /// Active zccache session ID whose compile-cache stats should be included.
        #[arg(long = "session-id")]
        session_id: Option<String>,
        /// IPC endpoint for session stats lookup.
        #[arg(long)]
        endpoint: Option<String>,
        /// Journal/log path to report in the summary, overriding the plan path.
        #[arg(long)]
        journal: Option<String>,
        /// Cache backend to use.
        #[arg(long, default_value = "auto")]
        backend: RustPlanBackendArg,
        /// Local cache directory used for bundle storage.
        #[arg(long = "cache-dir")]
        cache_dir: Option<String>,
    },
}

/// Rust artifact plan backend selection.
#[derive(Debug, Clone, Copy, ValueEnum)]
enum RustPlanBackendArg {
    Auto,
    Local,
    Gha,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RustPlanRuntimeErrorKind {
    Unavailable,
    Failure,
}

#[derive(Debug)]
enum RustPlanRuntimeError {
    Backend {
        backend: RustPlanBackendArg,
        kind: RustPlanRuntimeErrorKind,
        message: String,
    },
}

impl RustPlanRuntimeError {
    fn backend(&self) -> RustPlanBackendArg {
        match self {
            Self::Backend { backend, .. } => *backend,
        }
    }

    fn kind(&self) -> RustPlanRuntimeErrorKind {
        match self {
            Self::Backend { kind, .. } => *kind,
        }
    }

    fn message(&self) -> &str {
        match self {
            Self::Backend { message, .. } => message,
        }
    }

    fn with_kind(self, kind: RustPlanRuntimeErrorKind) -> Self {
        match self {
            Self::Backend {
                backend, message, ..
            } => Self::Backend {
                backend,
                kind,
                message,
            },
        }
    }
}

/// Known subcommand names for auto-detect.
const KNOWN_SUBCOMMANDS: &[&str] = &[
    "start",
    "stop",
    "status",
    "clear",
    "wrap",
    "inspect",
    "session-start",
    "session-end",
    "session-stats",
    "crashes",
    "fp",
    "ino",
    "download",
    "cargo-registry",
    "gha-cache",
    "rust-plan",
    "warm",
    "snapshot-bytes",
    "snapshot-fp-record",
    "snapshot-fp-validate",
    "help",
    "--help",
    "-h",
    "--version",
    "-V",
];

fn absolute_path(path: &str) -> NormalizedPath {
    let path = Path::new(path);
    if path.is_absolute() {
        path.into()
    } else {
        std::env::current_dir()
            .unwrap_or_default()
            .join(path)
            .into()
    }
}

/// Convert an i32 exit code to ExitCode without silent truncation.
/// A bare `exit_code as u8` wraps: 256 → 0 (success), masking failures.
/// This preserves success/failure semantics: non-zero stays non-zero.
fn exit_code_from_i32(code: i32) -> ExitCode {
    let truncated = (code & 0xFF) as u8;
    if code != 0 && truncated == 0 {
        ExitCode::from(1)
    } else {
        ExitCode::from(truncated)
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();

    // Auto-detect: if first arg isn't a known subcommand or a --flag, enter wrap mode.
    // e.g., `zccache clang++ -c foo.cpp -o foo.o`
    match strip_leading_strict_paths_flags(&args[1..]) {
        Ok((strict_paths, wrapper_args))
            if !wrapper_args.is_empty()
                && !KNOWN_SUBCOMMANDS.contains(&wrapper_args[0].as_str())
                && !wrapper_args[0].starts_with("--") =>
        {
            return run_wrap(&wrapper_args, strict_paths);
        }
        Err(err) => {
            eprintln!("zccache: {err}");
            return ExitCode::FAILURE;
        }
        _ => {}
    }

    let cli = Cli::parse();
    let global_strict_paths = cli.strict_paths.clone();

    init_tracing();

    // Handle top-level flags (sccache-compatible)
    if cli.clear {
        let endpoint = resolve_endpoint(None);
        return run_async(cmd_clear(&endpoint));
    }
    if cli.show_stats {
        let endpoint = resolve_endpoint(None);
        return run_async(cmd_status(&endpoint, false));
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

    match command {
        Commands::Start => {
            let endpoint = resolve_endpoint(None);
            run_async(cmd_start(&endpoint))
        }
        Commands::Stop => {
            let endpoint = resolve_endpoint(None);
            run_async(cmd_stop(&endpoint))
        }
        Commands::Status { json } => {
            let endpoint = resolve_endpoint(None);
            run_async(cmd_status(&endpoint, json))
        }
        Commands::Analyze { journal, json } => cmd_analyze(&journal, json),
        Commands::Clear => {
            let endpoint = resolve_endpoint(None);
            run_async(cmd_clear(&endpoint))
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
            GhaCacheCommands::Status => cmd_gha_status(),
            GhaCacheCommands::Save { key, path } => run_async(cmd_gha_save(&key, &path)),
            GhaCacheCommands::Restore { key, path } => run_async(cmd_gha_restore(&key, &path)),
        },
        Commands::RustPlan { action } => run_async(cmd_rust_plan(action)),
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
        } => cmd_download(DownloadParams {
            source: match resolve_download_source(url, part_urls) {
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
            stats,
            journal,
        } => {
            let endpoint = resolve_endpoint(endpoint.as_deref());
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
            run_async(cmd_session_start(
                &endpoint,
                cwd.as_path(),
                log.as_deref(),
                stats,
                journal,
            ))
        }
        Commands::SessionEnd {
            session_id,
            endpoint,
            json,
        } => {
            let endpoint = resolve_endpoint(endpoint.as_deref());
            cmd_session_end(&endpoint, session_id, json)
        }
        Commands::SessionStatsCmd {
            session_id,
            endpoint,
            json,
        } => {
            let endpoint = resolve_endpoint(endpoint.as_deref());
            run_async(cmd_session_stats(&endpoint, session_id, json))
        }
        Commands::Wrap { strict_paths, args } => {
            let strict_paths = match parse_optional_strict_paths(
                strict_paths.as_deref().or(global_strict_paths.as_deref()),
            ) {
                Ok(mode) => mode,
                Err(err) => {
                    eprintln!("zccache: {err}");
                    return ExitCode::FAILURE;
                }
            };
            run_wrap(&args, strict_paths)
        }
        Commands::Inspect { key } => {
            eprintln!("zccache inspect {key}: not yet implemented");
            ExitCode::FAILURE
        }
        Commands::Crashes { clear } => cmd_crashes(clear),
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
                    run_async(cmd_fp_check(
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
                    run_async(cmd_fp_mark(&endpoint, cache_file.as_path(), true))
                }
                FpCommands::MarkFailure => {
                    run_async(cmd_fp_mark(&endpoint, cache_file.as_path(), false))
                }
                FpCommands::Invalidate => {
                    run_async(cmd_fp_invalidate(&endpoint, cache_file.as_path()))
                }
            }
        }
        Commands::CargoRegistry { action } => match action {
            CargoRegistryCommands::Save { key, cargo_home } => {
                cmd_cargo_registry_save(&key, cargo_home.as_deref())
            }
            CargoRegistryCommands::Restore { key, cargo_home } => {
                cmd_cargo_registry_restore(&key, cargo_home.as_deref())
            }
            CargoRegistryCommands::Hash { lockfile } => cmd_cargo_registry_hash(&lockfile),
            CargoRegistryCommands::Clean => cmd_cargo_registry_clean(),
        },
        Commands::Kv { action } => cmd_kv(action),
        Commands::Warm {
            target_dir,
            profile,
            ..
        } => {
            let target_dir = absolute_path(&target_dir);
            cmd_warm(&target_dir, &profile)
        }
        Commands::SnapshotBytes {
            target,
            prune_incremental,
            prune_build_script_out,
        } => cmd_snapshot_bytes(&target, prune_incremental, prune_build_script_out),
        Commands::SnapshotFpRecord {
            target_dir,
            workspace_root,
            profile,
            manifest_path,
        } => cmd_snapshot_fp_record(&target_dir, workspace_root, &profile, manifest_path),
        Commands::SnapshotFpValidate {
            target_dir,
            workspace_root,
            profile,
            manifest_path,
            stamp_seconds_ahead,
        } => cmd_snapshot_fp_validate(
            &target_dir,
            workspace_root,
            &profile,
            manifest_path,
            stamp_seconds_ahead,
        ),
    }
}

// ─── Subcommand implementations ────────────────────────────────────────────

fn cmd_download(params: DownloadParams) -> ExitCode {
    match client_download(None, params) {
        Ok(result) => {
            println!("status={:?}", result.status);
            println!("archive_path={}", result.cache_path.display());
            println!("sha256={}", result.sha256);
            if let Some(unarchive_path) = &result.expanded_path {
                println!("unarchive_path={}", unarchive_path.display());
            }
            if let Some(bytes) = result.bytes {
                println!("bytes={bytes}");
            }
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("zccache download: {err}");
            ExitCode::FAILURE
        }
    }
}

fn resolve_download_source(
    url: Option<String>,
    part_urls: Vec<String>,
) -> Result<DownloadSource, String> {
    match (url, part_urls.is_empty()) {
        (Some(url), true) => Ok(DownloadSource::Url(url)),
        (None, false) => Ok(DownloadSource::MultipartUrls(part_urls)),
        (Some(_), false) => Err("use either --url or --part-url, not both".to_string()),
        (None, true) => Err("provide either --url or at least one --part-url".to_string()),
    }
}

async fn cmd_start(endpoint: &str) -> ExitCode {
    match ensure_daemon(endpoint).await {
        Ok(()) => {
            eprintln!("daemon running at {endpoint}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("failed to start daemon: {e}");
            ExitCode::FAILURE
        }
    }
}

async fn cmd_stop(endpoint: &str) -> ExitCode {
    let mut conn = match connect(endpoint).await {
        Ok(c) => c,
        Err(_) => {
            let Some(pid) = zccache_ipc::check_running_daemon() else {
                eprintln!("daemon not running at {endpoint}");
                // No daemon — but the index file might still be there from a
                // crashed prior run. Probe once so callers (CI tar) can rely
                // on the lock being gone after `zccache stop` returns.
                wait_for_daemon_teardown(endpoint).await;
                return ExitCode::SUCCESS;
            };

            match zccache_ipc::force_kill_process(pid) {
                Ok(()) => {
                    for _ in 0..50 {
                        if !zccache_ipc::is_process_alive(pid) {
                            zccache_ipc::remove_lock_file();
                            eprintln!(
                                "daemon process {pid} terminated after IPC connection failed"
                            );
                            wait_for_daemon_teardown(endpoint).await;
                            return ExitCode::SUCCESS;
                        }
                        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    }
                    eprintln!(
                        "zccache: sent termination to daemon process {pid}, but it did not exit"
                    );
                    return ExitCode::FAILURE;
                }
                Err(e) => {
                    eprintln!(
                        "zccache: cannot connect to daemon at {endpoint}, and failed to kill \
                         locked process {pid}: {e}"
                    );
                    return ExitCode::FAILURE;
                }
            }
        }
    };

    if let Err(e) = conn.send(&zccache_protocol::Request::Shutdown).await {
        eprintln!("zccache: failed to send to daemon: {e}");
        return ExitCode::FAILURE;
    }
    let recv_result = match conn.recv().await {
        Ok(r) => r,
        Err(e) => {
            eprintln!("zccache: broken connection to daemon: {e}");
            return ExitCode::FAILURE;
        }
    };
    match recv_result {
        Some(zccache_protocol::Response::ShuttingDown) => {
            // The daemon acknowledges `Shutdown` immediately and continues
            // teardown asynchronously. On Windows the redb index lock is held
            // until the daemon process actually exits and `Drop` fires. Wait
            // for the IPC endpoint to drop and for `index.redb` to be
            // openable (i.e. no exclusive share lock) so callers like the CI
            // post-step tar do not race the daemon. See issue #182.
            wait_for_daemon_teardown(endpoint).await;
            eprintln!("daemon stopped");
            ExitCode::SUCCESS
        }
        None => {
            eprintln!("zccache: lost connection to daemon (no response received)");
            ExitCode::FAILURE
        }
        Some(other) => {
            eprintln!("zccache: unexpected response from daemon: {other:?}");
            ExitCode::FAILURE
        }
    }
}

/// Default cap on how long `zccache stop` will wait after the daemon ACKs
/// `Shutdown` for the IPC endpoint to disappear and `index.redb` to become
/// openable. Overridable with `ZCCACHE_STOP_TIMEOUT_SECS`.
const STOP_WAIT_DEFAULT_SECS: u64 = 10;
/// Poll cadence inside the bounded wait loop.
const STOP_WAIT_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(100);

/// Returns the bounded total wait duration for `zccache stop`, honoring
/// `ZCCACHE_STOP_TIMEOUT_SECS` if it parses as a non-negative `u64`.
fn stop_wait_timeout() -> std::time::Duration {
    let secs = std::env::var("ZCCACHE_STOP_TIMEOUT_SECS")
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(STOP_WAIT_DEFAULT_SECS);
    std::time::Duration::from_secs(secs)
}

/// Poll until the IPC endpoint is unreachable. Emits a warning on timeout
/// but never fails the caller — the worst case is that the caller (e.g. CI
/// cache tar) sees the same error it would have seen without this wait.
///
/// The legacy redb-era version of this routine also waited for the index
/// file's exclusive share lock to drop on Windows. With the bincode blob
/// there is no file lock — `flush()` writes via temp+rename, holding the
/// file handle only briefly during the rename — so endpoint reachability
/// is the only signal we need.
async fn wait_for_daemon_teardown(endpoint: &str) {
    let deadline = std::time::Instant::now() + stop_wait_timeout();
    loop {
        if !is_ipc_endpoint_reachable(endpoint).await {
            return;
        }
        if std::time::Instant::now() >= deadline {
            eprintln!(
                "zccache: timed out waiting for daemon endpoint to disappear after stop; \
                 continuing anyway. set ZCCACHE_STOP_TIMEOUT_SECS to override."
            );
            return;
        }
        tokio::time::sleep(STOP_WAIT_POLL_INTERVAL).await;
    }
}

/// True if a fresh `connect()` to the daemon IPC endpoint succeeds.
async fn is_ipc_endpoint_reachable(endpoint: &str) -> bool {
    connect(endpoint).await.is_ok()
}

async fn cmd_status(endpoint: &str, json: bool) -> ExitCode {
    let mut conn = match connect(endpoint).await {
        Ok(c) => c,
        Err(e) => {
            let message = format!("daemon not running at {endpoint}: {e}");
            if json {
                print_status_error_json(endpoint, &message);
            } else {
                eprintln!("{message}");
            }
            return ExitCode::FAILURE;
        }
    };

    if let Err(e) = conn.send(&zccache_protocol::Request::Status).await {
        let message = format!("zccache: failed to send to daemon: {e}");
        if json {
            print_status_error_json(endpoint, &message);
        } else {
            eprintln!("{message}");
        }
        return ExitCode::FAILURE;
    }
    let recv_result = match conn.recv().await {
        Ok(r) => r,
        Err(e) => {
            let message = format!("zccache: broken connection to daemon: {e}");
            if json {
                print_status_error_json(endpoint, &message);
            } else {
                eprintln!("{message}");
            }
            return ExitCode::FAILURE;
        }
    };
    match recv_result {
        Some(zccache_protocol::Response::Status(s)) => {
            if json {
                print_status_ok_json(endpoint, &s);
                return ExitCode::SUCCESS;
            }
            let total = s.cache_hits + s.cache_misses;
            let hit_rate = if total > 0 {
                format!("{:.1}%", s.cache_hits as f64 / total as f64 * 100.0)
            } else {
                "n/a".to_string()
            };

            println!(
                "zccache daemon v{} (protocol v{}) ({}) — uptime {}",
                if s.version.is_empty() {
                    "unknown"
                } else {
                    &s.version
                },
                zccache_protocol::PROTOCOL_VERSION,
                endpoint,
                format_uptime(s.uptime_secs)
            );
            if !s.cache_dir.as_os_str().is_empty() {
                println!("cache dir: {}", s.cache_dir.display());
            }
            println!();
            println!(
                "  Compilations:  {} total ({} cached, {} cold, {} non-cacheable)",
                s.total_compilations, s.cache_hits, s.cache_misses, s.non_cacheable
            );
            println!("  Hit rate:      {hit_rate}");
            if s.time_saved_ms > 0 {
                println!("  Time saved:    ~{}", format_duration_ms(s.time_saved_ms));
            }
            if s.compile_errors > 0 {
                println!("  Errors:        {}", s.compile_errors);
            }
            println!();
            println!(
                "  Artifacts:     {} ({})",
                s.artifact_count,
                format_bytes(s.cache_size_bytes)
            );
            {
                let disk_info = if s.dep_graph_disk_size > 0 {
                    format!(
                        "v{}, persisted, {} on disk",
                        s.dep_graph_version,
                        format_bytes(s.dep_graph_disk_size)
                    )
                } else if s.dep_graph_persisted {
                    // Save has flushed at least once, but the file metadata
                    // call lost a race (e.g. rename window) — still persisted.
                    format!("v{}, persisted", s.dep_graph_version)
                } else {
                    format!("v{}, not persisted", s.dep_graph_version)
                };
                println!(
                    "  Dep graph:     {} contexts, {} files ({})",
                    s.dep_graph_contexts, s.dep_graph_files, disk_info
                );
            }
            println!("  Metadata:      {} entries", s.metadata_entries);
            println!();
            if s.total_links > 0 {
                println!();
                let link_total = s.link_hits + s.link_misses;
                let link_hit_rate = if link_total > 0 {
                    format!("{:.1}%", s.link_hits as f64 / link_total as f64 * 100.0)
                } else {
                    "n/a".to_string()
                };
                println!(
                    "  Links:         {} total ({} cached, {} cold, {} non-cacheable)",
                    s.total_links, s.link_hits, s.link_misses, s.link_non_cacheable
                );
                println!("  Link hit rate: {link_hit_rate}");
            }
            println!();
            println!(
                "  Sessions:      {} active / {} total",
                s.sessions_active, s.sessions_total
            );
            ExitCode::SUCCESS
        }
        None => {
            let message = "zccache: lost connection to daemon (no response received)";
            if json {
                print_status_error_json(endpoint, message);
            } else {
                eprintln!("{message}");
            }
            ExitCode::FAILURE
        }
        Some(other) => {
            let message = format!("zccache: unexpected response from daemon: {other:?}");
            if json {
                print_status_error_json(endpoint, &message);
            } else {
                eprintln!("{message}");
            }
            ExitCode::FAILURE
        }
    }
}

fn print_status_ok_json(endpoint: &str, s: &zccache_protocol::DaemonStatus) {
    let total = s.cache_hits + s.cache_misses;
    let hit_rate = if total > 0 {
        Some(s.cache_hits as f64 / total as f64)
    } else {
        None
    };
    let link_total = s.link_hits + s.link_misses;
    let link_hit_rate = if link_total > 0 {
        Some(s.link_hits as f64 / link_total as f64)
    } else {
        None
    };
    let value = serde_json::json!({
        "status": "ok",
        "endpoint": endpoint,
        "protocol_version": zccache_protocol::PROTOCOL_VERSION,
        "hit_rate": hit_rate,
        "link_hit_rate": link_hit_rate,
        "daemon": s,
    });
    print_json_value(&value);
}

fn print_status_error_json(endpoint: &str, message: &str) {
    let value = serde_json::json!({
        "status": "error",
        "endpoint": endpoint,
        "error": message,
    });
    print_json_value(&value);
}

fn cmd_analyze(journal_path: &str, json: bool) -> ExitCode {
    let report = match analyze_journal(journal_path) {
        Ok(report) => report,
        Err(e) => {
            let message = format!("zccache analyze: failed to read {journal_path}: {e}");
            if json {
                let value = serde_json::json!({
                    "status": "error",
                    "journal_path": journal_path,
                    "error": message,
                });
                print_json_value(&value);
            } else {
                eprintln!("{message}");
            }
            return ExitCode::FAILURE;
        }
    };

    if json {
        print_json_value(&report.to_json(journal_path));
    } else {
        report.print_human(journal_path);
    }
    ExitCode::SUCCESS
}

/// Aggregated read-only view of a compile journal.
#[derive(Debug, Default)]
struct AnalyzeReport {
    line_count: u64,
    parsed_count: u64,
    compile_count: u64,
    link_count: u64,
    hit_count: u64,
    miss_count: u64,
    error_count: u64,
    link_hit_count: u64,
    link_miss_count: u64,
    total_latency_ns: u128,
    /// Per-output-extension hit/miss/total-ms counters.
    by_extension: std::collections::BTreeMap<String, ExtensionBucket>,
    /// Per-tool total latency (basename of `compiler`).
    by_tool_total_ns: std::collections::BTreeMap<String, u128>,
    /// Hit counts per tool — useful to see which tools dominate the workload.
    by_tool_calls: std::collections::BTreeMap<String, u64>,
    /// Sorted slowest entries (any outcome). Bounded at 20.
    slowest_entries: Vec<SlowestEntry>,
    /// Per-crate-name miss counts. Bounded by HashMap during accumulation,
    /// surfaced as a sorted top-N in the report.
    miss_crate_counts: std::collections::HashMap<String, u64>,
}

#[derive(Debug, Default)]
struct ExtensionBucket {
    hits: u64,
    misses: u64,
    total_ns: u128,
}

#[derive(Debug, Clone)]
struct SlowestEntry {
    outcome: String,
    crate_name: Option<String>,
    crate_type: Option<String>,
    tool: String,
    latency_ns: u128,
}

#[derive(Debug, Clone)]
struct TopMissCrate {
    crate_name: String,
    misses: u64,
}

impl AnalyzeReport {
    fn ingest(&mut self, line: &serde_json::Value) {
        self.parsed_count += 1;
        let outcome = line
            .get("outcome")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");
        let latency_ns = line
            .get("latency_ns")
            .and_then(|v| v.as_u64())
            .map(u128::from)
            .or_else(|| {
                line.get("latency_ns")
                    .and_then(|v| v.as_f64())
                    .map(|f| f as u128)
            })
            .unwrap_or(0);
        self.total_latency_ns = self.total_latency_ns.saturating_add(latency_ns);

        let args = line
            .get("args")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .collect::<Vec<String>>()
            })
            .unwrap_or_default();
        let compiler = line.get("compiler").and_then(|v| v.as_str()).unwrap_or("");
        let tool = tool_basename(compiler);
        let crate_name = extract_flag_value(&args, "--crate-name");
        let crate_type = extract_flag_value(&args, "--crate-type");
        let extension_bucket = classify_extension(outcome, &crate_type);

        match outcome {
            "hit" => {
                self.compile_count += 1;
                self.hit_count += 1;
                let bucket = self.by_extension.entry(extension_bucket).or_default();
                bucket.hits += 1;
                bucket.total_ns = bucket.total_ns.saturating_add(latency_ns);
            }
            "miss" => {
                self.compile_count += 1;
                self.miss_count += 1;
                let bucket = self.by_extension.entry(extension_bucket).or_default();
                bucket.misses += 1;
                bucket.total_ns = bucket.total_ns.saturating_add(latency_ns);
                if let Some(name) = &crate_name {
                    *self.miss_crate_counts.entry(name.clone()).or_default() += 1;
                }
            }
            "error" => {
                self.compile_count += 1;
                self.error_count += 1;
            }
            "link_hit" => {
                self.link_count += 1;
                self.link_hit_count += 1;
            }
            "link_miss" => {
                self.link_count += 1;
                self.link_miss_count += 1;
            }
            _ => {}
        }

        *self.by_tool_calls.entry(tool.clone()).or_default() += 1;
        let tool_entry = self.by_tool_total_ns.entry(tool.clone()).or_default();
        *tool_entry = tool_entry.saturating_add(latency_ns);

        let entry = SlowestEntry {
            outcome: outcome.to_string(),
            crate_name,
            crate_type,
            tool,
            latency_ns,
        };
        // Maintain a top-20 sorted descending by latency.
        if self.slowest_entries.len() < 20 {
            self.slowest_entries.push(entry);
            self.slowest_entries
                .sort_by(|a, b| b.latency_ns.cmp(&a.latency_ns));
        } else if latency_ns
            > self
                .slowest_entries
                .last()
                .map(|e| e.latency_ns)
                .unwrap_or(0)
        {
            self.slowest_entries.pop();
            self.slowest_entries.push(entry);
            self.slowest_entries
                .sort_by(|a, b| b.latency_ns.cmp(&a.latency_ns));
        }
    }

    fn hit_rate(&self) -> Option<f64> {
        let total = self.hit_count + self.miss_count;
        if total == 0 {
            None
        } else {
            Some(self.hit_count as f64 / total as f64)
        }
    }

    fn top_miss_crates(&self, limit: usize) -> Vec<TopMissCrate> {
        let mut v: Vec<TopMissCrate> = self
            .miss_crate_counts
            .iter()
            .map(|(k, v)| TopMissCrate {
                crate_name: k.clone(),
                misses: *v,
            })
            .collect();
        v.sort_by(|a, b| {
            b.misses
                .cmp(&a.misses)
                .then_with(|| a.crate_name.cmp(&b.crate_name))
        });
        v.truncate(limit);
        v
    }

    fn to_json(&self, journal_path: &str) -> serde_json::Value {
        let by_extension: serde_json::Map<String, serde_json::Value> = self
            .by_extension
            .iter()
            .map(|(ext, bucket)| {
                (
                    ext.clone(),
                    serde_json::json!({
                        "hits": bucket.hits,
                        "misses": bucket.misses,
                        "total_ms": bucket.total_ns / 1_000_000,
                    }),
                )
            })
            .collect();
        let by_tool_total_ms: serde_json::Map<String, serde_json::Value> = self
            .by_tool_total_ns
            .iter()
            .map(|(tool, ns)| {
                (
                    tool.clone(),
                    serde_json::Value::from((ns / 1_000_000) as u64),
                )
            })
            .collect();
        let by_tool_calls: serde_json::Map<String, serde_json::Value> = self
            .by_tool_calls
            .iter()
            .map(|(tool, calls)| (tool.clone(), serde_json::Value::from(*calls)))
            .collect();
        let slowest = self
            .slowest_entries
            .iter()
            .map(|e| {
                serde_json::json!({
                    "outcome": e.outcome,
                    "crate_name": e.crate_name,
                    "crate_type": e.crate_type,
                    "tool": e.tool,
                    "ms": e.latency_ns / 1_000_000,
                })
            })
            .collect::<Vec<_>>();
        let top_miss_crates = self
            .top_miss_crates(10)
            .into_iter()
            .map(|c| {
                serde_json::json!({
                    "crate_name": c.crate_name,
                    "misses": c.misses,
                })
            })
            .collect::<Vec<_>>();
        serde_json::json!({
            "status": "ok",
            "schema_version": 1,
            "journal_path": journal_path,
            "line_count": self.line_count,
            "parsed_count": self.parsed_count,
            "compile_count": self.compile_count,
            "link_count": self.link_count,
            "hit_count": self.hit_count,
            "miss_count": self.miss_count,
            "error_count": self.error_count,
            "link_hit_count": self.link_hit_count,
            "link_miss_count": self.link_miss_count,
            "hit_rate": self.hit_rate(),
            "total_latency_ms": (self.total_latency_ns / 1_000_000) as u64,
            "by_extension": by_extension,
            "by_tool_total_ms": by_tool_total_ms,
            "by_tool_calls": by_tool_calls,
            "top_slowest": slowest,
            "top_miss_crates": top_miss_crates,
        })
    }

    fn print_human(&self, journal_path: &str) {
        println!("zccache analyze: {journal_path}");
        println!(
            "  lines: {} parsed; compiles: {} (hits {} / misses {} / errors {}); links: {} (hits {} / misses {})",
            self.parsed_count,
            self.compile_count,
            self.hit_count,
            self.miss_count,
            self.error_count,
            self.link_count,
            self.link_hit_count,
            self.link_miss_count,
        );
        if let Some(rate) = self.hit_rate() {
            println!("  hit rate: {:.1}%", rate * 100.0);
        } else {
            println!("  hit rate: n/a");
        }
        println!(
            "  total wall-clock: {} ms",
            self.total_latency_ns / 1_000_000
        );
        if !self.by_extension.is_empty() {
            println!();
            println!("  by extension:");
            for (ext, bucket) in &self.by_extension {
                println!(
                    "    {ext:<14}  hits={:>6}  misses={:>6}  ms={}",
                    bucket.hits,
                    bucket.misses,
                    bucket.total_ns / 1_000_000
                );
            }
        }
        if !self.by_tool_total_ns.is_empty() {
            println!();
            println!("  by tool (wall-clock ms):");
            let mut sorted: Vec<_> = self.by_tool_total_ns.iter().collect();
            sorted.sort_by(|a, b| b.1.cmp(a.1));
            for (tool, ns) in sorted.iter().take(10) {
                let calls = self.by_tool_calls.get(*tool).copied().unwrap_or(0);
                println!("    {tool:<24}  ms={:>9}  calls={calls}", *ns / 1_000_000);
            }
        }
        let top_miss = self.top_miss_crates(10);
        if !top_miss.is_empty() {
            println!();
            println!("  top miss crates:");
            for c in &top_miss {
                println!("    {:<32}  misses={}", c.crate_name, c.misses);
            }
        }
        if !self.slowest_entries.is_empty() {
            println!();
            println!("  slowest entries (top {}):", self.slowest_entries.len());
            for e in &self.slowest_entries {
                let crate_label = e
                    .crate_name
                    .as_deref()
                    .unwrap_or_else(|| e.crate_type.as_deref().unwrap_or("?"));
                println!(
                    "    {:<10} {:<24}  ms={}  tool={}",
                    e.outcome,
                    crate_label,
                    e.latency_ns / 1_000_000,
                    e.tool
                );
            }
        }
    }
}

fn analyze_journal(journal_path: &str) -> Result<AnalyzeReport, std::io::Error> {
    use std::io::{BufRead, BufReader};
    let file = std::fs::File::open(journal_path)?;
    let reader = BufReader::new(file);
    let mut report = AnalyzeReport::default();
    for line in reader.lines() {
        let line = line?;
        report.line_count += 1;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        // Permissive parse: skip malformed lines rather than fail the run.
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed) {
            report.ingest(&value);
        }
    }
    Ok(report)
}

fn tool_basename(compiler: &str) -> String {
    // Split on both separators so Windows-style paths round-trip on Unix
    // (where std::path doesn't recognize `\` as a component boundary).
    let last_component = compiler.rsplit(['/', '\\']).next().unwrap_or(compiler);
    let stem = last_component
        .rsplit_once('.')
        .map(|(stem, _ext)| stem)
        .filter(|s| !s.is_empty())
        .unwrap_or(last_component);
    stem.to_string()
}

fn extract_flag_value(args: &[String], flag: &str) -> Option<String> {
    let prefix = format!("{flag}=");
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if arg == flag {
            return iter.next().cloned();
        }
        if let Some(rest) = arg.strip_prefix(&prefix) {
            return Some(rest.to_string());
        }
    }
    None
}

fn classify_extension(outcome: &str, crate_type: &Option<String>) -> String {
    // Links are not parameterized by --crate-type in the rustc sense; bucket
    // them separately so the rollup can distinguish linker work from compile
    // work even when the per-compile classification is unknown.
    if outcome == "link_hit" || outcome == "link_miss" {
        return "link".to_string();
    }
    match crate_type.as_deref() {
        Some("bin") => "bin".to_string(),
        Some("lib") | Some("rlib") => "rlib".to_string(),
        Some("dylib") => "dylib".to_string(),
        Some("cdylib") => "cdylib".to_string(),
        Some("staticlib") => "staticlib".to_string(),
        Some("proc-macro") => "proc-macro".to_string(),
        Some(other) => other.to_string(),
        None => "unknown".to_string(),
    }
}

async fn cmd_clear(endpoint: &str) -> ExitCode {
    let mut conn = match connect(endpoint).await {
        Ok(c) => c,
        Err(_) => {
            eprintln!("daemon not running at {endpoint} — nothing to clear");
            return ExitCode::SUCCESS;
        }
    };

    if let Err(e) = conn.send(&zccache_protocol::Request::Clear).await {
        eprintln!("zccache: failed to send to daemon: {e}");
        return ExitCode::FAILURE;
    }
    let recv_result = match conn.recv().await {
        Ok(r) => r,
        Err(e) => {
            eprintln!("zccache: broken connection to daemon: {e}");
            return ExitCode::FAILURE;
        }
    };
    match recv_result {
        Some(zccache_protocol::Response::Cleared {
            artifacts_removed,
            metadata_cleared,
            dep_graph_contexts_cleared,
            on_disk_bytes_freed,
        }) => {
            println!("Cache cleared:");
            println!("  Artifacts removed:  {artifacts_removed}");
            println!("  Metadata cleared:   {metadata_cleared}");
            println!("  Dep graph contexts: {dep_graph_contexts_cleared}");
            if on_disk_bytes_freed > 0 {
                println!(
                    "  Disk freed:         {}",
                    format_bytes(on_disk_bytes_freed)
                );
            }
            ExitCode::SUCCESS
        }
        None => {
            eprintln!("zccache: lost connection to daemon (no response received)");
            ExitCode::FAILURE
        }
        Some(other) => {
            eprintln!("zccache: unexpected response from daemon: {other:?}");
            ExitCode::FAILURE
        }
    }
}

fn cmd_kv(action: KvCommands) -> ExitCode {
    use std::io::{Read, Write};
    use zccache_artifact::{Key, KvError, KvStore};

    fn open_store() -> Result<KvStore, ExitCode> {
        match KvStore::open_default() {
            Ok(s) => Ok(s),
            Err(e) => {
                eprintln!("zccache kv: open: {e}");
                Err(ExitCode::FAILURE)
            }
        }
    }

    fn parse_key(hex: &str) -> Result<Key, ExitCode> {
        Key::from_hex(hex).map_err(|e| {
            eprintln!("zccache kv: bad key: {e}");
            ExitCode::FAILURE
        })
    }

    match action {
        KvCommands::Get { namespace, hex_key } => {
            let store = match open_store() {
                Ok(s) => s,
                Err(c) => return c,
            };
            let key = match parse_key(&hex_key) {
                Ok(k) => k,
                Err(c) => return c,
            };
            match store.get(&namespace, &key) {
                Ok(Some(bytes)) => {
                    let stdout = std::io::stdout();
                    let mut handle = stdout.lock();
                    if let Err(e) = handle.write_all(&bytes) {
                        eprintln!("zccache kv get: write stdout: {e}");
                        return ExitCode::FAILURE;
                    }
                    ExitCode::SUCCESS
                }
                Ok(None) => ExitCode::from(2),
                Err(e) => {
                    eprintln!("zccache kv get: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        KvCommands::Put {
            namespace,
            hex_key,
            value_from,
            value_from_stdin,
        } => {
            let store = match open_store() {
                Ok(s) => s,
                Err(c) => return c,
            };
            let key = match parse_key(&hex_key) {
                Ok(k) => k,
                Err(c) => return c,
            };
            let bytes = if let Some(path) = value_from {
                match std::fs::read(&path) {
                    Ok(b) => b,
                    Err(e) => {
                        eprintln!("zccache kv put: read {path}: {e}");
                        return ExitCode::FAILURE;
                    }
                }
            } else if value_from_stdin {
                let mut buf = Vec::new();
                if let Err(e) = std::io::stdin().read_to_end(&mut buf) {
                    eprintln!("zccache kv put: read stdin: {e}");
                    return ExitCode::FAILURE;
                }
                buf
            } else {
                eprintln!("zccache kv put: must specify --value-from <file> or --value-from-stdin");
                return ExitCode::FAILURE;
            };
            match store.put(&namespace, &key, &bytes) {
                Ok(_) => ExitCode::SUCCESS,
                Err(KvError::TooLarge(n, m)) => {
                    eprintln!("zccache kv put: value too large: {n} bytes (max {m})");
                    ExitCode::FAILURE
                }
                Err(e) => {
                    eprintln!("zccache kv put: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        KvCommands::Rm { namespace, hex_key } => {
            let store = match open_store() {
                Ok(s) => s,
                Err(c) => return c,
            };
            let key = match parse_key(&hex_key) {
                Ok(k) => k,
                Err(c) => return c,
            };
            match store.remove(&namespace, &key) {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("zccache kv rm: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        KvCommands::Ls { namespace } => {
            let store = match open_store() {
                Ok(s) => s,
                Err(c) => return c,
            };
            match store.list_namespace(&namespace) {
                Ok(entries) => {
                    for (k, len) in entries {
                        println!("{}  {}", k.to_hex(), len);
                    }
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("zccache kv ls: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        KvCommands::Clear { namespace } => {
            let store = match open_store() {
                Ok(s) => s,
                Err(c) => return c,
            };
            match store.clear_namespace(&namespace) {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("zccache kv clear: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        KvCommands::Stats => {
            let store = match open_store() {
                Ok(s) => s,
                Err(c) => return c,
            };
            let total = match store.total_bytes() {
                Ok(n) => n,
                Err(e) => {
                    eprintln!("zccache kv stats: {e}");
                    return ExitCode::FAILURE;
                }
            };
            let by_ns = match store.stats() {
                Ok(v) => v,
                Err(e) => {
                    eprintln!("zccache kv stats: {e}");
                    return ExitCode::FAILURE;
                }
            };
            println!("total_bytes  {total}");
            for (ns, bytes) in by_ns {
                println!("{ns}  {bytes}");
            }
            ExitCode::SUCCESS
        }
    }
}

fn cmd_warm(target_dir: &Path, profile: &str) -> ExitCode {
    let cache_dir = zccache_core::config::default_cache_dir();
    let index_path = zccache_core::config::index_path_from_cache_dir(&cache_dir);
    let artifact_dir = cache_dir.join("artifacts");
    // Look for Cargo.lock in cwd or next to target_dir
    let lockfile = {
        let cwd = Path::new("Cargo.lock");
        let parent = target_dir.parent().map(|p| p.join("Cargo.lock"));
        if cwd.exists() {
            Some(cwd.to_path_buf())
        } else if let Some(ref p) = parent {
            if p.exists() {
                Some(p.clone())
            } else {
                None
            }
        } else {
            None
        }
    };
    match warm_target(
        index_path.as_ref(),
        artifact_dir.as_ref(),
        target_dir,
        profile,
        lockfile.as_deref(),
    ) {
        Ok((restored, skipped, errors)) => {
            println!("zccache warm: restored {restored} files, skipped {skipped}, errors {errors}");
            if errors > 0 {
                ExitCode::FAILURE
            } else {
                ExitCode::SUCCESS
            }
        }
        Err(e) => {
            eprintln!("zccache warm: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Parse crate names from a Cargo.lock file.
/// Returns a set of crate names with hyphens converted to underscores
/// (matching how cargo names output files).
fn parse_lockfile_crates(lockfile: &Path) -> Result<std::collections::HashSet<String>, String> {
    let content = std::fs::read_to_string(lockfile)
        .map_err(|e| format!("failed to read {}: {e}", lockfile.display()))?;
    let mut crates = std::collections::HashSet::new();
    for line in content.lines() {
        // Cargo.lock format: name = "crate-name"
        if let Some(name) = line.strip_prefix("name = \"") {
            if let Some(name) = name.strip_suffix('"') {
                // Cargo converts hyphens to underscores in output filenames
                crates.insert(name.replace('-', "_"));
            }
        }
    }
    Ok(crates)
}

/// Check if an output filename matches any crate in the allowed set.
/// Output filenames look like: libserde-abc123.rlib, serde-abc123.d,
/// libproc_macro2-def456.so, etc.
fn artifact_matches_lockfile(
    filename: &str,
    allowed_crates: &std::collections::HashSet<String>,
) -> bool {
    // Strip lib prefix if present
    let name = filename.strip_prefix("lib").unwrap_or(filename);
    // Find the hash separator: last hyphen followed by hex chars
    // e.g., "serde-abc123.rlib" → crate name is "serde"
    // e.g., "proc_macro2-def456.rmeta" → crate name is "proc_macro2"
    // Walk from the end to find the hash suffix
    if let Some(pos) = name.rfind('-') {
        let crate_name = &name[..pos];
        allowed_crates.contains(crate_name)
    } else {
        // No hash separator — might be a build script or other file, allow it
        true
    }
}

/// Core logic for `zccache warm` — testable with custom paths.
/// If lockfile is Some, only restores artifacts matching crates in the lockfile.
fn warm_target(
    index_path: &Path,
    artifact_dir: &Path,
    target_dir: &Path,
    profile: &str,
    lockfile: Option<&Path>,
) -> Result<(u64, u64, u64), String> {
    if !index_path.exists() {
        return Err(format!("no artifact index at {}", index_path.display()));
    }

    let store = zccache_artifact::ArtifactStore::open(index_path)
        .map_err(|e| format!("failed to open artifact index: {e}"))?;

    let all_entries = store.load_all();

    if all_entries.is_empty() {
        return Err("no cached artifacts found in index".to_string());
    }

    // If we have a lockfile, only restore artifacts matching its crates
    let allowed_crates = match lockfile {
        Some(lf) => Some(parse_lockfile_crates(lf)?),
        None => None,
    };

    let artifacts = all_entries;

    let deps_dir = target_dir.join(profile).join("deps");
    std::fs::create_dir_all(&deps_dir)
        .map_err(|e| format!("failed to create {}: {e}", deps_dir.display()))?;
    let now = std::time::SystemTime::now();
    let file_times = std::fs::FileTimes::new()
        .set_accessed(now)
        .set_modified(now);

    let mut restored = 0u64;
    let mut skipped = 0u64;
    let mut errors = 0u64;

    for (key_hex, idx) in &artifacts {
        for (i, name) in idx.output_names.iter().enumerate() {
            let src = artifact_dir.join(format!("{key_hex}_{i}"));
            let dst = deps_dir.join(name.as_str());

            // Skip if artifact doesn't match any crate in the lockfile
            if let Some(ref allowed) = allowed_crates {
                if !artifact_matches_lockfile(name, allowed) {
                    skipped += 1;
                    continue;
                }
            }

            // Skip if source payload does not exist on disk.
            if !src.exists() {
                skipped += 1;
                continue;
            }

            // Remove existing file at destination (hardlink will fail if it exists).
            if dst.exists() {
                if let Err(e) = std::fs::remove_file(&dst) {
                    eprintln!(
                        "zccache warm: failed to remove existing {}: {e}",
                        dst.display()
                    );
                    errors += 1;
                    continue;
                }
            }

            // Try hardlink first, fall back to copy.
            let linked = std::fs::hard_link(&src, &dst).is_ok();
            if !linked {
                if let Err(e) = std::fs::copy(&src, &dst) {
                    eprintln!(
                        "zccache warm: failed to copy {} -> {}: {e}",
                        src.display(),
                        dst.display()
                    );
                    errors += 1;
                    continue;
                }
            }

            // Touch mtime to current time so cargo sees the file as fresh.
            if let Ok(f) = std::fs::File::open(&dst) {
                let _ = f.set_times(file_times);
            }

            restored += 1;
        }
    }

    Ok((restored, skipped, errors))
}

async fn cmd_session_start(
    endpoint: &str,
    cwd: &Path,
    log: Option<&Path>,
    track_stats: bool,
    journal: Option<NormalizedPath>,
) -> ExitCode {
    if let Err(e) = ensure_daemon(endpoint).await {
        eprintln!("cannot start daemon at {endpoint}: {e}");
        return ExitCode::FAILURE;
    }

    let mut conn = match connect(endpoint).await {
        Ok(c) => c,
        Err(e) => {
            eprintln!("cannot connect to daemon at {endpoint}: {e}");
            return ExitCode::FAILURE;
        }
    };

    if let Err(e) = conn
        .send(&zccache_protocol::Request::SessionStart {
            client_pid: std::process::id(),
            working_dir: cwd.into(),
            log_file: log.map(NormalizedPath::from),
            track_stats,
            journal_path: journal,
        })
        .await
    {
        eprintln!("zccache: failed to send to daemon: {e}");
        return ExitCode::FAILURE;
    }

    let recv_result = match conn.recv().await {
        Ok(r) => r,
        Err(e) => {
            eprintln!("zccache: broken connection to daemon: {e}");
            return ExitCode::FAILURE;
        }
    };
    match recv_result {
        Some(zccache_protocol::Response::SessionStarted {
            session_id,
            journal_path,
        }) => {
            // One-line JSON so scripts can parse both the session ID and start time:
            //   result=$(zccache session-start)
            let started_at = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            if let Some(ref jp) = journal_path {
                // Escape backslashes for valid JSON (Windows paths contain `\`)
                let jp_escaped = jp.display().to_string().replace('\\', "\\\\");
                println!(
                    r#"{{"session_id":"{}","started_at":{},"journal_path":"{}"}}"#,
                    session_id, started_at, jp_escaped
                );
            } else {
                println!(
                    r#"{{"session_id":"{}","started_at":{}}}"#,
                    session_id, started_at
                );
            }
            ExitCode::SUCCESS
        }
        Some(zccache_protocol::Response::Error { message }) => {
            eprintln!("session-start failed: {message}");
            ExitCode::FAILURE
        }
        None => {
            eprintln!("zccache: lost connection to daemon (no response received)");
            ExitCode::FAILURE
        }
        Some(other) => {
            eprintln!("zccache: unexpected response from daemon: {other:?}");
            ExitCode::FAILURE
        }
    }
}

fn cmd_session_end(endpoint: &str, session_id: String, json: bool) -> ExitCode {
    // Thin wrapper around the shared library entry point. All daemon
    // callers (CLI, soldr, future tools) must agree on what "the daemon
    // is gone" means — see `session_end_idempotent` for the contract
    // and issue #159 for why this lives in the library.
    match session_end_idempotent(endpoint, &session_id) {
        Ok(Some(s)) => {
            if json {
                print_session_stats_json(&session_id, &s);
            } else {
                print_session_stats_human(&session_id, &s, "complete");
            }
            ExitCode::SUCCESS
        }
        // `Ok(None)` covers both:
        //   - daemon was unreachable (already logged by the library), and
        //   - daemon was reached but had no stats for this session.
        // Both are no-op successes.
        Ok(None) => {
            if json {
                print_session_stats_unavailable_json(&session_id, "stats_unavailable");
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            if json {
                print_session_stats_error_json(&session_id, &e.to_string());
            } else {
                eprintln!("zccache: session-end failed: {e}");
            }
            ExitCode::FAILURE
        }
    }
}

async fn cmd_session_stats(endpoint: &str, session_id: String, json: bool) -> ExitCode {
    let mut conn = match connect(endpoint).await {
        Ok(c) => c,
        Err(e) => {
            let message = format!("cannot connect to daemon at {endpoint}: {e}");
            if json {
                print_session_stats_error_json(&session_id, &message);
            } else {
                eprintln!("{message}");
            }
            return ExitCode::FAILURE;
        }
    };

    if let Err(e) = conn
        .send(&zccache_protocol::Request::SessionStats {
            session_id: session_id.clone(),
        })
        .await
    {
        let message = format!("zccache: failed to send to daemon: {e}");
        if json {
            print_session_stats_error_json(&session_id, &message);
        } else {
            eprintln!("{message}");
        }
        return ExitCode::FAILURE;
    }

    let recv_result = match conn.recv().await {
        Ok(r) => r,
        Err(e) => {
            let message = format!("zccache: broken connection to daemon: {e}");
            if json {
                print_session_stats_error_json(&session_id, &message);
            } else {
                eprintln!("{message}");
            }
            return ExitCode::FAILURE;
        }
    };
    match recv_result {
        Some(zccache_protocol::Response::SessionStatsResult { stats }) => {
            if let Some(s) = stats {
                if json {
                    print_session_stats_json(&session_id, &s);
                } else {
                    print_session_stats_human(&session_id, &s, "active");
                }
            } else if json {
                print_session_stats_unavailable_json(&session_id, "stats_not_enabled");
            } else {
                eprintln!("Session {session_id}: stats tracking not enabled");
            }
            ExitCode::SUCCESS
        }
        Some(zccache_protocol::Response::Error { message }) => {
            if json {
                print_session_stats_error_json(&session_id, &message);
            } else {
                eprintln!("session-stats failed: {message}");
            }
            ExitCode::FAILURE
        }
        None => {
            let message = "zccache: lost connection to daemon (no response received)";
            if json {
                print_session_stats_error_json(&session_id, message);
            } else {
                eprintln!("{message}");
            }
            ExitCode::FAILURE
        }
        Some(other) => {
            let message = format!("zccache: unexpected response from daemon: {other:?}");
            if json {
                print_session_stats_error_json(&session_id, &message);
            } else {
                eprintln!("{message}");
            }
            ExitCode::FAILURE
        }
    }
}

fn print_session_stats_human(
    session_id: &str,
    stats: &zccache_protocol::SessionStats,
    state: &str,
) {
    let total = stats.hits + stats.misses;
    let hit_rate = if total > 0 {
        format!("{:.1}%", stats.hits as f64 / total as f64 * 100.0)
    } else {
        "n/a".to_string()
    };
    let label = if state == "active" {
        format!(
            "Session {session_id} (active, {})",
            format_duration_ms(stats.duration_ms)
        )
    } else {
        format!(
            "Session {session_id} {state} ({})",
            format_duration_ms(stats.duration_ms)
        )
    };
    eprintln!("{label}");
    eprintln!(
        "  {} compilations: {} hits, {} misses, {} non-cacheable",
        stats.compilations, stats.hits, stats.misses, stats.non_cacheable
    );
    eprintln!("  Hit rate: {hit_rate}");
    if stats.time_saved_ms > 0 {
        eprintln!("  Time saved: ~{}", format_duration_ms(stats.time_saved_ms));
    }
}

fn print_session_stats_json(session_id: &str, stats: &zccache_protocol::SessionStats) {
    print_json_value(&session_stats_json(session_id, stats));
}

fn print_session_stats_unavailable_json(session_id: &str, reason: &str) {
    print_json_value(&session_stats_unavailable_json(session_id, reason));
}

fn print_session_stats_error_json(session_id: &str, error: &str) {
    print_json_value(&session_stats_error_json(session_id, error));
}

fn session_stats_unavailable_json(session_id: &str, reason: &str) -> serde_json::Value {
    serde_json::json!({
        "status": "unavailable",
        "session_id": session_id,
        "reason": reason,
    })
}

fn session_stats_error_json(session_id: &str, error: &str) -> serde_json::Value {
    serde_json::json!({
        "status": "error",
        "session_id": session_id,
        "error": error,
    })
}

fn print_json_value(value: &serde_json::Value) {
    match serde_json::to_string_pretty(value) {
        Ok(s) => println!("{s}"),
        Err(err) => {
            eprintln!("zccache: failed to encode JSON output: {err}");
            println!(r#"{{"status":"error","error":"failed to encode JSON output"}}"#);
        }
    }
}

fn cmd_crashes(clear: bool) -> ExitCode {
    let crash_dir = zccache_core::config::crash_dump_dir();

    if clear {
        let count = match std::fs::read_dir(&crash_dir) {
            Ok(entries) => {
                let mut n = 0u64;
                for entry in entries.flatten() {
                    if std::fs::remove_file(entry.path()).is_ok() {
                        n += 1;
                    }
                }
                n
            }
            Err(_) => 0,
        };
        println!("Deleted {count} crash dump(s).");
        return ExitCode::SUCCESS;
    }

    let mut dumps: Vec<_> = match std::fs::read_dir(&crash_dir) {
        Ok(entries) => entries
            .flatten()
            .filter(|e| e.path().extension().is_some_and(|ext| ext == "txt"))
            .collect(),
        Err(_) => {
            println!("No crash dumps found.");
            return ExitCode::SUCCESS;
        }
    };

    if dumps.is_empty() {
        println!("No crash dumps found.");
        return ExitCode::SUCCESS;
    }

    dumps.sort_by_key(|e| e.file_name());

    println!("Crash dumps ({}):", dumps.len());
    println!();
    for entry in &dumps {
        let path = entry.path();
        println!("  {}", path.display());
        if let Ok(content) = std::fs::read_to_string(&path) {
            for (i, line) in content.lines().enumerate() {
                if i >= 5 {
                    println!("    ...");
                    break;
                }
                println!("    {line}");
            }
            println!();
        }
    }

    ExitCode::SUCCESS
}

// ─── Cargo registry subcommands ──────────────────────────────────────────

/// Resolve the cargo home directory from an explicit argument, the `CARGO_HOME`
/// env var, or the default `~/.cargo`.
fn resolve_cargo_home(explicit: Option<&str>) -> Result<NormalizedPath, String> {
    if let Some(p) = explicit {
        return Ok(NormalizedPath::from(p));
    }
    if let Ok(ch) = std::env::var("CARGO_HOME") {
        if !ch.is_empty() {
            return Ok(NormalizedPath::from(ch));
        }
    }
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .map_err(|_| "cannot determine home directory (set HOME or CARGO_HOME)".to_string())?;
    Ok(NormalizedPath::from(home).join(".cargo"))
}

/// Directory where cargo-registry archives are stored.
fn cargo_registry_cache_dir() -> Result<NormalizedPath, String> {
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .map_err(|_| "cannot determine home directory (set HOME)".to_string())?;
    Ok(NormalizedPath::from(home)
        .join(".zccache")
        .join("cargo-registry"))
}

/// Matches setup-soldr's boolean env-var normalization: `1`, `true`, `yes`,
/// `on` (case-insensitive) are truthy; anything else (including `None`,
/// empty, `0`, `false`, `no`, `off`) is falsy. See zccache#184.
fn flag_truthy(value: Option<&str>) -> bool {
    let Some(raw) = value else { return false };
    let trimmed = raw.trim();
    matches!(trimmed, "1")
        || trimmed.eq_ignore_ascii_case("true")
        || trimmed.eq_ignore_ascii_case("yes")
        || trimmed.eq_ignore_ascii_case("on")
}

fn env_flag_truthy(name: &str) -> bool {
    flag_truthy(std::env::var(name).ok().as_deref())
}

/// Parallel walk of `target` summing the bytes of every regular file, with
/// optional pruning. Uses jwalk for parallel readdir + stat (rayon under the
/// hood) — on Windows this hides per-file Defender callback latency that
/// dominates the single-threaded `os.walk` baseline. See zccache#189.
fn cmd_snapshot_bytes(
    target: &Path,
    prune_incremental: bool,
    prune_build_script_out: bool,
) -> ExitCode {
    match snapshot_bytes_walk(target, prune_incremental, prune_build_script_out) {
        Ok(total) => {
            println!("{total}");
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("zccache snapshot-bytes: {err}");
            ExitCode::FAILURE
        }
    }
}

fn cmd_snapshot_fp_record(
    target_dir: &Path,
    workspace_root: Option<PathBuf>,
    profile: &str,
    manifest_path: Option<PathBuf>,
) -> ExitCode {
    let workspace = workspace_root.unwrap_or_else(|| std::env::current_dir().expect("cwd"));
    let manifest = manifest_path.unwrap_or_else(|| target_dir.join(snapshot_fp::MANIFEST_FILENAME));
    match snapshot_fp::record(target_dir, &workspace, &manifest, profile) {
        Ok(stats) => {
            eprintln!(
                "zccache snapshot-fp-record: wrote {} ({} crates, {} sources)",
                manifest.display(),
                stats.crates_recorded,
                stats.sources_hashed,
            );
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("zccache snapshot-fp-record: {e}");
            ExitCode::FAILURE
        }
    }
}

fn cmd_snapshot_fp_validate(
    target_dir: &Path,
    workspace_root: Option<PathBuf>,
    profile: &str,
    manifest_path: Option<PathBuf>,
    stamp_seconds_ahead: u64,
) -> ExitCode {
    let workspace = workspace_root.unwrap_or_else(|| std::env::current_dir().expect("cwd"));
    let manifest = manifest_path.unwrap_or_else(|| target_dir.join(snapshot_fp::MANIFEST_FILENAME));
    match snapshot_fp::validate(
        target_dir,
        &workspace,
        &manifest,
        profile,
        stamp_seconds_ahead,
    ) {
        Ok(stats) => {
            eprintln!(
                "zccache snapshot-fp-validate: {} clean / {} dirty",
                stats.crates_clean,
                stats.crates_dirty(),
            );
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("zccache snapshot-fp-validate: {e}");
            ExitCode::FAILURE
        }
    }
}

fn snapshot_bytes_walk(
    target: &Path,
    prune_incremental: bool,
    prune_build_script_out: bool,
) -> std::io::Result<u64> {
    use jwalk::WalkDirGeneric;
    use std::sync::Mutex;

    if !target.exists() {
        return Ok(0);
    }

    // Dedup by (dev, inode) so hardlinked files don't double-count.
    let seen: Mutex<std::collections::HashSet<(u64, u64)>> = Mutex::new(Default::default());

    let walker = WalkDirGeneric::<((), Option<u64>)>::new(target).process_read_dir(
        move |_depth, parent_path, _read_dir_state, children| {
            for child in children.iter_mut() {
                let Ok(entry) = child.as_mut() else { continue };
                if !entry.file_type().is_dir() {
                    continue;
                }
                let name = entry.file_name().to_string_lossy().into_owned();
                if prune_incremental && name == "incremental" {
                    entry.read_children_path = None;
                    continue;
                }
                if prune_build_script_out && name == "out" {
                    // `*/build/*/out` — only prune if grandparent is `build`.
                    if let Some(grandparent) = parent_path.parent() {
                        if grandparent.file_name().and_then(|s| s.to_str()) == Some("build") {
                            entry.read_children_path = None;
                        }
                    }
                }
            }
        },
    );

    let mut total: u64 = 0;
    for entry in walker {
        let entry = match entry {
            Ok(e) => e,
            Err(err) => {
                // Tolerate per-entry stat failures the same way `os.walk` does
                // in the bash fallback: skip and continue. We only bail on
                // catastrophic root-level failure (handled by walker init).
                eprintln!("zccache snapshot-bytes: skip entry: {err}");
                continue;
            }
        };
        if !entry.file_type().is_file() {
            continue;
        }
        let meta = match entry.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };
        if let Some(key) = file_identity(&meta) {
            let mut seen_guard = seen.lock().expect("seen mutex poisoned");
            if !seen_guard.insert(key) {
                continue;
            }
        }
        total = total.saturating_add(meta.len());
    }
    Ok(total)
}

#[cfg(unix)]
fn file_identity(meta: &std::fs::Metadata) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    Some((meta.dev(), meta.ino()))
}

#[cfg(windows)]
fn file_identity(_meta: &std::fs::Metadata) -> Option<(u64, u64)> {
    // Windows file IDs require a separate Win32 call; not worth the cost just
    // for hardlink dedup in a target/ tree. Cargo doesn't hardlink within
    // `target/` in practice, so the dedup is a no-op here.
    None
}

#[cfg(not(any(unix, windows)))]
fn file_identity(_meta: &std::fs::Metadata) -> Option<(u64, u64)> {
    None
}

fn cmd_cargo_registry_save(key: &str, cargo_home: Option<&str>) -> ExitCode {
    // setup-soldr#70's payload C migration: when setup-soldr owns
    // `~/.cargo/registry` caching with fast-zstd, double-saving here just
    // burns CPU on the same bytes. Caller signals takeover via env var.
    if env_flag_truthy("SOLDR_SKIP_CARGO_REGISTRY_SAVE") {
        println!(
            "cargo-registry save: skipping (SOLDR_SKIP_CARGO_REGISTRY_SAVE=1) \
             — caller owns the cache layer"
        );
        return ExitCode::SUCCESS;
    }
    let cargo_home = match resolve_cargo_home(cargo_home) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("zccache cargo-registry save: {e}");
            return ExitCode::FAILURE;
        }
    };
    let cache_dir = match cargo_registry_cache_dir() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("zccache cargo-registry save: {e}");
            return ExitCode::FAILURE;
        }
    };
    if let Err(e) = std::fs::create_dir_all(&cache_dir) {
        eprintln!(
            "zccache cargo-registry save: failed to create {}: {e}",
            cache_dir.display()
        );
        return ExitCode::FAILURE;
    }
    let archive_path = cache_dir.join(format!("{key}.tar.gz"));

    // Collect paths to archive.
    let subdirs: &[&str] = &["registry/index", "registry/cache", "git/db"];
    let mut paths: Vec<(NormalizedPath, String)> = Vec::new();
    for subdir in subdirs {
        let p = cargo_home.join(subdir);
        if p.exists() {
            paths.push((p, subdir.to_string()));
        }
    }

    if paths.is_empty() {
        eprintln!(
            "no cargo registry directories found in {}",
            cargo_home.display()
        );
        return ExitCode::SUCCESS;
    }

    // Create tar.gz archive.
    let file = match std::fs::File::create(&archive_path) {
        Ok(f) => f,
        Err(e) => {
            eprintln!(
                "zccache cargo-registry save: failed to create {}: {e}",
                archive_path.display()
            );
            return ExitCode::FAILURE;
        }
    };
    let gz = flate2::write::GzEncoder::new(file, flate2::Compression::fast());
    let mut tar = tar::Builder::new(gz);

    for (path, name) in &paths {
        if let Err(e) = tar.append_dir_all(name, path) {
            eprintln!("zccache cargo-registry save: failed to add {name}: {e}");
            return ExitCode::FAILURE;
        }
    }
    if let Err(e) = tar.finish() {
        eprintln!("zccache cargo-registry save: failed to finalize archive: {e}");
        return ExitCode::FAILURE;
    }

    let size = std::fs::metadata(&archive_path)
        .map(|m| m.len())
        .unwrap_or(0);
    println!(
        "saved cargo registry to {} ({})",
        archive_path.display(),
        format_bytes(size)
    );
    ExitCode::SUCCESS
}

fn cmd_cargo_registry_restore(key: &str, cargo_home: Option<&str>) -> ExitCode {
    let cargo_home = match resolve_cargo_home(cargo_home) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("zccache cargo-registry restore: {e}");
            return ExitCode::FAILURE;
        }
    };
    let cache_dir = match cargo_registry_cache_dir() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("zccache cargo-registry restore: {e}");
            return ExitCode::FAILURE;
        }
    };
    let archive_path = cache_dir.join(format!("{key}.tar.gz"));

    if !archive_path.exists() {
        eprintln!("no cached registry found for key: {key}");
        return ExitCode::FAILURE;
    }

    let file = match std::fs::File::open(&archive_path) {
        Ok(f) => f,
        Err(e) => {
            eprintln!(
                "zccache cargo-registry restore: failed to open {}: {e}",
                archive_path.display()
            );
            return ExitCode::FAILURE;
        }
    };
    let gz = flate2::read::GzDecoder::new(file);
    let mut tar = tar::Archive::new(gz);
    if let Err(e) = tar.unpack(&cargo_home) {
        eprintln!("zccache cargo-registry restore: failed to unpack archive: {e}");
        return ExitCode::FAILURE;
    }

    println!("restored cargo registry from {}", archive_path.display());
    ExitCode::SUCCESS
}

fn cmd_cargo_registry_hash(lockfile: &str) -> ExitCode {
    let path = Path::new(lockfile);
    if !path.exists() {
        eprintln!("lockfile not found: {lockfile}");
        return ExitCode::FAILURE;
    }
    let hash = match zccache_hash::hash_file(path) {
        Ok(h) => h,
        Err(e) => {
            eprintln!("zccache cargo-registry hash: failed to hash {lockfile}: {e}");
            return ExitCode::FAILURE;
        }
    };
    // Print first 16 hex chars (matches action's cache key format).
    let hex = hash.to_hex();
    println!("{}", &hex[..16]);
    ExitCode::SUCCESS
}

fn cmd_cargo_registry_clean() -> ExitCode {
    let cache_dir = match cargo_registry_cache_dir() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("zccache cargo-registry clean: {e}");
            return ExitCode::FAILURE;
        }
    };
    if cache_dir.exists() {
        let count = match std::fs::read_dir(&cache_dir) {
            Ok(entries) => entries.count(),
            Err(e) => {
                eprintln!(
                    "zccache cargo-registry clean: failed to read {}: {e}",
                    cache_dir.display()
                );
                return ExitCode::FAILURE;
            }
        };
        if let Err(e) = std::fs::remove_dir_all(&cache_dir) {
            eprintln!(
                "zccache cargo-registry clean: failed to remove {}: {e}",
                cache_dir.display()
            );
            return ExitCode::FAILURE;
        }
        println!("removed {count} cached registry archive(s)");
    } else {
        println!("no cached archives to clean");
    }
    ExitCode::SUCCESS
}

// ─── GHA cache subcommands ────────────────────────────────────────────────

fn cmd_gha_status() -> ExitCode {
    if GhaCache::is_available() {
        let url = std::env::var("ACTIONS_CACHE_URL").unwrap_or_default();
        println!("GHA cache: available");
        println!("  ACTIONS_CACHE_URL = {url}");
        ExitCode::SUCCESS
    } else {
        println!("GHA cache: not available (ACTIONS_CACHE_URL or ACTIONS_RUNTIME_TOKEN not set)");
        ExitCode::SUCCESS
    }
}

async fn cmd_gha_save(key: &str, path: &str) -> ExitCode {
    let cache = match GhaCache::from_env() {
        Ok(c) => c,
        Err(GhaError::NotAvailable) => {
            eprintln!("zccache gha-cache: not running in GitHub Actions");
            return ExitCode::FAILURE;
        }
        Err(e) => {
            eprintln!("zccache gha-cache: {e}");
            return ExitCode::FAILURE;
        }
    };

    let src = Path::new(path);
    if !src.exists() {
        eprintln!("zccache gha-cache save: path does not exist: {path}");
        return ExitCode::FAILURE;
    }

    // Create a tar.gz archive in memory.
    let data = match tar_gz_encode(src) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("zccache gha-cache save: failed to create archive: {e}");
            return ExitCode::FAILURE;
        }
    };

    let version = GhaCache::version_hash(&[path]);
    match cache.save(key, &version, &data).await {
        Ok(()) => {
            eprintln!(
                "zccache gha-cache save: uploaded {} bytes for key '{key}'",
                data.len()
            );
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("zccache gha-cache save: {e}");
            ExitCode::FAILURE
        }
    }
}

async fn cmd_gha_restore(key: &str, path: &str) -> ExitCode {
    let cache = match GhaCache::from_env() {
        Ok(c) => c,
        Err(GhaError::NotAvailable) => {
            eprintln!("zccache gha-cache: not running in GitHub Actions");
            return ExitCode::FAILURE;
        }
        Err(e) => {
            eprintln!("zccache gha-cache: {e}");
            return ExitCode::FAILURE;
        }
    };

    let version = GhaCache::version_hash(&[path]);
    let data = match cache.restore(key, &version).await {
        Ok(Some(d)) => d,
        Ok(None) => {
            eprintln!("zccache gha-cache restore: cache miss for key '{key}'");
            return ExitCode::FAILURE;
        }
        Err(e) => {
            eprintln!("zccache gha-cache restore: {e}");
            return ExitCode::FAILURE;
        }
    };

    let dest = Path::new(path);
    if let Err(e) = std::fs::create_dir_all(dest) {
        eprintln!("zccache gha-cache restore: failed to create directory: {e}");
        return ExitCode::FAILURE;
    }

    match tar_gz_decode(&data, dest) {
        Ok(()) => {
            eprintln!(
                "zccache gha-cache restore: restored {} bytes for key '{key}' to {path}",
                data.len()
            );
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("zccache gha-cache restore: failed to extract archive: {e}");
            ExitCode::FAILURE
        }
    }
}

// ─── Rust artifact plan subcommands ─────────────────────────────────────────

async fn cmd_rust_plan(action: RustPlanCommands) -> ExitCode {
    match action {
        RustPlanCommands::Validate {
            plan,
            json,
            session_id,
            endpoint,
            journal,
            cache_dir,
        } => {
            let cache_dir = resolve_rust_plan_cache_dir(cache_dir.as_deref());
            match load_rust_plan_for_cli(&plan, RustPlanOperation::Validate, json) {
                Ok(plan) => {
                    let mut summary =
                        RustPlanSummary::validation_success(&plan, cache_dir.as_path());
                    enrich_rust_plan_summary(
                        &mut summary,
                        session_id.as_deref(),
                        endpoint.as_deref(),
                        journal.as_deref(),
                    )
                    .await;
                    print_rust_plan_summary(&summary, json);
                    ExitCode::SUCCESS
                }
                Err(code) => code,
            }
        }
        RustPlanCommands::Restore {
            plan,
            json,
            session_id,
            endpoint,
            journal,
            backend,
            cache_dir,
        } => {
            let cache_dir = resolve_rust_plan_cache_dir(cache_dir.as_deref());
            let plan = match load_rust_plan_for_cli(&plan, RustPlanOperation::Restore, json) {
                Ok(plan) => plan,
                Err(code) => return code,
            };
            let backend = resolve_rust_plan_backend(backend);
            match run_rust_plan_restore(&plan, cache_dir.as_path(), backend).await {
                Ok(mut summary) => {
                    enrich_rust_plan_summary(
                        &mut summary,
                        session_id.as_deref(),
                        endpoint.as_deref(),
                        journal.as_deref(),
                    )
                    .await;
                    print_rust_plan_summary(&summary, json);
                    ExitCode::SUCCESS
                }
                Err(err) => {
                    print_rust_plan_runtime_error(
                        RustPlanOperation::Restore,
                        &plan,
                        cache_dir.as_path(),
                        backend,
                        &err,
                        json,
                    );
                    ExitCode::FAILURE
                }
            }
        }
        RustPlanCommands::Save {
            plan,
            json,
            session_id,
            endpoint,
            journal,
            backend,
            cache_dir,
        } => {
            let cache_dir = resolve_rust_plan_cache_dir(cache_dir.as_deref());
            let plan = match load_rust_plan_for_cli(&plan, RustPlanOperation::Save, json) {
                Ok(plan) => plan,
                Err(code) => return code,
            };
            let backend = resolve_rust_plan_backend(backend);
            match run_rust_plan_save(&plan, cache_dir.as_path(), backend).await {
                Ok(mut summary) => {
                    enrich_rust_plan_summary(
                        &mut summary,
                        session_id.as_deref(),
                        endpoint.as_deref(),
                        journal.as_deref(),
                    )
                    .await;
                    print_rust_plan_summary(&summary, json);
                    ExitCode::SUCCESS
                }
                Err(err) => {
                    print_rust_plan_runtime_error(
                        RustPlanOperation::Save,
                        &plan,
                        cache_dir.as_path(),
                        backend,
                        &err,
                        json,
                    );
                    ExitCode::FAILURE
                }
            }
        }
    }
}

fn resolve_rust_plan_cache_dir(explicit: Option<&str>) -> NormalizedPath {
    explicit
        .map(NormalizedPath::from)
        .unwrap_or_else(|| zccache_core::config::default_cache_dir().join("rust-artifacts"))
}

fn load_rust_plan_for_cli(
    path: &str,
    operation: RustPlanOperation,
    json: bool,
) -> Result<RustArtifactPlanV1, ExitCode> {
    match RustArtifactPlanV1::load(Path::new(path)) {
        Ok(plan) => Ok(plan),
        Err(err) => {
            print_rust_plan_error(operation, &err, json);
            Err(ExitCode::FAILURE)
        }
    }
}

async fn run_rust_plan_restore(
    plan: &RustArtifactPlanV1,
    cache_dir: &Path,
    backend: RustPlanBackendArg,
) -> Result<RustPlanSummary, RustPlanRuntimeError> {
    match backend {
        RustPlanBackendArg::Local => restore_rust_plan_local(plan, cache_dir)
            .map_err(|err| rust_plan_backend_failure(backend, err.to_string())),
        RustPlanBackendArg::Gha => restore_rust_plan_gha(plan, cache_dir).await,
        RustPlanBackendArg::Auto => unreachable!("auto backend is resolved before execution"),
    }
}

async fn run_rust_plan_save(
    plan: &RustArtifactPlanV1,
    cache_dir: &Path,
    backend: RustPlanBackendArg,
) -> Result<RustPlanSummary, RustPlanRuntimeError> {
    match backend {
        RustPlanBackendArg::Local => save_rust_plan_local(plan, cache_dir)
            .map_err(|err| rust_plan_backend_failure(backend, err.to_string())),
        RustPlanBackendArg::Gha => save_rust_plan_gha(plan, cache_dir).await,
        RustPlanBackendArg::Auto => unreachable!("auto backend is resolved before execution"),
    }
}

fn resolve_rust_plan_backend(backend: RustPlanBackendArg) -> RustPlanBackendArg {
    match backend {
        RustPlanBackendArg::Auto if GhaCache::is_available() => RustPlanBackendArg::Gha,
        RustPlanBackendArg::Auto => RustPlanBackendArg::Local,
        other => other,
    }
}

fn rust_plan_gha_version(cache_key: &str) -> String {
    GhaCache::version_hash(&["zccache-rust-plan-v1", cache_key])
}

async fn restore_rust_plan_gha(
    plan: &RustArtifactPlanV1,
    cache_dir: &Path,
) -> Result<RustPlanSummary, RustPlanRuntimeError> {
    let cache_key = rust_plan_cache_key(plan);
    let version = rust_plan_gha_version(&cache_key);
    let cache = GhaCache::from_env().map_err(rust_plan_gha_error)?;
    let Some(data) = cache
        .restore(&cache_key, &version)
        .await
        .map_err(rust_plan_gha_error)?
    else {
        let mut summary = restore_rust_plan_local(plan, cache_dir)
            .map_err(|err| rust_plan_backend_failure(RustPlanBackendArg::Gha, err.to_string()))?;
        summary.set_backend("gha", Some(cache_key), Some(version));
        summary.record_skip("<gha-cache>", "backend_cache_miss");
        return Ok(summary);
    };

    let bundle_dir = rust_plan_bundle_dir(cache_dir, &cache_key);
    if bundle_dir.exists() {
        std::fs::remove_dir_all(&bundle_dir)
            .map_err(|err| rust_plan_backend_failure(RustPlanBackendArg::Gha, err.to_string()))?;
    }
    let bundle_parent = bundle_dir.parent().ok_or_else(|| {
        rust_plan_backend_failure(
            RustPlanBackendArg::Gha,
            "invalid rust-plan bundle path".to_string(),
        )
    })?;
    std::fs::create_dir_all(bundle_parent)
        .map_err(|err| rust_plan_backend_failure(RustPlanBackendArg::Gha, err.to_string()))?;
    tar_gz_decode(&data, bundle_parent)
        .map_err(|err| rust_plan_backend_failure(RustPlanBackendArg::Gha, err.to_string()))?;
    let mut summary = restore_rust_plan_local(plan, cache_dir)
        .map_err(|err| rust_plan_backend_failure(RustPlanBackendArg::Gha, err.to_string()))?;
    summary.set_backend("gha", Some(cache_key), Some(version));
    Ok(summary)
}

async fn save_rust_plan_gha(
    plan: &RustArtifactPlanV1,
    cache_dir: &Path,
) -> Result<RustPlanSummary, RustPlanRuntimeError> {
    let summary = save_rust_plan_local(plan, cache_dir)
        .map_err(|err| rust_plan_backend_failure(RustPlanBackendArg::Gha, err.to_string()))?;
    let cache_key = summary.cache_key.clone();
    let bundle_dir = rust_plan_bundle_dir(cache_dir, &cache_key);
    let data = tar_gz_encode(&bundle_dir)
        .map_err(|err| rust_plan_backend_failure(RustPlanBackendArg::Gha, err.to_string()))?;
    let version = rust_plan_gha_version(&cache_key);
    let cache = GhaCache::from_env().map_err(rust_plan_gha_error)?;
    cache
        .save(&cache_key, &version, &data)
        .await
        .map_err(rust_plan_gha_error)?;
    let mut summary = summary;
    summary.set_backend("gha", Some(cache_key), Some(version));
    Ok(summary)
}

fn rust_plan_gha_error(err: GhaError) -> RustPlanRuntimeError {
    let kind = if matches!(&err, GhaError::NotAvailable) {
        RustPlanRuntimeErrorKind::Unavailable
    } else {
        RustPlanRuntimeErrorKind::Failure
    };
    rust_plan_backend_failure(RustPlanBackendArg::Gha, err.to_string()).with_kind(kind)
}

fn rust_plan_backend_failure(backend: RustPlanBackendArg, message: String) -> RustPlanRuntimeError {
    RustPlanRuntimeError::Backend {
        backend,
        kind: RustPlanRuntimeErrorKind::Failure,
        message,
    }
}

fn rust_plan_runtime_error_message(err: &RustPlanRuntimeError) -> String {
    let backend = match err.backend() {
        RustPlanBackendArg::Local => "local",
        RustPlanBackendArg::Gha => "GHA",
        RustPlanBackendArg::Auto => unreachable!("auto backend is resolved before execution"),
    };
    let kind = match err.kind() {
        RustPlanRuntimeErrorKind::Unavailable => "unavailable",
        RustPlanRuntimeErrorKind::Failure => "failure",
    };
    format!("{backend} cache backend {kind}: {}", err.message())
}

fn rust_plan_runtime_failure_summary(
    operation: RustPlanOperation,
    plan: &RustArtifactPlanV1,
    cache_dir: &Path,
    backend: RustPlanBackendArg,
    err: &RustPlanRuntimeError,
) -> RustPlanSummary {
    let mut summary = RustPlanSummary::validation_success(plan, cache_dir);
    summary.operation = operation;
    summary.backend = match backend {
        RustPlanBackendArg::Local => "local".to_string(),
        RustPlanBackendArg::Gha => "gha".to_string(),
        RustPlanBackendArg::Auto => unreachable!("auto backend is resolved before execution"),
    };
    if matches!(backend, RustPlanBackendArg::Gha) {
        let cache_key = summary.cache_key.clone();
        summary.backend_cache_key = Some(cache_key.clone());
        summary.backend_cache_version = Some(rust_plan_gha_version(&cache_key));
    }
    summary.compatibility.status = "error".to_string();
    summary.compatibility.errors = vec![rust_plan_runtime_error_message(err)];
    summary
}

fn print_rust_plan_runtime_error(
    operation: RustPlanOperation,
    plan: &RustArtifactPlanV1,
    cache_dir: &Path,
    backend: RustPlanBackendArg,
    err: &RustPlanRuntimeError,
    json: bool,
) {
    if json {
        let summary = rust_plan_runtime_failure_summary(operation, plan, cache_dir, backend, err);
        print_rust_plan_summary(&summary, true);
    } else {
        eprintln!(
            "zccache rust-plan: {}",
            rust_plan_runtime_error_message(err)
        );
    }
}

async fn enrich_rust_plan_summary(
    summary: &mut RustPlanSummary,
    session_id: Option<&str>,
    endpoint: Option<&str>,
    journal: Option<&str>,
) {
    if let Some(journal) = journal {
        summary.journal_log_path = Some(absolute_path(journal));
    }

    if let Some(session_id) = session_id {
        let endpoint = resolve_endpoint(endpoint);
        summary.compile_cache_stats = Some(query_session_stats_json(&endpoint, session_id).await);
    }
}

async fn query_session_stats_json(endpoint: &str, session_id: &str) -> serde_json::Value {
    match query_session_stats(endpoint, session_id).await {
        Ok(Some(stats)) => session_stats_json(session_id, &stats),
        Ok(None) => serde_json::json!({
            "status": "not_tracked",
            "session_id": session_id,
            "message": "session exists but stats tracking is not enabled"
        }),
        Err(err) => serde_json::json!({
            "status": "error",
            "session_id": session_id,
            "error": err
        }),
    }
}

async fn query_session_stats(
    endpoint: &str,
    session_id: &str,
) -> Result<Option<zccache_protocol::SessionStats>, String> {
    let mut conn = connect(endpoint)
        .await
        .map_err(|err| format!("cannot connect to daemon at {endpoint}: {err}"))?;

    conn.send(&zccache_protocol::Request::SessionStats {
        session_id: session_id.to_string(),
    })
    .await
    .map_err(|err| format!("failed to send session stats request: {err}"))?;

    let recv_result = conn
        .recv()
        .await
        .map_err(|err| format!("broken daemon connection: {err}"))?;
    match recv_result {
        Some(zccache_protocol::Response::SessionStatsResult { stats }) => Ok(stats),
        Some(zccache_protocol::Response::Error { message }) => Err(message),
        Some(other) => Err(format!("unexpected daemon response: {other:?}")),
        None => Err("lost connection to daemon (no response received)".to_string()),
    }
}

fn session_stats_json(
    session_id: &str,
    stats: &zccache_protocol::SessionStats,
) -> serde_json::Value {
    let total = stats.hits + stats.misses;
    let hit_rate = if total > 0 {
        Some(stats.hits as f64 / total as f64)
    } else {
        None
    };
    serde_json::json!({
        "status": "ok",
        "session_id": session_id,
        "duration_ms": stats.duration_ms,
        "compilations": stats.compilations,
        "hits": stats.hits,
        "misses": stats.misses,
        "non_cacheable": stats.non_cacheable,
        "errors": stats.errors,
        "time_saved_ms": stats.time_saved_ms,
        "unique_sources": stats.unique_sources,
        "bytes_read": stats.bytes_read,
        "bytes_written": stats.bytes_written,
        "hit_rate": hit_rate,
    })
}

fn print_rust_plan_summary(summary: &RustPlanSummary, json: bool) {
    if json {
        match serde_json::to_string_pretty(summary) {
            Ok(s) => println!("{s}"),
            Err(err) => eprintln!("zccache rust-plan: failed to encode JSON summary: {err}"),
        }
        return;
    }

    println!(
        "zccache rust-plan {}: {}",
        match summary.operation {
            RustPlanOperation::Validate => "validate",
            RustPlanOperation::Restore => "restore",
            RustPlanOperation::Save => "save",
        },
        summary.compatibility.status
    );
    println!("  mode: {}", summary.mode);
    println!("  backend: {}", summary.backend);
    println!("  cache key: {}", summary.cache_key);
    if let Some(key) = &summary.backend_cache_key {
        println!("  backend cache key: {key}");
    }
    if let Some(version) = &summary.backend_cache_version {
        println!("  backend cache version: {version}");
    }
    if let Some(path) = &summary.archive_path {
        println!("  bundle: {}", path.display());
    }
    if summary.saved_file_count > 0 || summary.saved_bytes > 0 {
        println!(
            "  saved: {} files ({})",
            summary.saved_file_count,
            format_bytes(summary.saved_bytes)
        );
    }
    if summary.restored_file_count > 0 || summary.restored_bytes > 0 {
        println!(
            "  restored: {} files ({})",
            summary.restored_file_count,
            format_bytes(summary.restored_bytes)
        );
    }
    if summary.skipped_count > 0 {
        println!("  skipped: {}", summary.skipped_count);
        for (reason, count) in &summary.skipped_reasons {
            println!("    {reason}: {count}");
        }
    }
    for mismatch in &summary.key_input_mismatches {
        println!("  mismatch: {mismatch}");
    }
    if let Some(stats) = &summary.compile_cache_stats {
        println!("  compile cache stats: {stats}");
    }
}

fn print_rust_plan_error(operation: RustPlanOperation, err: &RustPlanError, json: bool) {
    if json {
        let summary = RustPlanSummary::compatibility_failure(operation, err);
        print_rust_plan_summary(&summary, true);
    } else {
        eprintln!("zccache rust-plan: {err}");
    }
}

/// Create a tar.gz archive from a directory path.
fn tar_gz_encode(src: &Path) -> Result<Vec<u8>, std::io::Error> {
    use flate2::write::GzEncoder;
    use flate2::Compression;

    let buf = Vec::new();
    let enc = GzEncoder::new(buf, Compression::fast());
    let mut tar = tar::Builder::new(enc);
    // Use the last component of the path as the archive prefix so that
    // extraction recreates the directory structure relative to the target.
    let prefix = src
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| ".".to_string());
    tar.append_dir_all(&prefix, src)?;
    let enc = tar.into_inner()?;
    enc.finish()
}

/// Extract a tar.gz archive into a destination directory.
fn tar_gz_decode(data: &[u8], dest: &Path) -> Result<(), std::io::Error> {
    use flate2::read::GzDecoder;

    let dec = GzDecoder::new(data);
    let mut archive = tar::Archive::new(dec);
    archive.unpack(dest)
}

// ─── Fingerprint subcommands ──────────────────────────────────────────────

async fn cmd_fp_check(
    endpoint: &str,
    cache_file: &Path,
    cache_type: &str,
    root: &Path,
    ext: &[String],
    include: &[String],
    exclude: &[String],
) -> ExitCode {
    if let Err(e) = ensure_daemon(endpoint).await {
        eprintln!("zccache fp: failed to start daemon: {e}");
        return ExitCode::from(2);
    }

    let mut conn = match connect(endpoint).await {
        Ok(c) => c,
        Err(e) => {
            eprintln!("zccache fp: cannot connect to daemon: {e}");
            return ExitCode::from(2);
        }
    };

    let request = zccache_protocol::Request::FingerprintCheck {
        cache_file: cache_file.into(),
        cache_type: cache_type.to_string(),
        root: root.into(),
        extensions: ext.to_vec(),
        include_globs: include.to_vec(),
        exclude: exclude.to_vec(),
    };

    if let Err(e) = conn.send(&request).await {
        eprintln!("zccache fp: send error: {e}");
        return ExitCode::from(2);
    }

    match conn.recv::<zccache_protocol::Response>().await {
        Ok(Some(zccache_protocol::Response::FingerprintCheckResult {
            decision,
            reason,
            changed_files,
        })) => {
            if decision == "skip" {
                eprintln!("zccache fp: skip (no changes)");
                ExitCode::from(1)
            } else {
                let reason_str = reason.as_deref().unwrap_or("unknown");
                if changed_files.is_empty() {
                    eprintln!("zccache fp: run ({reason_str})");
                } else {
                    eprintln!(
                        "zccache fp: run ({reason_str}, {} file(s) changed)",
                        changed_files.len()
                    );
                }
                ExitCode::SUCCESS
            }
        }
        Ok(Some(zccache_protocol::Response::Error { message })) => {
            eprintln!("zccache fp: daemon error: {message}");
            ExitCode::from(2)
        }
        Ok(other) => {
            eprintln!("zccache fp: unexpected response: {other:?}");
            ExitCode::from(2)
        }
        Err(e) => {
            eprintln!("zccache fp: recv error: {e}");
            ExitCode::from(2)
        }
    }
}

async fn cmd_fp_mark(endpoint: &str, cache_file: &Path, success: bool) -> ExitCode {
    if let Err(e) = ensure_daemon(endpoint).await {
        eprintln!("zccache fp: failed to start daemon: {e}");
        return ExitCode::from(2);
    }

    let mut conn = match connect(endpoint).await {
        Ok(c) => c,
        Err(e) => {
            eprintln!("zccache fp: cannot connect to daemon: {e}");
            return ExitCode::from(2);
        }
    };

    let request = if success {
        zccache_protocol::Request::FingerprintMarkSuccess {
            cache_file: cache_file.into(),
        }
    } else {
        zccache_protocol::Request::FingerprintMarkFailure {
            cache_file: cache_file.into(),
        }
    };

    if let Err(e) = conn.send(&request).await {
        eprintln!("zccache fp: send error: {e}");
        return ExitCode::from(2);
    }

    match conn.recv::<zccache_protocol::Response>().await {
        Ok(Some(zccache_protocol::Response::FingerprintAck)) => {
            let label = if success {
                "mark-success"
            } else {
                "mark-failure"
            };
            eprintln!("zccache fp: {label}");
            ExitCode::SUCCESS
        }
        Ok(Some(zccache_protocol::Response::Error { message })) => {
            eprintln!("zccache fp: daemon error: {message}");
            ExitCode::from(2)
        }
        Ok(other) => {
            eprintln!("zccache fp: unexpected response: {other:?}");
            ExitCode::from(2)
        }
        Err(e) => {
            eprintln!("zccache fp: recv error: {e}");
            ExitCode::from(2)
        }
    }
}

async fn cmd_fp_invalidate(endpoint: &str, cache_file: &Path) -> ExitCode {
    if let Err(e) = ensure_daemon(endpoint).await {
        eprintln!("zccache fp: failed to start daemon: {e}");
        return ExitCode::from(2);
    }

    let mut conn = match connect(endpoint).await {
        Ok(c) => c,
        Err(e) => {
            eprintln!("zccache fp: cannot connect to daemon: {e}");
            return ExitCode::from(2);
        }
    };

    let request = zccache_protocol::Request::FingerprintInvalidate {
        cache_file: cache_file.into(),
    };

    if let Err(e) = conn.send(&request).await {
        eprintln!("zccache fp: send error: {e}");
        return ExitCode::from(2);
    }

    match conn.recv::<zccache_protocol::Response>().await {
        Ok(Some(zccache_protocol::Response::FingerprintAck)) => {
            eprintln!("zccache fp: invalidated");
            ExitCode::SUCCESS
        }
        Ok(Some(zccache_protocol::Response::Error { message })) => {
            eprintln!("zccache fp: daemon error: {message}");
            ExitCode::from(2)
        }
        Ok(other) => {
            eprintln!("zccache fp: unexpected response: {other:?}");
            ExitCode::from(2)
        }
        Err(e) => {
            eprintln!("zccache fp: recv error: {e}");
            ExitCode::from(2)
        }
    }
}

// ─── Wrap (compiler wrapper) ───────────────────────────────────────────────

/// Run the compiler/tool directly without caching (ZCCACHE_DISABLE mode).
fn run_passthrough(args: &[String]) -> ExitCode {
    let tool = &args[0];
    let tool_args = if args.len() > 1 { &args[1..] } else { &[] };

    // Resolve the tool path (normalize MSYS paths, search PATH)
    let resolved = resolve_compiler_path(tool);

    match std::process::Command::new(&resolved)
        .args(tool_args)
        .status()
    {
        Ok(status) => exit_code_from_i32(status.code().unwrap_or(1)),
        Err(e) => {
            eprintln!("zccache: failed to run {}: {e}", resolved.display());
            ExitCode::FAILURE
        }
    }
}

// ─── Rustfmt caching ───────────────────────────────────────────────────────

/// Run rustfmt with format caching.
///
/// Files whose content hash is already in the format cache are skipped entirely,
/// preserving their mtime and avoiding unnecessary downstream rebuilds.
/// After formatting, the new content hash of each file is stored in the cache.
fn run_rustfmt_cached(rustfmt_path: &Path, args: &[String], cwd: &Path) -> ExitCode {
    use zccache_compiler::parse_rustfmt::{find_rustfmt_config, parse_rustfmt_invocation};

    let parsed = match parse_rustfmt_invocation(args) {
        Some(p) => p,
        None => {
            // --help, --version, or stdin mode: pass through
            return run_tool_direct(rustfmt_path, args);
        }
    };

    // Build format context: rustfmt binary identity + config + flags.
    // Changes to any of these invalidate the entire format cache scope.
    let context_hash = {
        let mut hasher = zccache_hash::StreamHasher::new();
        hasher.update(b"zccache-fmt-v1");

        // Hash rustfmt binary content for version identity
        if let Ok(bin_hash) = zccache_hash::hash_file(rustfmt_path) {
            hasher.update(bin_hash.as_bytes());
        } else {
            hasher.update(b"unknown-binary");
        }

        // Hash config file content (if found)
        let config_path = parsed
            .config_path
            .clone()
            .or_else(|| find_rustfmt_config(cwd));
        if let Some(ref cfg) = config_path {
            if let Ok(cfg_hash) = zccache_hash::hash_file(cfg) {
                hasher.update(cfg_hash.as_bytes());
            }
        }

        // Hash flags (edition, --check, etc.)
        for flag in &parsed.flags {
            hasher.update(flag.as_bytes());
            hasher.update(b"\0");
        }

        hasher.finalize().to_hex()
    };

    // Format cache directory: {cache_dir}/fmt/{context_hash}/
    let cache_dir = zccache_core::config::default_cache_dir()
        .join("fmt")
        .join(&context_hash);

    // Ensure cache dir exists
    let _ = std::fs::create_dir_all(&cache_dir);

    // Resolve source files to absolute paths and check cache (parallel)
    use rayon::prelude::*;

    let results: Vec<(NormalizedPath, bool, Option<zccache_hash::ContentHash>)> = parsed
        .source_files
        .par_iter()
        .map(|src| {
            let abs = if src.is_absolute() {
                src.clone()
            } else {
                cwd.join(src).into()
            };
            let (is_hit, hash) = match zccache_hash::hash_file(&abs) {
                Ok(content_hash) => {
                    let marker = cache_dir.join(content_hash.to_hex());
                    (marker.exists(), Some(content_hash))
                }
                Err(_) => (false, None),
            };
            (abs, is_hit, hash)
        })
        .collect();

    let mut miss_files: Vec<NormalizedPath> = Vec::new();
    let mut all_files: Vec<(NormalizedPath, bool, Option<zccache_hash::ContentHash>)> = Vec::new();
    for (abs, is_hit, hash) in results {
        if !is_hit {
            miss_files.push(abs.clone());
        }
        all_files.push((abs, is_hit, hash));
    }

    // All files are cache hits — skip rustfmt entirely (mtime preserved!)
    if miss_files.is_empty() {
        if parsed.check_mode {
            // --check: all files are known-formatted → exit 0
            return ExitCode::SUCCESS;
        }
        // Normal mode: all files already formatted → nothing to do
        return ExitCode::SUCCESS;
    }

    // Run rustfmt on miss files only (normal mode) or all files (--check mode)
    let exit_code = if parsed.check_mode {
        // --check mode: run on miss files only; if all would pass, we
        // already returned above. For misses, we must run to determine
        // if they're formatted.
        run_rustfmt_on_files(rustfmt_path, args, &miss_files, &parsed)
    } else {
        // Normal mode: run on miss files only
        run_rustfmt_on_files(rustfmt_path, args, &miss_files, &parsed)
    };

    let exit_i32 = match exit_code {
        Ok(code) => code,
        Err(e) => {
            eprintln!("zccache: failed to run rustfmt: {e}");
            return ExitCode::FAILURE;
        }
    };

    // On success (exit 0), store new content hashes in format cache
    if exit_i32 == 0 {
        // For --check mode with exit 0: the miss files were already formatted
        // (we just didn't know it). Reuse the hash from the lookup phase.
        // For normal mode with exit 0: files were reformatted. Must re-hash.
        for (abs, was_hit, cached_hash) in &all_files {
            if *was_hit {
                continue; // Already in cache
            }
            let new_hash = if parsed.check_mode {
                *cached_hash
            } else {
                zccache_hash::hash_file(abs).ok()
            };
            if let Some(h) = new_hash {
                let marker = cache_dir.join(h.to_hex());
                let _ = std::fs::write(&marker, b"");
            }
        }
    }

    exit_code_from_i32(exit_i32)
}

/// Run rustfmt on a specific set of files, reconstructing the argument list.
fn run_rustfmt_on_files(
    rustfmt_path: &Path,
    original_args: &[String],
    files: &[NormalizedPath],
    parsed: &zccache_compiler::parse_rustfmt::ParsedRustfmt,
) -> Result<i32, std::io::Error> {
    // Reconstruct args: flags + the miss files (not the original file list)
    let mut cmd = std::process::Command::new(rustfmt_path);
    cmd.args(&parsed.flags);
    for f in files {
        cmd.arg(f);
    }

    // Suppress original args' source files — we pass our filtered list above.
    // But we need to preserve any non-file, non-flag args. In practice,
    // flags + files covers everything.
    let _ = original_args; // intentionally unused — we reconstruct from parsed

    let status = cmd.status()?;
    Ok(status.code().unwrap_or(1))
}

/// Run a tool directly and return its exit code.
fn run_tool_direct(tool: &Path, args: &[String]) -> ExitCode {
    match std::process::Command::new(tool).args(args).status() {
        Ok(status) => exit_code_from_i32(status.code().unwrap_or(1)),
        Err(e) => {
            eprintln!("zccache: failed to run {}: {e}", tool.display());
            ExitCode::FAILURE
        }
    }
}

// ─── Wrap (compiler wrapper) ───────────────────────────────────────────────

fn strip_leading_strict_paths_flags(
    args: &[String],
) -> Result<(Option<StrictPathsMode>, Vec<String>), String> {
    let mut strict_paths = None;
    let mut index = 0;

    while let Some(arg) = args.get(index) {
        if arg == "--strict-paths" {
            strict_paths = Some(StrictPathsMode::Absolute);
            index += 1;
        } else if let Some(value) = arg.strip_prefix("--strict-paths=") {
            strict_paths = Some(StrictPathsMode::parse(value).map_err(|err| err.to_string())?);
            index += 1;
        } else {
            break;
        }
    }

    Ok((strict_paths, args[index..].to_vec()))
}

fn parse_optional_strict_paths(value: Option<&str>) -> Result<Option<StrictPathsMode>, String> {
    value
        .map(|value| StrictPathsMode::parse(value).map_err(|err| err.to_string()))
        .transpose()
}

fn effective_strict_paths_mode(
    strict_paths_override: Option<StrictPathsMode>,
) -> Result<StrictPathsMode, String> {
    if let Some(mode) = strict_paths_override {
        return Ok(mode);
    }

    match std::env::var("ZCCACHE_STRICT_PATHS") {
        Ok(value) => StrictPathsMode::parse(&value).map_err(|err| err.to_string()),
        Err(std::env::VarError::NotPresent) => Ok(StrictPathsMode::Off),
        Err(std::env::VarError::NotUnicode(_)) => {
            Err("ZCCACHE_STRICT_PATHS is not valid Unicode".to_string())
        }
    }
}

fn set_client_env(env: &mut Vec<(String, String)>, key: &str, value: String) {
    if let Some((_, existing)) = env.iter_mut().find(|(env_key, _)| env_key == key) {
        *existing = value;
    } else {
        env.push((key.to_string(), value));
    }
}

/// Wrap a compiler or tool invocation.
///
/// `args` is the full command: ["clang++", "-c", "foo.cpp", "-o", "foo.o"]
/// or ["ar", "rcs", "libfoo.a", "a.o", "b.o"]
///
/// If the first arg is a known archiver (ar, llvm-ar, lib.exe), routes to
/// the link/archive path. Otherwise, routes to the compile path.
///
/// If ZCCACHE_SESSION_ID is set, uses that session and sends the tool
/// as a per-request override. If unset, auto-creates an ephemeral session.
fn run_wrap(args: &[String], strict_paths_override: Option<StrictPathsMode>) -> ExitCode {
    if args.is_empty() {
        eprintln!("usage: zccache <compiler|tool> <args...>");
        return ExitCode::FAILURE;
    }

    // ZCCACHE_DISABLE=1 — passthrough to compiler/tool without caching.
    if std::env::var("ZCCACHE_DISABLE").is_ok_and(|v| v == "1" || v.eq_ignore_ascii_case("true")) {
        return run_passthrough(args);
    }

    let strict_paths_mode = match effective_strict_paths_mode(strict_paths_override) {
        Ok(mode) => mode,
        Err(err) => {
            eprintln!("zccache: {err}");
            return ExitCode::FAILURE;
        }
    };

    // Normalize MSYS paths (e.g. /c/Users/... → C:\Users\...) on Windows,
    // then resolve to an absolute path so the daemon can find it.
    let wrapped_tool = resolve_compiler_path(&args[0]);
    let tool_args: Vec<String> = if args.len() > 1 {
        args[1..].to_vec()
    } else {
        Vec::new()
    };

    let cwd = std::env::current_dir().unwrap_or_default();

    let mut client_env: Vec<(String, String)> = std::env::vars().collect();
    if let Some(mode) = strict_paths_override {
        set_client_env(
            &mut client_env,
            "ZCCACHE_STRICT_PATHS",
            mode.as_str().to_string(),
        );
    }
    let endpoint = resolve_endpoint(None);

    // Release the CWD handle on the build directory. On Windows, a process's
    // CWD holds an implicit kernel handle that prevents the directory from
    // being deleted. We've captured everything we need into local variables.
    let _ = std::env::set_current_dir(std::env::temp_dir());

    // Check if this is a rustfmt invocation — handle via format cache path
    if zccache_compiler::detect_family(&args[0]).is_formatter() {
        return run_rustfmt_cached(&wrapped_tool, &tool_args, &cwd);
    }

    // Check if this is an archiver or linker tool (including gcc -shared)
    if zccache_compiler::parse_archiver::is_archiver(&args[0])
        || zccache_compiler::parse_linker::is_link_invocation(&args[0], &tool_args)
    {
        return run_async(cmd_link_ephemeral(
            &endpoint,
            &wrapped_tool,
            tool_args,
            cwd.into(),
            client_env,
        ));
    }

    if let Err(err) = zccache_compiler::strict_paths::validate_args(&tool_args, strict_paths_mode) {
        eprintln!("{}", err.diagnostic(&args[0], &tool_args));
        return ExitCode::FAILURE;
    }

    // Otherwise, treat as a compiler invocation
    match std::env::var("ZCCACHE_SESSION_ID") {
        Ok(session_id) => {
            if session_id.is_empty() {
                eprintln!("ZCCACHE_SESSION_ID is empty");
                return ExitCode::FAILURE;
            }
            run_async(cmd_compile(
                &endpoint,
                &session_id,
                tool_args,
                cwd.into(),
                wrapped_tool,
                client_env,
            ))
        }
        Err(_) => {
            // No session — auto-create an ephemeral one for this compilation.
            run_async(cmd_compile_ephemeral(
                &endpoint,
                &wrapped_tool,
                tool_args,
                cwd.into(),
                client_env,
            ))
        }
    }
}

/// Resolve a compiler name/path to an absolute path.
/// Normalizes MSYS paths on Windows, then searches PATH if not already absolute.
fn resolve_compiler_path(compiler: &str) -> NormalizedPath {
    let normalized = zccache_core::path::normalize_msys_path(compiler);
    let path = Path::new(&normalized);

    // Already absolute — return as-is.
    if path.is_absolute() {
        return normalized.into();
    }

    // Search PATH for the compiler.
    match which_on_path(&normalized) {
        Some(abs) => abs,
        None => normalized.into(), // Let the daemon report the error.
    }
}

async fn cmd_compile(
    endpoint: &str,
    session_id: &str,
    args: Vec<String>,
    cwd: NormalizedPath,
    compiler: NormalizedPath,
    client_env: Vec<(String, String)>,
) -> ExitCode {
    let mut conn = match connect(endpoint).await {
        Ok(c) => c,
        Err(e) => {
            eprintln!("cannot connect to daemon at {endpoint}: {e}");
            return ExitCode::FAILURE;
        }
    };

    if let Err(e) = conn
        .send(&zccache_protocol::Request::Compile {
            session_id: session_id.to_string(),
            args,
            cwd,
            compiler,
            env: Some(client_env),
        })
        .await
    {
        eprintln!("zccache: failed to send to daemon: {e}");
        return ExitCode::FAILURE;
    }

    let recv_result = match conn.recv().await {
        Ok(r) => r,
        Err(e) => {
            eprintln!("zccache: broken connection to daemon: {e}");
            return ExitCode::FAILURE;
        }
    };
    match recv_result {
        Some(zccache_protocol::Response::CompileResult {
            exit_code,
            stdout,
            stderr,
            ..
        }) => {
            // Relay compiler output
            use std::io::Write;
            let _ = std::io::stdout().write_all(&stdout);
            let _ = std::io::stderr().write_all(&stderr);
            exit_code_from_i32(exit_code)
        }
        Some(zccache_protocol::Response::Error { message }) => {
            eprintln!("zccache error: {message}");
            ExitCode::FAILURE
        }
        None => {
            eprintln!("zccache: lost connection to daemon (no response received)");
            ExitCode::FAILURE
        }
        Some(other) => {
            eprintln!("zccache: unexpected response from daemon: {other:?}");
            ExitCode::FAILURE
        }
    }
}

/// Ephemeral session: single-roundtrip compile (session start + compile + session end
/// in one IPC message). Used when ZCCACHE_SESSION_ID is not set (drop-in mode).
async fn cmd_compile_ephemeral(
    endpoint: &str,
    compiler: &Path,
    args: Vec<String>,
    cwd: NormalizedPath,
    client_env: Vec<(String, String)>,
) -> ExitCode {
    // Ensure daemon is running and version-compatible.
    if let Err(e) = ensure_daemon(endpoint).await {
        eprintln!("cannot start daemon at {endpoint}: {e}");
        return ExitCode::FAILURE;
    }
    let mut conn = match connect(endpoint).await {
        Ok(c) => c,
        Err(e) => {
            eprintln!("cannot connect to daemon at {endpoint}: {e}");
            return ExitCode::FAILURE;
        }
    };

    if let Err(e) = conn
        .send(&zccache_protocol::Request::CompileEphemeral {
            client_pid: std::process::id(),
            working_dir: cwd.clone(),
            compiler: compiler.into(),
            args,
            cwd,
            env: Some(client_env),
        })
        .await
    {
        eprintln!("zccache: failed to send to daemon: {e}");
        return ExitCode::FAILURE;
    }

    let recv_result = match conn.recv().await {
        Ok(r) => r,
        Err(e) => {
            eprintln!("zccache: broken connection to daemon: {e}");
            return ExitCode::FAILURE;
        }
    };
    match recv_result {
        Some(zccache_protocol::Response::CompileResult {
            exit_code,
            stdout,
            stderr,
            ..
        }) => {
            use std::io::Write;
            let _ = std::io::stdout().write_all(&stdout);
            let _ = std::io::stderr().write_all(&stderr);
            exit_code_from_i32(exit_code)
        }
        Some(zccache_protocol::Response::Error { message }) => {
            eprintln!("zccache error: {message}");
            ExitCode::FAILURE
        }
        None => {
            eprintln!("zccache: lost connection to daemon (no response received)");
            ExitCode::FAILURE
        }
        Some(other) => {
            eprintln!("zccache: unexpected response from daemon: {other:?}");
            ExitCode::FAILURE
        }
    }
}

/// Ephemeral link/archive: single-roundtrip for `zccache ar ...` etc.
async fn cmd_link_ephemeral(
    endpoint: &str,
    tool: &Path,
    args: Vec<String>,
    cwd: NormalizedPath,
    client_env: Vec<(String, String)>,
) -> ExitCode {
    if let Err(e) = ensure_daemon(endpoint).await {
        eprintln!("cannot start daemon at {endpoint}: {e}");
        return ExitCode::FAILURE;
    }
    let mut conn = match connect(endpoint).await {
        Ok(c) => c,
        Err(e) => {
            eprintln!("cannot connect to daemon at {endpoint}: {e}");
            return ExitCode::FAILURE;
        }
    };

    if let Err(e) = conn
        .send(&zccache_protocol::Request::LinkEphemeral {
            client_pid: std::process::id(),
            tool: tool.into(),
            args,
            cwd,
            env: Some(client_env),
        })
        .await
    {
        eprintln!("zccache: failed to send to daemon: {e}");
        return ExitCode::FAILURE;
    }

    let recv_result = match conn.recv().await {
        Ok(r) => r,
        Err(e) => {
            eprintln!("zccache: broken connection to daemon: {e}");
            return ExitCode::FAILURE;
        }
    };
    match recv_result {
        Some(zccache_protocol::Response::LinkResult {
            exit_code,
            stdout,
            stderr,
            warning,
            ..
        }) => {
            use std::io::Write;
            let _ = std::io::stdout().write_all(&stdout);
            let _ = std::io::stderr().write_all(&stderr);
            if let Some(w) = warning {
                eprintln!("zccache warning: {w}");
            }
            exit_code_from_i32(exit_code)
        }
        Some(zccache_protocol::Response::Error { message }) => {
            eprintln!("zccache error: {message}");
            ExitCode::FAILURE
        }
        None => {
            eprintln!("zccache: lost connection to daemon (no response received)");
            ExitCode::FAILURE
        }
        Some(other) => {
            eprintln!("zccache: unexpected response from daemon: {other:?}");
            ExitCode::FAILURE
        }
    }
}

// ─── Daemon auto-start ─────────────────────────────────────────────────────

enum VersionCheck {
    Ok,
    /// Daemon is newer than client — safe to proceed.
    DaemonNewer {
        daemon_ver: String,
    },
    /// Daemon is older than client — must restart.
    DaemonOlder {
        daemon_ver: String,
    },
    /// Could not connect to the daemon at all.
    Unreachable,
    /// Connected but could not complete the version exchange (protocol mismatch, etc.).
    CommError,
}

/// Connect to the daemon and compare its version to ours.
async fn check_daemon_version(endpoint: &str) -> VersionCheck {
    let mut conn = match connect(endpoint).await {
        Ok(c) => c,
        Err(_) => return VersionCheck::Unreachable,
    };
    if conn.send(&zccache_protocol::Request::Status).await.is_err() {
        return VersionCheck::CommError;
    }
    match conn.recv::<zccache_protocol::Response>().await {
        Ok(Some(zccache_protocol::Response::Status(s))) => {
            if s.version == zccache_core::VERSION {
                return VersionCheck::Ok;
            }
            let client_ver = zccache_core::version::current();
            match zccache_core::version::Version::parse(&s.version) {
                Some(daemon_ver) => match daemon_ver.cmp(&client_ver) {
                    std::cmp::Ordering::Equal => VersionCheck::Ok,
                    std::cmp::Ordering::Greater => VersionCheck::DaemonNewer {
                        daemon_ver: s.version,
                    },
                    std::cmp::Ordering::Less => VersionCheck::DaemonOlder {
                        daemon_ver: s.version,
                    },
                },
                // Unparseable daemon version → treat as older (safe default)
                None => VersionCheck::DaemonOlder {
                    daemon_ver: s.version,
                },
            }
        }
        _ => VersionCheck::CommError,
    }
}

/// Spawn a new daemon and wait for it to become ready.
async fn spawn_and_wait(endpoint: &str) -> Result<(), String> {
    let daemon_bin = find_daemon_binary().ok_or("cannot find zccache-daemon binary")?;
    tracing::debug!(?daemon_bin, %endpoint, "spawning daemon");
    spawn_daemon(&daemon_bin, endpoint)?;

    for _ in 0..100 {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        if connect(endpoint).await.is_ok() {
            return Ok(());
        }
    }
    Err("daemon started but not accepting connections after 10s".to_string())
}

/// Ensure the daemon is running **and version-compatible**.
///
/// Version checking is asymmetric: a newer daemon is accepted (it's
/// backward-compatible), but an older daemon triggers a hard error
/// telling the user to run `zccache stop` first.
///
/// Handles concurrent calls gracefully: when multiple processes race to start
/// the daemon, only one wins the bind. The losers detect this and connect to
/// the winning daemon instead of failing.
/// Stop a stale daemon that is unreachable or version-incompatible.
///
/// Attempts graceful shutdown via IPC first, then falls back to force-killing
/// the process via the lock file PID. Waits for the endpoint to be released.
async fn stop_stale_daemon(endpoint: &str) {
    // Try graceful shutdown via IPC
    if let Ok(mut conn) = connect(endpoint).await {
        let _ = conn.send(&zccache_protocol::Request::Shutdown).await;
        // Give it a moment to process the shutdown
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }

    // Force-kill via lock file PID if the daemon is still alive
    if let Some(pid) = zccache_ipc::check_running_daemon() {
        tracing::debug!(pid, "force-killing stale daemon process");
        if zccache_ipc::force_kill_process(pid).is_ok() {
            for _ in 0..50 {
                if !zccache_ipc::is_process_alive(pid) {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        }
        zccache_ipc::remove_lock_file();
    }

    // Wait briefly for the endpoint (named pipe / socket) to be fully released
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
}

async fn ensure_daemon(endpoint: &str) -> Result<(), String> {
    // Fast path: connect + version check
    match check_daemon_version(endpoint).await {
        VersionCheck::Ok => return Ok(()),
        VersionCheck::DaemonNewer { daemon_ver } => {
            tracing::debug!(
                daemon_ver,
                client_ver = zccache_core::VERSION,
                "daemon is newer than client, proceeding"
            );
            return Ok(());
        }
        VersionCheck::DaemonOlder { daemon_ver } => {
            tracing::info!(
                daemon_ver,
                client_ver = zccache_core::VERSION,
                "daemon is older than client, auto-recovering"
            );
            stop_stale_daemon(endpoint).await;
            return spawn_and_wait(endpoint).await;
        }
        VersionCheck::CommError => {
            tracing::info!("cannot communicate with daemon, auto-recovering");
            stop_stale_daemon(endpoint).await;
            return spawn_and_wait(endpoint).await;
        }
        VersionCheck::Unreachable => {
            // Fall through to lock-file check / spawn
        }
    }

    // Check lock file for a running daemon we just can't reach yet
    if let Some(pid) = zccache_ipc::check_running_daemon() {
        for _ in 0..20 {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            match check_daemon_version(endpoint).await {
                VersionCheck::Ok => return Ok(()),
                VersionCheck::DaemonNewer { daemon_ver } => {
                    tracing::debug!(
                        daemon_ver,
                        client_ver = zccache_core::VERSION,
                        "daemon is newer than client, proceeding"
                    );
                    return Ok(());
                }
                VersionCheck::DaemonOlder { daemon_ver } => {
                    tracing::info!(
                        daemon_ver,
                        client_ver = zccache_core::VERSION,
                        "daemon is older than client during startup, auto-recovering"
                    );
                    stop_stale_daemon(endpoint).await;
                    return spawn_and_wait(endpoint).await;
                }
                VersionCheck::CommError => {
                    tracing::info!(
                        "cannot communicate with daemon during startup, auto-recovering"
                    );
                    stop_stale_daemon(endpoint).await;
                    return spawn_and_wait(endpoint).await;
                }
                VersionCheck::Unreachable => continue,
            }
        }
        return Err(format!(
            "daemon process {pid} exists but not accepting connections"
        ));
    }

    // No daemon running — spawn one
    spawn_and_wait(endpoint).await
}

/// Find the daemon binary. Looks next to the CLI binary first, then on PATH.
fn find_daemon_binary() -> Option<NormalizedPath> {
    let name = if cfg!(windows) {
        "zccache-daemon.exe"
    } else {
        "zccache-daemon"
    };

    // Look next to the CLI binary
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let candidate = dir.join(name);
            if candidate.exists() {
                return Some(candidate.into());
            }
        }
    }

    // Fall back to PATH
    which_on_path(name)
}

/// Simple PATH lookup (no external crate needed).
/// On Windows, also tries appending `.exe` if the name has no extension.
fn which_on_path(name: &str) -> Option<NormalizedPath> {
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate.into());
        }
        // On Windows, try with .exe suffix
        #[cfg(windows)]
        if std::path::Path::new(name).extension().is_none() {
            let with_exe = dir.join(format!("{name}.exe"));
            if with_exe.is_file() {
                return Some(with_exe.into());
            }
        }
    }
    None
}

/// Spawn the daemon as a detached background process.
///
/// On Windows, we must prevent the daemon from inheriting pipe handles.
/// When the CLI is invoked via `subprocess.run(capture_output=True)` (e.g. from
/// Python/meson), the parent creates pipes for stdout/stderr. If the daemon
/// inherits these handles, the parent hangs forever waiting for pipe closure
/// because the daemon never exits.
fn spawn_daemon(bin: &std::path::Path, endpoint: &str) -> Result<(), String> {
    // Best-effort: GC stale copies, then spawn from a fresh copy in the
    // zccache global runtime-binaries dir so the install path stays
    // overwritable. See issue #134.
    zccache_cli::gc_runtime_binaries();
    let bin_owned: std::path::PathBuf;
    let spawn_bin: &std::path::Path = match zccache_cli::prepare_daemon_exe(bin) {
        Ok(p) => {
            bin_owned = p;
            &bin_owned
        }
        Err(_) => bin,
    };

    let mut cmd = std::process::Command::new(spawn_bin);
    cmd.args(["--foreground", "--endpoint", endpoint]);

    // Detach stdio so the daemon doesn't hold our console
    cmd.stdin(std::process::Stdio::null());
    cmd.stdout(std::process::Stdio::null());
    cmd.stderr(std::process::Stdio::null());

    apply_cli_spawn_lineage(&mut cmd);

    // Platform-specific: prevent console window on Windows and avoid
    // inheriting parent pipe handles (which cause subprocess hangs).
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);

        // Mark our stdout/stderr as non-inheritable before spawning the daemon.
        // This prevents the daemon from inheriting pipe handles that a grandparent
        // process (e.g. Python subprocess.run) may have created for capture.
        // Without this, the daemon keeps the pipe open indefinitely, causing the
        // grandparent to hang waiting for EOF on the pipe.
        disable_handle_inheritance();
    }

    cmd.spawn()
        .map_err(|e| format!("failed to spawn daemon: {e}"))?;

    // Re-enable inheritance for our own handles (in case we do further spawns)
    #[cfg(windows)]
    restore_handle_inheritance();

    Ok(())
}

/// Initialize spawn-lineage env vars on a command the CLI is about to spawn.
///
/// Mirrors the daemon-side propagation in `zccache_daemon::lineage` so process
/// attribution (orphan tracking, running-process scanners) sees a consistent
/// chain across CLI -> daemon -> compiler hops. The chain is initialized with
/// the CLI's PID, and the originator marker is set to `zccache-cli:<pid>`
/// unless an outer tool has already claimed it.
fn apply_cli_spawn_lineage(cmd: &mut std::process::Command) {
    const ENV_ORIGINATOR: &str = "RUNNING_PROCESS_ORIGINATOR";
    const ENV_LINEAGE: &str = "ZCCACHE_LINEAGE";
    const ENV_PARENT_PID: &str = "ZCCACHE_PARENT_PID";
    const ENV_CLIENT_PID: &str = "ZCCACHE_CLIENT_PID";

    let cli_pid = std::process::id();

    if std::env::var(ENV_ORIGINATOR).is_err() {
        cmd.env(ENV_ORIGINATOR, format!("zccache-cli:{cli_pid}"));
    }

    let chain = match std::env::var(ENV_LINEAGE) {
        Ok(existing)
            if existing
                .rsplit_once('>')
                .map_or(existing.as_str(), |(_, last)| last)
                != cli_pid.to_string() =>
        {
            format!("{existing}>{cli_pid}")
        }
        Ok(existing) => existing,
        Err(_) => cli_pid.to_string(),
    };
    cmd.env(ENV_LINEAGE, chain);
    cmd.env(ENV_PARENT_PID, cli_pid.to_string());
    cmd.env(ENV_CLIENT_PID, cli_pid.to_string());
}

/// On Windows, mark stdout/stderr handles as non-inheritable so that child
/// processes (the daemon) do not inherit pipe handles from grandparent processes.
#[cfg(windows)]
fn disable_handle_inheritance() {
    use std::os::windows::io::AsRawHandle;

    extern "system" {
        fn SetHandleInformation(handle: *mut std::ffi::c_void, mask: u32, flags: u32) -> i32;
    }
    const HANDLE_FLAG_INHERIT: u32 = 1;

    // Safety: we're calling a standard Win32 API with valid handle values.
    // The handles come from Rust's stdout/stderr which are always valid.
    unsafe {
        let stdout = std::io::stdout();
        let stderr = std::io::stderr();
        SetHandleInformation(stdout.as_raw_handle() as *mut _, HANDLE_FLAG_INHERIT, 0);
        SetHandleInformation(stderr.as_raw_handle() as *mut _, HANDLE_FLAG_INHERIT, 0);
    }
}

/// Restore stdout/stderr handles as inheritable (undo `disable_handle_inheritance`).
#[cfg(windows)]
fn restore_handle_inheritance() {
    use std::os::windows::io::AsRawHandle;

    extern "system" {
        fn SetHandleInformation(handle: *mut std::ffi::c_void, mask: u32, flags: u32) -> i32;
    }
    const HANDLE_FLAG_INHERIT: u32 = 1;

    unsafe {
        let stdout = std::io::stdout();
        let stderr = std::io::stderr();
        SetHandleInformation(
            stdout.as_raw_handle() as *mut _,
            HANDLE_FLAG_INHERIT,
            HANDLE_FLAG_INHERIT,
        );
        SetHandleInformation(
            stderr.as_raw_handle() as *mut _,
            HANDLE_FLAG_INHERIT,
            HANDLE_FLAG_INHERIT,
        );
    }
}

// ─── Helpers ───────────────────────────────────────────────────────────────

/// Platform-correct connect (returns different types on Unix vs Windows).
#[cfg(unix)]
async fn connect(endpoint: &str) -> Result<zccache_ipc::IpcConnection, zccache_ipc::IpcError> {
    zccache_ipc::connect(endpoint).await
}

#[cfg(windows)]
async fn connect(
    endpoint: &str,
) -> Result<zccache_ipc::IpcClientConnection, zccache_ipc::IpcError> {
    zccache_ipc::connect(endpoint).await
}

fn resolve_endpoint(explicit: Option<&str>) -> String {
    if let Some(ep) = explicit {
        return ep.to_string();
    }
    if let Ok(ep) = std::env::var("ZCCACHE_ENDPOINT") {
        return ep;
    }
    zccache_ipc::default_endpoint()
}

fn run_async(future: impl std::future::Future<Output = ExitCode>) -> ExitCode {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("failed to create tokio runtime")
        .block_on(future)
}

fn format_uptime(secs: u64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m {}s", secs / 60, secs % 60)
    } else {
        let h = secs / 3600;
        let m = (secs % 3600) / 60;
        format!("{h}h {m}m")
    }
}

fn format_duration_ms(ms: u64) -> String {
    if ms < 1000 {
        format!("{ms}ms")
    } else if ms < 60_000 {
        format!("{:.1}s", ms as f64 / 1000.0)
    } else {
        let secs = ms / 1000;
        format!("{}m {}s", secs / 60, secs % 60)
    }
}

fn format_bytes(bytes: u64) -> String {
    if bytes == 0 {
        "0 B".to_string()
    } else if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else if bytes < 1024 * 1024 * 1024 {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    } else {
        format!("{:.1} GB", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
    }
}

fn init_tracing() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .init();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_code_zero_stays_zero() {
        assert_eq!(exit_code_from_i32(0), ExitCode::from(0));
    }

    #[test]
    fn exit_code_one_stays_one() {
        assert_eq!(exit_code_from_i32(1), ExitCode::from(1));
    }

    #[test]
    fn exit_code_255_stays_255() {
        assert_eq!(exit_code_from_i32(255), ExitCode::from(255));
    }

    #[test]
    fn exit_code_256_becomes_one_not_zero() {
        // Without the fix, 256 as u8 == 0, masking the failure.
        assert_ne!(exit_code_from_i32(256), ExitCode::from(0));
        assert_eq!(exit_code_from_i32(256), ExitCode::from(1));
    }

    #[test]
    fn exit_code_512_becomes_one_not_zero() {
        assert_eq!(exit_code_from_i32(512), ExitCode::from(1));
    }

    #[test]
    fn exit_code_negative_preserves_failure() {
        // -1 & 0xFF == 255
        assert_ne!(exit_code_from_i32(-1), ExitCode::from(0));
        assert_eq!(exit_code_from_i32(-1), ExitCode::from(255));
    }

    #[test]
    fn exit_code_257_keeps_low_byte() {
        // 257 & 0xFF == 1, non-zero, so kept as-is.
        assert_eq!(exit_code_from_i32(257), ExitCode::from(1));
    }

    #[test]
    fn rust_plan_cli_parses_validate_restore_save() {
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
        let stats = zccache_protocol::SessionStats {
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
        let cli = Cli::try_parse_from(["zccache", "session-end", "session-123", "--json"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Commands::SessionEnd { json: true, .. })
        ));
    }

    #[test]
    fn session_stats_accepts_json_flag() {
        let cli =
            Cli::try_parse_from(["zccache", "session-stats", "session-123", "--json"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Commands::SessionStatsCmd { json: true, .. })
        ));
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

    #[test]
    fn warm_restores_rust_artifacts_to_correct_paths() {
        let dir = tempfile::tempdir().unwrap();
        let cache_dir = dir.path().join("cache");
        let artifact_dir = cache_dir.join("artifacts");
        let index_path = cache_dir.join("index.bin");
        let target_dir = dir.path().join("target");

        std::fs::create_dir_all(&artifact_dir).unwrap();

        // Create a fake artifact store with two Rust crates
        let store = zccache_artifact::ArtifactStore::open(&index_path).unwrap();

        // Artifact 1: libserde-abc123.rlib + libserde-abc123.rmeta + serde-abc123.d
        let key1 = "aaaaaaaabbbbbbbb";
        let idx1 = zccache_artifact::ArtifactIndex::new(
            vec![
                "libserde-abc123.rlib".to_string(),
                "libserde-abc123.rmeta".to_string(),
                "serde-abc123.d".to_string(),
            ],
            vec![100, 50, 10],
            vec![],
            vec![],
            0,
        );
        store.insert(key1, &idx1);
        // Write payload files on disk
        std::fs::write(artifact_dir.join(format!("{key1}_0")), b"rlib-content").unwrap();
        std::fs::write(artifact_dir.join(format!("{key1}_1")), b"rmeta-content").unwrap();
        std::fs::write(artifact_dir.join(format!("{key1}_2")), b"dep-info").unwrap();

        // Artifact 2: libproc_macro2-def456.rlib
        let key2 = "ccccccccdddddddd";
        let idx2 = zccache_artifact::ArtifactIndex::new(
            vec!["libproc_macro2-def456.rlib".to_string()],
            vec![200],
            vec![],
            vec![],
            0,
        );
        store.insert(key2, &idx2);
        std::fs::write(artifact_dir.join(format!("{key2}_0")), b"proc-macro2-rlib").unwrap();

        // Artifact 3: NOT Rust (C++ object file) — should be filtered out
        let key3 = "eeeeeeeeffffffff";
        let idx3 = zccache_artifact::ArtifactIndex::new(
            vec!["foo.o".to_string()],
            vec![300],
            vec![],
            vec![],
            0,
        );
        store.insert(key3, &idx3);
        std::fs::write(artifact_dir.join(format!("{key3}_0")), b"object-file").unwrap();

        store.flush().unwrap();
        store.flush().unwrap();
        drop(store);

        // Run warm
        let (restored, skipped, errors) =
            warm_target(&index_path, &artifact_dir, &target_dir, "debug", None).unwrap();

        // Verify counts
        assert_eq!(errors, 0, "should have 0 errors");
        assert_eq!(
            restored, 5,
            "should restore all 5 files (3 serde + 1 proc_macro2 + 1 C++ .o)"
        );
        assert_eq!(skipped, 0, "all payloads exist on disk");

        // Verify files exist at correct paths
        let deps = target_dir.join("debug").join("deps");
        assert!(
            deps.join("libserde-abc123.rlib").exists(),
            "serde rlib missing"
        );
        assert!(
            deps.join("libserde-abc123.rmeta").exists(),
            "serde rmeta missing"
        );
        assert!(
            deps.join("serde-abc123.d").exists(),
            "serde dep-info missing"
        );
        assert!(
            deps.join("libproc_macro2-def456.rlib").exists(),
            "proc_macro2 rlib missing"
        );

        // Verify content is correct
        assert_eq!(
            std::fs::read(deps.join("libserde-abc123.rlib")).unwrap(),
            b"rlib-content"
        );
        assert_eq!(
            std::fs::read(deps.join("libproc_macro2-def456.rlib")).unwrap(),
            b"proc-macro2-rlib"
        );

        // Verify C++ artifact IS restored (warm restores everything, not just Rust)
        assert!(
            deps.join("foo.o").exists(),
            "C++ .o file should also be in deps/"
        );
        assert_eq!(std::fs::read(deps.join("foo.o")).unwrap(), b"object-file");

        // Verify mtime is recent (within 5 seconds)
        let meta = std::fs::metadata(deps.join("libserde-abc123.rlib")).unwrap();
        let age = meta.modified().unwrap().elapsed().unwrap();
        assert!(age.as_secs() < 5, "mtime should be fresh, got {age:?}");
    }

    #[test]
    fn warm_skips_missing_payloads() {
        let dir = tempfile::tempdir().unwrap();
        let cache_dir = dir.path().join("cache");
        let artifact_dir = cache_dir.join("artifacts");
        let index_path = cache_dir.join("index.bin");
        let target_dir = dir.path().join("target");

        std::fs::create_dir_all(&artifact_dir).unwrap();

        let store = zccache_artifact::ArtifactStore::open(&index_path).unwrap();
        let key = "1111111122222222";
        let idx = zccache_artifact::ArtifactIndex::new(
            vec!["libfoo-xyz.rlib".to_string()],
            vec![100],
            vec![],
            vec![],
            0,
        );
        store.insert(key, &idx);
        // DON'T write the payload file — simulate missing artifact on disk
        store.flush().unwrap();
        drop(store);

        let (restored, skipped, errors) =
            warm_target(&index_path, &artifact_dir, &target_dir, "debug", None).unwrap();

        assert_eq!(restored, 0);
        assert_eq!(skipped, 1, "should skip 1 missing payload");
        assert_eq!(errors, 0);
    }

    #[test]
    fn warm_returns_error_on_missing_index() {
        let dir = tempfile::tempdir().unwrap();
        let result = warm_target(
            &dir.path().join("nonexistent.redb"),
            &dir.path().join("artifacts"),
            &dir.path().join("target"),
            "debug",
            None,
        );
        assert!(result.is_err());
    }

    // ── Helper: create a fake artifact store with test data ──────

    fn make_test_store(dir: &Path) -> (PathBuf, PathBuf) {
        let cache_dir = dir.join("cache");
        let artifact_dir = cache_dir.join("artifacts");
        let index_path = cache_dir.join("index.bin");
        std::fs::create_dir_all(&artifact_dir).unwrap();

        let store = zccache_artifact::ArtifactStore::open(&index_path).unwrap();

        // serde (in a typical Cargo.lock)
        let k1 = "aaaa0001";
        store.insert(
            k1,
            &zccache_artifact::ArtifactIndex::new(
                vec![
                    "libserde-abc123.rlib".into(),
                    "libserde-abc123.rmeta".into(),
                    "serde-abc123.d".into(),
                ],
                vec![100, 50, 10],
                vec![],
                vec![],
                0,
            ),
        );
        std::fs::write(artifact_dir.join(format!("{k1}_0")), b"serde-rlib").unwrap();
        std::fs::write(artifact_dir.join(format!("{k1}_1")), b"serde-rmeta").unwrap();
        std::fs::write(artifact_dir.join(format!("{k1}_2")), b"serde-d").unwrap();

        // proc-macro2 (hyphen → underscore in filename)
        let k2 = "aaaa0002";
        store.insert(
            k2,
            &zccache_artifact::ArtifactIndex::new(
                vec!["libproc_macro2-def456.rlib".into()],
                vec![200],
                vec![],
                vec![],
                0,
            ),
        );
        std::fs::write(artifact_dir.join(format!("{k2}_0")), b"proc-macro2-rlib").unwrap();

        // tokio (NOT in our test lockfile)
        let k3 = "aaaa0003";
        store.insert(
            k3,
            &zccache_artifact::ArtifactIndex::new(
                vec!["libtokio-ghi789.rlib".into()],
                vec![300],
                vec![],
                vec![],
                0,
            ),
        );
        std::fs::write(artifact_dir.join(format!("{k3}_0")), b"tokio-rlib").unwrap();

        // C++ object file (no crate name pattern)
        let k4 = "aaaa0004";
        store.insert(
            k4,
            &zccache_artifact::ArtifactIndex::new(
                vec!["foo.o".into()],
                vec![50],
                vec![],
                vec![],
                0,
            ),
        );
        std::fs::write(artifact_dir.join(format!("{k4}_0")), b"cpp-object").unwrap();

        store.flush().unwrap();
        drop(store);
        (index_path, artifact_dir)
    }

    fn write_lockfile(dir: &Path, crates: &[&str]) -> PathBuf {
        let lockfile = dir.join("Cargo.lock");
        let mut content = String::from("# This file is automatically @generated\nversion = 3\n\n");
        for name in crates {
            content.push_str(&format!(
                "[[package]]\nname = \"{name}\"\nversion = \"1.0.0\"\n\n"
            ));
        }
        std::fs::write(&lockfile, &content).unwrap();
        lockfile
    }

    // ── Lockfile parsing tests ───────────────────────────────────

    #[test]
    fn parse_lockfile_extracts_crate_names() {
        let dir = tempfile::tempdir().unwrap();
        let lf = write_lockfile(dir.path(), &["serde", "proc-macro2", "unicode-ident"]);
        let crates = parse_lockfile_crates(&lf).unwrap();
        assert!(crates.contains("serde"));
        assert!(
            crates.contains("proc_macro2"),
            "hyphens should be underscores"
        );
        assert!(crates.contains("unicode_ident"));
        assert!(!crates.contains("tokio"), "tokio not in lockfile");
    }

    #[test]
    fn artifact_matches_lockfile_basic() {
        let mut allowed = std::collections::HashSet::new();
        allowed.insert("serde".to_string());
        allowed.insert("proc_macro2".to_string());

        assert!(artifact_matches_lockfile("libserde-abc123.rlib", &allowed));
        assert!(artifact_matches_lockfile("libserde-abc123.rmeta", &allowed));
        assert!(artifact_matches_lockfile("serde-abc123.d", &allowed));
        assert!(artifact_matches_lockfile(
            "libproc_macro2-def456.rlib",
            &allowed
        ));
        assert!(!artifact_matches_lockfile("libtokio-ghi789.rlib", &allowed));
        // No hash separator → allowed (could be build script output)
        assert!(artifact_matches_lockfile("build_script_build", &allowed));
    }

    // ── Strategy tests ───────────────────────────────────────────

    #[test]
    fn warm_without_lockfile_restores_everything() {
        let dir = tempfile::tempdir().unwrap();
        let (index_path, artifact_dir) = make_test_store(dir.path());
        let target_dir = dir.path().join("target");

        let (restored, _, _) =
            warm_target(&index_path, &artifact_dir, &target_dir, "debug", None).unwrap();

        let deps = target_dir.join("debug").join("deps");
        assert_eq!(restored, 6, "without lockfile: restore all 6 files");
        assert!(deps.join("libserde-abc123.rlib").exists());
        assert!(
            deps.join("libtokio-ghi789.rlib").exists(),
            "tokio restored without filter"
        );
        assert!(
            deps.join("foo.o").exists(),
            "C++ file restored without filter"
        );
    }

    #[test]
    fn warm_with_lockfile_filters_to_matching_crates() {
        let dir = tempfile::tempdir().unwrap();
        let (index_path, artifact_dir) = make_test_store(dir.path());
        let target_dir = dir.path().join("target");
        let lockfile = write_lockfile(dir.path(), &["serde", "proc-macro2"]);

        let (restored, skipped, _) = warm_target(
            &index_path,
            &artifact_dir,
            &target_dir,
            "debug",
            Some(&lockfile),
        )
        .unwrap();

        let deps = target_dir.join("debug").join("deps");
        // serde (3) + proc-macro2 (1) + foo.o (1, no hash separator = allowed)
        assert_eq!(restored, 5);
        assert!(deps.join("libserde-abc123.rlib").exists());
        assert!(deps.join("libproc_macro2-def456.rlib").exists());
        assert!(
            !deps.join("libtokio-ghi789.rlib").exists(),
            "tokio NOT in lockfile"
        );
        assert!(
            deps.join("foo.o").exists(),
            "no hash separator = allowed through"
        );
        assert!(skipped > 0, "tokio should be skipped");
    }

    // ── Adversarial tests ────────────────────────────────────────

    #[test]
    fn adversarial_crate_removed_from_lockfile() {
        // Scenario: tokio was in the cache from a previous build,
        // but was removed from Cargo.toml/Cargo.lock.
        // Warm should NOT restore it.
        let dir = tempfile::tempdir().unwrap();
        let (index_path, artifact_dir) = make_test_store(dir.path());
        let target_dir = dir.path().join("target");
        // Lockfile has serde but NOT tokio
        let lockfile = write_lockfile(dir.path(), &["serde"]);

        let (restored, _, _) = warm_target(
            &index_path,
            &artifact_dir,
            &target_dir,
            "debug",
            Some(&lockfile),
        )
        .unwrap();

        let deps = target_dir.join("debug").join("deps");
        assert!(deps.join("libserde-abc123.rlib").exists());
        assert!(
            !deps.join("libtokio-ghi789.rlib").exists(),
            "removed crate must NOT be restored"
        );
        // serde (3) + foo.o (1, no hash separator = allowed)
        assert_eq!(restored, 4);
    }

    #[test]
    fn adversarial_stale_file_in_target_from_previous_warm() {
        // Scenario: previous warm restored tokio. Then tokio was removed
        // from Cargo.lock. New warm runs — does it leave the stale file?
        // Answer: yes, warm doesn't delete. But cargo ignores unknown files.
        let dir = tempfile::tempdir().unwrap();
        let (index_path, artifact_dir) = make_test_store(dir.path());
        let target_dir = dir.path().join("target");
        let deps = target_dir.join("debug").join("deps");
        std::fs::create_dir_all(&deps).unwrap();

        // Simulate stale file from previous warm
        std::fs::write(deps.join("libtokio-ghi789.rlib"), b"stale").unwrap();

        // Now warm with lockfile that excludes tokio
        let lockfile = write_lockfile(dir.path(), &["serde"]);
        warm_target(
            &index_path,
            &artifact_dir,
            &target_dir,
            "debug",
            Some(&lockfile),
        )
        .unwrap();

        // Stale file still there (warm doesn't delete)
        assert!(
            deps.join("libtokio-ghi789.rlib").exists(),
            "warm doesn't clean up stale files — cargo ignores them"
        );
        // But it wasn't overwritten with fresh content
        assert_eq!(
            std::fs::read(deps.join("libtokio-ghi789.rlib")).unwrap(),
            b"stale",
            "stale file content unchanged"
        );
    }

    #[test]
    fn adversarial_version_bump_old_artifact_in_cache() {
        // Scenario: cache has serde 1.0.227 artifacts, but Cargo.lock
        // now requires serde 1.0.228. The old artifacts have different
        // hashes in the filename so they won't conflict.
        let dir = tempfile::tempdir().unwrap();
        let cache_dir = dir.path().join("cache");
        let artifact_dir = cache_dir.join("artifacts");
        let index_path = cache_dir.join("index.bin");
        std::fs::create_dir_all(&artifact_dir).unwrap();

        let store = zccache_artifact::ArtifactStore::open(&index_path).unwrap();

        // Old version's artifact (different hash suffix)
        let k_old = "bbbb0001";
        store.insert(
            k_old,
            &zccache_artifact::ArtifactIndex::new(
                vec!["libserde-old111.rlib".into()],
                vec![100],
                vec![],
                vec![],
                0,
            ),
        );
        std::fs::write(artifact_dir.join(format!("{k_old}_0")), b"old-serde").unwrap();

        // New version's artifact (different hash suffix)
        let k_new = "bbbb0002";
        store.insert(
            k_new,
            &zccache_artifact::ArtifactIndex::new(
                vec!["libserde-new222.rlib".into()],
                vec![100],
                vec![],
                vec![],
                0,
            ),
        );
        std::fs::write(artifact_dir.join(format!("{k_new}_0")), b"new-serde").unwrap();

        store.flush().unwrap();
        drop(store);

        let target_dir = dir.path().join("target");
        let lockfile = write_lockfile(dir.path(), &["serde"]);

        let (restored, _, _) = warm_target(
            &index_path,
            &artifact_dir,
            &target_dir,
            "debug",
            Some(&lockfile),
        )
        .unwrap();

        let deps = target_dir.join("debug").join("deps");
        // Both old and new are restored — cargo will use the one matching
        // its own fingerprint and ignore the other
        assert_eq!(restored, 2);
        assert!(deps.join("libserde-old111.rlib").exists());
        assert!(deps.join("libserde-new222.rlib").exists());
        // This is safe: cargo only links the artifact matching its
        // fingerprint hash. The extra file wastes ~100 bytes of disk.
    }

    #[test]
    fn adversarial_corrupted_cache_file() {
        // Scenario: artifact payload on disk is corrupted (truncated).
        // Warm restores it, cargo tries to use it, gets an error,
        // and recompiles from scratch. Verify warm doesn't crash.
        let dir = tempfile::tempdir().unwrap();
        let cache_dir = dir.path().join("cache");
        let artifact_dir = cache_dir.join("artifacts");
        let index_path = cache_dir.join("index.bin");
        std::fs::create_dir_all(&artifact_dir).unwrap();

        let store = zccache_artifact::ArtifactStore::open(&index_path).unwrap();
        let key = "cccc0001";
        store.insert(
            key,
            &zccache_artifact::ArtifactIndex::new(
                vec!["libserde-abc123.rlib".into()],
                vec![1000], // Claims 1000 bytes
                vec![],
                vec![],
                0,
            ),
        );
        // But payload is only 5 bytes (corrupted/truncated)
        std::fs::write(artifact_dir.join(format!("{key}_0")), b"short").unwrap();
        store.flush().unwrap();
        drop(store);

        let target_dir = dir.path().join("target");
        let (restored, _, errors) =
            warm_target(&index_path, &artifact_dir, &target_dir, "debug", None).unwrap();

        // Warm restores it without error (it doesn't validate content)
        assert_eq!(restored, 1);
        assert_eq!(errors, 0);
        // Cargo will detect the corruption via its own hash check and rebuild
        let deps = target_dir.join("debug").join("deps");
        assert_eq!(
            std::fs::read(deps.join("libserde-abc123.rlib")).unwrap(),
            b"short"
        );
    }

    #[test]
    fn adversarial_empty_lockfile() {
        // Edge case: Cargo.lock exists but has no packages
        let dir = tempfile::tempdir().unwrap();
        let (index_path, artifact_dir) = make_test_store(dir.path());
        let target_dir = dir.path().join("target");
        let lockfile = write_lockfile(dir.path(), &[]);

        let (restored, skipped, _) = warm_target(
            &index_path,
            &artifact_dir,
            &target_dir,
            "debug",
            Some(&lockfile),
        )
        .unwrap();

        // foo.o has no hash separator → allowed through. Everything else skipped.
        assert_eq!(restored, 1, "only foo.o (no hash separator) passes");
        assert!(skipped > 0);
    }

    // ── Protocol mismatch recovery (issue #27) ──────────────────

    /// Regression test for <https://github.com/zackees/zccache/issues/27>.
    ///
    /// When a stale daemon is running but can't communicate (protocol mismatch
    /// or corrupt pipe), `ensure_daemon` should auto-recover instead of telling
    /// the user to manually run `zccache stop`.
    ///
    /// This test creates a fake "stale daemon" — an IPC listener that accepts
    /// connections and immediately drops them, causing `check_daemon_version`
    /// to return `CommError`. We then verify that `ensure_daemon` does NOT
    /// return the "Run `zccache stop` first" error.
    #[tokio::test]
    #[ignore] // Integration test — needs daemon binary. Run with `test --full`.
    async fn ensure_daemon_auto_recovers_on_comm_error() {
        let endpoint = zccache_ipc::unique_test_endpoint();

        // Spawn a fake stale daemon: accepts one connection, drops it (CommError),
        // then shuts down so the endpoint is released for the real daemon.
        let ep = endpoint.clone();
        let mut listener = zccache_ipc::IpcListener::bind(&ep).unwrap();
        let server = tokio::spawn(async move {
            // Accept the connection from check_daemon_version, drop it immediately
            let _ = listener.accept().await;
            // Listener drops here, releasing the endpoint
        });

        // Give the listener time to be ready
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        let result = ensure_daemon(&endpoint).await;

        // Ensure server task has completed
        let _ = server.await;

        // The OLD behavior (bug): returns Err("...Run `zccache stop` first.")
        // The NEW behavior (fix): auto-recovers — either succeeds or fails
        // for a different reason (e.g., daemon binary not found).
        if let Err(msg) = &result {
            assert!(
                !msg.contains("zccache stop"),
                "Bug #27: ensure_daemon requires manual `zccache stop` instead of \
                 auto-recovering on protocol mismatch: {msg}"
            );
        }
    }

    /// The bounded wait loop must return promptly when the IPC endpoint is
    /// already unreachable (typical CI shape after a clean stop).
    #[test]
    fn wait_for_daemon_teardown_returns_when_endpoint_unreachable() {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::env::set_var("ZCCACHE_STOP_TIMEOUT_SECS", "2");

        let unreachable_endpoint = if cfg!(windows) {
            r"\\.\pipe\zccache-test-does-not-exist-182".to_string()
        } else {
            tmp.path()
                .join("does-not-exist.sock")
                .to_string_lossy()
                .into_owned()
        };

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let started = std::time::Instant::now();
        rt.block_on(wait_for_daemon_teardown(&unreachable_endpoint));
        let elapsed = started.elapsed();
        std::env::remove_var("ZCCACHE_STOP_TIMEOUT_SECS");

        assert!(
            elapsed < std::time::Duration::from_secs(2),
            "wait_for_daemon_teardown blocked for {elapsed:?} despite endpoint unreachable at t=0"
        );
    }

    /// Exercises both branches of the setup-soldr-compatible bool grammar.
    /// Tests the pure function so we don't have to mutate process env vars
    /// — that's a documented foot-gun in cargo's parallel test runner.
    #[test]
    fn flag_truthy_matches_setup_soldr_normalization() {
        // Truthy variants
        for v in ["1", "true", "True", "TRUE", "yes", "YES", "on", "On"] {
            assert!(flag_truthy(Some(v)), "expected truthy: {v:?}");
        }
        // Whitespace tolerated
        assert!(flag_truthy(Some("  true  ")));

        // Falsy / "leave behavior unchanged" variants
        assert!(!flag_truthy(None));
        for v in [
            "", "0", "false", "False", "no", "off", "OFF", "garbage", "2",
        ] {
            assert!(!flag_truthy(Some(v)), "expected falsy: {v:?}");
        }
    }

    // ─── snapshot-bytes parallel walk (issue #189) ──────────────────────

    fn write_file(path: &Path, bytes: &[u8]) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("mkdir -p");
        }
        std::fs::write(path, bytes).expect("write file");
    }

    /// Empty / missing target dir returns 0 bytes (mirrors os.walk behavior).
    #[test]
    fn snapshot_bytes_missing_target_is_zero() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let missing = tmp.path().join("nope");
        assert_eq!(snapshot_bytes_walk(&missing, true, false).unwrap(), 0);
    }

    /// Sums regular files. `--prune-incremental` removes `incremental/`
    /// directories from the walk entirely.
    #[test]
    fn snapshot_bytes_prunes_incremental() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let target = tmp.path();
        write_file(&target.join("debug/deps/libfoo.rlib"), &[0u8; 100]);
        write_file(&target.join("debug/incremental/foo/state.bin"), &[0u8; 999]);
        write_file(
            &target.join("release/incremental/bar/state.bin"),
            &[0u8; 999],
        );

        let with_prune = snapshot_bytes_walk(target, true, false).unwrap();
        assert_eq!(with_prune, 100, "incremental should be excluded");

        let without_prune = snapshot_bytes_walk(target, false, false).unwrap();
        assert_eq!(
            without_prune,
            100 + 999 + 999,
            "without prune, all files counted"
        );
    }

    /// `--prune-build-script-out` removes `*/build/*/out/` only. A bare `out/`
    /// outside that pattern stays in the count.
    #[test]
    fn snapshot_bytes_prunes_build_script_out_only_under_build() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let target = tmp.path();
        write_file(
            &target.join("debug/build/libz-sys-abc/out/native/libz.a"),
            &[0u8; 500],
        );
        write_file(
            &target.join("debug/build/libz-sys-abc/build-script-build"),
            &[0u8; 50],
        );
        // `out/` that is NOT under `build/<pkg>/` should not be pruned.
        write_file(&target.join("debug/deps/some/out/data.bin"), &[0u8; 7]);

        let pruned = snapshot_bytes_walk(target, true, true).unwrap();
        assert_eq!(
            pruned,
            50 + 7,
            "only build/<pkg>/out should be pruned; deps/some/out kept"
        );

        let kept = snapshot_bytes_walk(target, true, false).unwrap();
        assert_eq!(kept, 500 + 50 + 7);
    }

    /// Walker tolerates an entirely empty tree — returns 0, doesn't error.
    #[test]
    fn snapshot_bytes_empty_target_is_zero() {
        let tmp = tempfile::tempdir().expect("tempdir");
        assert_eq!(snapshot_bytes_walk(tmp.path(), true, false).unwrap(), 0);
    }

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
}
